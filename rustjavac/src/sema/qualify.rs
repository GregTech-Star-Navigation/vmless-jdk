//! 包名解析：把**简单类型名**补成 `pkg/Name` 或 `import` 指到的全名。
//!
//! 编译器内部一律用 `p/C` 形式的内部名（描述符 `Lp/C;`、class 文件的 `this_class`、
//! 继承链查找都是它），所以这一步放在 AST 进 codegen **之前**做一次 ——
//! 后面所有 `descriptor` / `ver_type` / `internal_name` / 继承链解析都不用再关心包名。
//!
//! 简单名的解析顺序（与 javac 一致，去掉嵌套类型的 `$` 规则）：
//!
//! 1. 本编译单元自己声明的类型 → 当前包
//! 2. `import a.b.C;` 的单类型导入
//! 3. 当前包下 classpath 上有 `pkg/Name.class`
//! 4. `import a.b.*;` 的通配导入（`a/b/Name.class` 存在才算命中）
//!
//! java.lang 的简写（`String`）不参与这一套，保持原样由 `internal_name` 补全。

use crate::ast::*;
use crate::codegen::types::is_java_lang;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// 目录里的文件名集合（进程级缓存）。
fn dir_listing(dir: &Path) -> Option<Rc<HashSet<String>>> {
    thread_local! {
        static C: RefCell<HashMap<PathBuf, Option<Rc<HashSet<String>>>>> = RefCell::new(HashMap::new());
    }
    if let Some(hit) = C.with(|c| c.borrow().get(dir).cloned()) {
        return hit;
    }
    let v = std::fs::read_dir(dir).ok().map(|rd| {
        Rc::new(
            rd.flatten()
                .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
                .collect::<HashSet<String>>(),
        )
    });
    C.with(|c| c.borrow_mut().insert(dir.to_path_buf(), v.clone()));
    v
}

/// classpath 上有没有 `rel` 这个 `.class` —— 文件名按**大小写敏感**比较。
///
/// Windows 的文件系统不区分大小写，直接用 `is_file()` 会把表达式里的变量名当成类名：
/// `key` 命中 `java/security/Key.class`、`cipher` 命中 `javax/crypto/Cipher.class`。
/// java.base 上这就是「未定义的变量」那一大桶的根因。
fn class_file_exists(dirs: &[PathBuf], rel: &str) -> bool {
    let (parent, file) = match rel.rsplit_once('/') {
        Some((p, f)) => (p, f),
        None => ("", rel),
    };
    dirs.iter().any(|d| {
        let dir = if parent.is_empty() { d.clone() } else { d.join(parent) };
        match dir_listing(&dir) {
            Some(set) => set.contains(file),
            None => false,
        }
    })
}

struct Qualifier<'a> {
    pkg: String,
    /// 本编译单元声明的类型名（含嵌套类型）
    declared: HashSet<String>,
    /// 单类型导入：简单名 → 内部名前缀（`a/b`）
    singles: Vec<(String, String)>,
    /// 通配导入的包前缀（`a/b`）
    wildcards: Vec<String>,
    dirs: &'a [PathBuf],
}

impl Qualifier<'_> {
    /// 简单名 → 内部名前缀（不含类型名本身）。返回 None 表示按原样保留。
    fn package_of(&self, name: &str) -> Option<String> {
        if is_java_lang(name) {
            return None;
        }
        // 默认包里的类型没有前缀可加
        let same_pkg = || class_file_exists(self.dirs, &format!("{}/{}.class", self.pkg, name));
        let hit = if self.declared.contains(name) {
            Some(self.pkg.clone())
        } else if let Some((_, pfx)) = self.singles.iter().find(|(s, _)| s == name) {
            Some(pfx.clone())
        } else if same_pkg() {
            Some(self.pkg.clone())
        } else if class_file_exists(self.dirs, &format!("java/lang/{name}.class")) {
            // **隐式 `import java.lang.*;`**（JLS 7.3）：`Appendable` / `Readable` /
            // `CharSequence` 这类不在 `is_java_lang` 白名单里的 java.lang 类型，
            // 只要 classpath 上有就必须补成 `java/lang/X`。
            Some("java/lang".to_string())
        } else {
            self.wildcards
                .iter()
                .find(|pfx| class_file_exists(self.dirs, &format!("{}/{}.class", pfx, name)))
                .cloned()
        };
        hit.filter(|p| !p.is_empty())
    }

    /// 表达式里的类名引用（`Tool.twice(3)`、`Tool.count`）也要按 import 补全。
    ///
    /// 只认**导入**来的名字（单类型 / 通配）：同包的类型名本来就能解析，
    /// 不去动它以免和同名局部变量/静态字段混淆。
    fn imported_class(&self, name: &str) -> Option<String> {
        if is_java_lang(name) {
            return None;
        }
        if let Some((_, pfx)) = self.singles.iter().find(|(s, _)| s == name) {
            return Some(format!("{pfx}/{name}"));
        }
        self.wildcards
            .iter()
            .find(|pfx| class_file_exists(self.dirs, &format!("{}/{}.class", pfx, name)))
            .map(|pfx| format!("{pfx}/{name}"))
    }

    fn ty(&self, ty: &mut Type) {
        match ty {
            Type::Named { path, args } => {
                for a in args.iter_mut() {
                    self.ty(a);
                }
                self.resolve_path(path);
            }
            Type::Array(inner) => self.ty(inner),
            _ => {}
        }
    }

    /// 类型路径 → 内部名分段（就地改写 `path`）。
    ///
    /// 两件事：
    /// 1. 单段名补包（同包 / import / java.lang / 通配）；
    /// 2. **多段名的最后几段可能是嵌套类型**：`Map.Entry` 要变成 `java/util/Map$Entry`，
    ///    `SSLHandshake.HandshakeMessage` 是 `sun/security/ssl/SSLHandshake$HandshakeMessage`。
    ///    不补的话描述符里会出现 `LMap/Entry;` 这种不存在的类名 —— 编过了也是坏 class。
    fn resolve_path(&self, path: &mut Vec<String>) {
        if path.is_empty() {
            return;
        }
        // 整条按斜杠就是已知的类 → 完事（`java.util.List` 这种全限定的常见形态）
        if path.len() > 1 && class_file_exists(self.dirs, &format!("{}.class", path.join("/"))) {
            return;
        }
        // 找「最长的类型前缀」：前缀 = 包名 + 一个类名，其余段全部按嵌套类型拼 `$`
        for split in 1..path.len() {
            let head = if split == 1 {
                match self.package_of(&path[0]) {
                    Some(p) => format!("{p}/{}", path[0]),
                    None => path[0].clone(),
                }
            } else {
                path[..split].join("/")
            };
            let joined = format!("{head}${}", path[split..].join("$"));
            if class_file_exists(self.dirs, &format!("{joined}.class")) {
                let segs: Vec<String> = joined.split('/').map(|s| s.to_string()).collect();
                *path = segs;
                return;
            }
        }
        if path.len() == 1 {
            if let Some(pfx) = self.package_of(&path[0]) {
                path.insert(0, pfx);
            }
        }
    }

    fn member(&self, m: &mut Member) {
        match m {
            Member::Field(f) => {
                self.ty(&mut f.ty);
                if let Some(e) = &mut f.init {
                    self.expr(e);
                }
            }
            Member::Method(md) => {
                if let Some(r) = &mut md.ret {
                    self.ty(r);
                }
                for p in md.params.iter_mut() {
                    self.ty(&mut p.ty);
                }
                for t in md.throws.iter_mut() {
                    self.ty(t);
                }
                if let Some(b) = &mut md.body {
                    self.stmt(b);
                }
            }
            Member::Type(t) => self.type_decl(t),
            Member::Init { body, .. } => self.stmt(body),
            // 枚举常量（降级发生在 qualify 之前；这里兜住嵌套类型里漏下来的）
            Member::EnumConst { args, .. } => {
                for a in args.iter_mut() {
                    self.expr(a);
                }
            }
        }
    }

    fn type_decl(&self, t: &mut TypeDecl) {
        for ty in t.extends.iter_mut().chain(t.implements.iter_mut()) {
            self.ty(ty);
        }
        for m in t.members.iter_mut() {
            self.member(m);
        }
    }

    fn stmt(&self, s: &mut Stmt) {
        match s {
            Stmt::Block(items) => {
                for it in items.iter_mut() {
                    self.stmt(it);
                }
            }
            Stmt::Labeled { body, .. } => self.stmt(body),
            Stmt::LocalVar { ty, init, .. } => {
                self.ty(ty);
                if let Some(e) = init {
                    self.expr(e);
                }
            }
            Stmt::Expr(e) | Stmt::Throw(e) => self.expr(e),
            Stmt::If { cond, then, els } => {
                self.expr(cond);
                self.stmt(then);
                if let Some(e) = els {
                    self.stmt(e);
                }
            }
            Stmt::While { cond, body } => {
                self.expr(cond);
                self.stmt(body);
            }
            Stmt::DoWhile { body, cond } => {
                self.stmt(body);
                self.expr(cond);
            }
            Stmt::For { init, cond, update, body } => {
                for s in init.iter_mut() {
                    self.stmt(s);
                }
                if let Some(c) = cond {
                    self.expr(c);
                }
                for u in update.iter_mut() {
                    self.expr(u);
                }
                self.stmt(body);
            }
            Stmt::ForEach { ty, iter, body, .. } => {
                self.ty(ty);
                self.expr(iter);
                self.stmt(body);
            }
            Stmt::Return(e) => {
                if let Some(e) = e {
                    self.expr(e);
                }
            }
            Stmt::Switch { selector, cases } => {
                self.expr(selector);
                for c in cases.iter_mut() {
                    for l in c.labels.iter_mut().flatten() {
                        self.expr(l);
                    }
                    for s in c.body.iter_mut() {
                        self.stmt(s);
                    }
                }
            }
            Stmt::Sync { lock, body } => {
                self.expr(lock);
                self.stmt(body);
            }
            Stmt::Try { body, catches, finally } => {
                self.stmt(body);
                for c in catches.iter_mut() {
                    for t in c.types.iter_mut() {
                        self.ty(t);
                    }
                    self.stmt(&mut c.body);
                }
                if let Some(f) = finally {
                    self.stmt(f);
                }
            }
            Stmt::Yield(e) => self.expr(e),
            Stmt::Break(_) | Stmt::Continue(_) | Stmt::Empty => {}
        }
    }

    fn expr(&self, e: &mut Expr) {
        match e {
            Expr::New { ty, args } => {
                self.ty(ty);
                for a in args.iter_mut() {
                    self.expr(a);
                }
            }
            Expr::ArrayNew { elem, dims } => {
                self.ty(elem);
                for d in dims.iter_mut() {
                    self.expr(d);
                }
            }
            Expr::Cast { ty, expr } => {
                self.ty(ty);
                self.expr(expr);
            }
            Expr::InstanceOf { expr, ty, .. } => {
                self.ty(ty);
                self.expr(expr);
            }
            Expr::ClassLit(ty) => self.ty(ty),
            Expr::Field(r, _) => self.expr(r),
            Expr::Call { recv, args, type_args, .. } => {
                if let Some(r) = recv {
                    self.expr(r);
                }
                for a in args.iter_mut() {
                    self.expr(a);
                }
                for t in type_args.iter_mut() {
                    self.ty(t);
                }
            }
            Expr::ArrayInit(items) => {
                for i in items.iter_mut() {
                    self.expr(i);
                }
            }
            Expr::ArrayAccess { array, index } => {
                self.expr(array);
                self.expr(index);
            }
            Expr::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            Expr::Unary { expr, .. } => self.expr(expr),
            Expr::Assign { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            Expr::Cond { cond, then, els } => {
                self.expr(cond);
                self.expr(then);
                self.expr(els);
            }
            Expr::Lambda { body, .. } => self.expr(body),
            Expr::LambdaStmt { body, .. } => self.stmt(body),
            // 类名当表达式用（静态方法/静态字段的接收者）：按 import 补成内部名
            Expr::Ident(n) => {
                if let Some(full) = self.imported_class(n) {
                    *n = full;
                }
            }
            Expr::MethodRef { recv, .. } => self.expr(recv),
            Expr::SwitchExpr(inner) => self.stmt(inner),
            _ => {}
        }
    }
}

/// 收集本编译单元声明的所有类型名（含嵌套类型）
fn collect_names(types: &[TypeDecl], out: &mut HashSet<String>) {
    for t in types {
        out.insert(t.name.clone());
        for m in &t.members {
            if let Member::Type(inner) = m {
                collect_names(std::slice::from_ref(inner), out);
            }
        }
    }
}

/// 把编译单元里的同包简单类型名补成 `pkg/Name`。
///
/// 没有 `package` 时原样返回（不 clone，走同一条路径）。
pub fn qualify_unit(cu: &CompilationUnit, dirs: &[PathBuf]) -> CompilationUnit {
    let pkg = cu
        .package
        .as_deref()
        .unwrap_or("")
        .replace('.', "/");
    let mut declared = HashSet::new();
    collect_names(&cu.types, &mut declared);
    // 导入表：`import a.b.C;` → ("C", "a/b")，`import a.b.*;` → 通配前缀 "a/b"
    // （静态导入导的是成员不是类型，这里不参与类型名解析）
    let mut singles: Vec<(String, String)> = Vec::new();
    let mut wildcards: Vec<String> = Vec::new();
    for imp in &cu.imports {
        if imp.is_static || imp.name.is_empty() {
            continue;
        }
        if imp.wildcard {
            wildcards.push(imp.name.replace('.', "/"));
            continue;
        }
        let Some((pkg_part, simple)) = imp.name.rsplit_once('.') else {
            continue;
        };
        singles.push((simple.to_string(), pkg_part.replace('.', "/")));
    }
    let q = Qualifier { pkg, declared, singles, wildcards, dirs };
    let mut out = cu.clone();
    for t in out.types.iter_mut() {
        q.type_decl(t);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_compilation_unit;

    #[test]
    fn qualifies_same_package_types() {
        let src = r#"
            package p;
            class B { }
            class A {
                B b;
                p.B other;
                String s;
                A[] arr;
                B make() { return new B(); }
                void take(java.util.List l, Unknown u) { }
            }
        "#;
        let cu = parse_compilation_unit(src).expect("parse");
        let out = qualify_unit(&cu, &[]);
        let a = out.types.iter().find(|t| t.name == "A").expect("A");
        let Member::Field(f) = &a.members[0] else { panic!("field") };
        assert_eq!(f.ty, Type::Named { path: vec!["p".into(), "B".into()], args: vec![] });
        let Member::Field(f) = &a.members[1] else { panic!("field") };
        assert_eq!(f.ty, Type::Named { path: vec!["p".into(), "B".into()], args: vec![] });
        // java.lang 的简写不能被包名污染
        let Member::Field(f) = &a.members[2] else { panic!("field") };
        assert_eq!(f.ty, Type::Named { path: vec!["String".into()], args: vec![] });
        // 数组元素也要补
        let Member::Field(f) = &a.members[3] else { panic!("field") };
        assert_eq!(
            f.ty,
            Type::Array(Box::new(Type::Named { path: vec!["p".into(), "A".into()], args: vec![] }))
        );
        // 全限定名与未知简单名都原样保留
        let Member::Method(md) = &a.members[5] else { panic!("method") };
        assert_eq!(md.params[0].ty, Type::Named {
            path: vec!["java".into(), "util".into(), "List".into()],
            args: vec![],
        });
        assert_eq!(md.params[1].ty, Type::Named { path: vec!["Unknown".into()], args: vec![] });
    }

    #[test]
    fn no_package_keeps_types_as_is() {
        let src = r#"class A { B b; }"#;
        let cu = parse_compilation_unit(src).expect("parse");
        let out = qualify_unit(&cu, &[]);
        let Member::Field(f) = &out.types[0].members[0] else { panic!("field") };
        assert_eq!(f.ty, Type::Named { path: vec!["B".into()], args: vec![] });
    }
}
