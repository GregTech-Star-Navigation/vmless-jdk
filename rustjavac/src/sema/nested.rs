//! 嵌套类型拍平：把 `Outer.Inner` 变成顶层类型 `Outer$Inner`。
//!
//! 只做**静态嵌套类型**（`static class Inner`、嵌套 `enum` / `interface` / `record`）：
//! 非静态内部类要带一个外部 `this$0` 引用，构造器签名也得跟着变，那是另一件事。
//!
//! 拍平放在**枚举降级之前**（`codegen::compile_with_path` 的第一道工序）：
//! 嵌套 `enum` 一旦拍平成顶层枚举，`sema::enums` 那套「静态常量 + 构造器 +
//! values/valueOf」就原样适用，不用在枚举侧再认识「嵌套」这件事。
//!
//! 引用改写覆盖三种写法：
//!
//! | 源码 | 改写为 |
//! | --- | --- |
//! | `Inner x`（在外层类内部） | `Outer$Inner x` |
//! | `Outer.Inner x`（任意位置） | `Outer$Inner x` |
//! | `Inner.helper()` / `Inner.FIELD` | `Outer$Inner.helper()` / `Outer$Inner.FIELD` |
//!
//! 之后所有阶段（描述符、`this_class`、JVM 的类文件名）都按 `Outer$Inner` 走 ——
//! 这也正是 javac 的命名（`Outer$Inner.class`）。

use crate::ast::*;
use std::collections::HashMap;

/// 拍平后的编译单元：嵌套类型升为顶层，成员里的 `Member::Type` 已经摘掉
pub fn flatten_unit(cu: &CompilationUnit) -> CompilationUnit {
    let mut f = Flattener { nested: Vec::new(), children: HashMap::new() };
    for t in &cu.types {
        f.collect(t, t.name.clone().as_str());
    }
    let mut nested = std::mem::take(&mut f.nested);
    for n in nested.iter_mut() {
        f.rewrite_type_decl(n);
    }
    // 顶层类型本身也要摘掉嵌套成员（它们已经单独成了顶层类型）
    let mut types: Vec<TypeDecl> = Vec::new();
    for t in &cu.types {
        let mut top = t.clone();
        top.members.retain(|m| !matches!(m, Member::Type(_)));
        f.rewrite_type_decl(&mut top);
        types.push(top);
    }
    types.extend(nested);
    CompilationUnit { package: cu.package.clone(), imports: cu.imports.clone(), types }
}

struct Flattener {
    /// 拍平出来的嵌套类型（名字已经改成 `Outer$Inner`）
    nested: Vec<TypeDecl>,
    /// 外层二进制名 → (嵌套类型简单名 → 二进制名)
    children: HashMap<String, HashMap<String, String>>,
}

impl Flattener {
    /// 深度优先收集嵌套类型；`owner` 是直接外层类型的二进制名
    fn collect(&mut self, t: &TypeDecl, owner: &str) {
        for m in &t.members {
            let Member::Type(inner) = m else { continue };
            let bin = format!("{owner}${}", inner.name);
            self.children.entry(owner.to_string()).or_default().insert(inner.name.clone(), bin.clone());
            self.collect(inner, &bin);
            let mut c = inner.clone();
            let source_name = c.name.clone();
            c.name = bin;
            // 构造器靠「名字 == 类型名」认（`Member::Method` 的 ret 为 None）：
            // 类型改名后构造器必须跟着改，否则它就变成一个没有返回类型的方法了
            let new_name = c.name.clone();
            for m in c.members.iter_mut() {
                if let Member::Method(md) = m {
                    if md.ret.is_none() && md.name == source_name {
                        md.name = new_name.clone();
                    }
                }
            }
            // 更深的嵌套已经各自单独拍平，从这份副本里摘掉，别在符号表里出现两次
            c.members.retain(|m| !matches!(m, Member::Type(_)));
            self.nested.push(c);
        }
    }

    /// `A$B$C` 的外层链：`A$B` → `A` → None
    fn parent(bin: &str) -> Option<String> {
        bin.rsplit_once('$').map(|(p, _)| p.to_string())
    }

    /// 在 `bin` 这个类型里能直接用哪些嵌套类型的简单名
    fn visible(&self, bin: &str) -> HashMap<String, String> {
        let mut out = HashMap::new();
        let mut cur = Some(bin.to_string());
        while let Some(c) = cur {
            if let Some(ch) = self.children.get(&c) {
                for (k, v) in ch {
                    out.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
            cur = Self::parent(&c);
        }
        out
    }

    /// `["Outer", "Inner"]` → `Some("Outer$Inner")`；解析不出来返回 None
    fn resolve_path(&self, path: &[String]) -> Option<String> {
        let mut base = path[0].clone();
        for seg in &path[1..] {
            let next = self.children.get(&base).and_then(|m| m.get(seg)).cloned()?;
            base = next;
        }
        Some(base)
    }

    fn rewrite_type(&self, ty: &mut Type, vis: &HashMap<String, String>) {
        match ty {
            Type::Named { path, args } => {
                for a in args.iter_mut() {
                    self.rewrite_type(a, vis);
                }
                if path.len() > 1 {
                    if let Some(bin) = self.resolve_path(path) {
                        *path = vec![bin];
                    }
                } else if let Some(bin) = vis.get(&path[0]) {
                    path[0] = bin.clone();
                }
            }
            Type::Array(inner) => self.rewrite_type(inner, vis),
            _ => {}
        }
    }

    fn rewrite_type_decl(&self, t: &mut TypeDecl) {
        let vis = self.visible(&t.name);
        for ty in t.extends.iter_mut().chain(t.implements.iter_mut()) {
            self.rewrite_type(ty, &vis);
        }
        for m in t.members.iter_mut() {
            match m {
                Member::Field(f) => {
                    self.rewrite_type(&mut f.ty, &vis);
                    if let Some(e) = &mut f.init {
                        self.rewrite_expr(e, &vis);
                    }
                }
                Member::Method(md) => {
                    if let Some(r) = &mut md.ret {
                        self.rewrite_type(r, &vis);
                    }
                    for p in md.params.iter_mut() {
                        self.rewrite_type(&mut p.ty, &vis);
                    }
                    for x in md.throws.iter_mut() {
                        self.rewrite_type(x, &vis);
                    }
                    if let Some(b) = &mut md.body {
                        self.rewrite_stmt(b, &vis);
                    }
                }
                Member::Init { body, .. } => self.rewrite_stmt(body, &vis),
                Member::EnumConst { args, .. } => {
                    for a in args.iter_mut() {
                        self.rewrite_expr(a, &vis);
                    }
                }
                Member::Type(_) => {}
            }
        }
    }

    fn rewrite_stmt(&self, s: &mut Stmt, vis: &HashMap<String, String>) {
        match s {
            Stmt::Block(items) => {
                for it in items.iter_mut() {
                    self.rewrite_stmt(it, vis);
                }
            }
            Stmt::Labeled { body, .. } => self.rewrite_stmt(body, vis),
            Stmt::LocalVar { ty, init, .. } => {
                self.rewrite_type(ty, vis);
                if let Some(e) = init {
                    self.rewrite_expr(e, vis);
                }
            }
            Stmt::Expr(e) | Stmt::Throw(e) => self.rewrite_expr(e, vis),
            Stmt::Return(e) => {
                if let Some(e) = e {
                    self.rewrite_expr(e, vis);
                }
            }
            Stmt::If { cond, then, els } => {
                self.rewrite_expr(cond, vis);
                self.rewrite_stmt(then, vis);
                if let Some(e) = els {
                    self.rewrite_stmt(e, vis);
                }
            }
            Stmt::While { cond, body } => {
                self.rewrite_expr(cond, vis);
                self.rewrite_stmt(body, vis);
            }
            Stmt::DoWhile { body, cond } => {
                self.rewrite_stmt(body, vis);
                self.rewrite_expr(cond, vis);
            }
            Stmt::For { init, cond, update, body } => {
                for s in init.iter_mut() {
                    self.rewrite_stmt(s, vis);
                }
                if let Some(c) = cond {
                    self.rewrite_expr(c, vis);
                }
                for u in update.iter_mut() {
                    self.rewrite_expr(u, vis);
                }
                self.rewrite_stmt(body, vis);
            }
            Stmt::ForEach { ty, iter, body, .. } => {
                self.rewrite_type(ty, vis);
                self.rewrite_expr(iter, vis);
                self.rewrite_stmt(body, vis);
            }
            Stmt::Switch { selector, cases } => {
                self.rewrite_expr(selector, vis);
                for c in cases.iter_mut() {
                    for l in c.labels.iter_mut().flatten() {
                        self.rewrite_expr(l, vis);
                    }
                    for s in c.body.iter_mut() {
                        self.rewrite_stmt(s, vis);
                    }
                }
            }
            Stmt::Try { body, catches, finally } => {
                self.rewrite_stmt(body, vis);
                for c in catches.iter_mut() {
                    for t in c.types.iter_mut() {
                        self.rewrite_type(t, vis);
                    }
                    self.rewrite_stmt(&mut c.body, vis);
                }
                if let Some(f) = finally {
                    self.rewrite_stmt(f, vis);
                }
            }
            Stmt::Yield(e) => self.rewrite_expr(e, vis),
            Stmt::Break(_) | Stmt::Continue(_) | Stmt::Empty => {}
        }
    }

    fn rewrite_expr(&self, e: &mut Expr, vis: &HashMap<String, String>) {
        match e {
            Expr::New { ty, args } => {
                self.rewrite_type(ty, vis);
                for a in args.iter_mut() {
                    self.rewrite_expr(a, vis);
                }
            }
            Expr::ArrayNew { elem, dims } => {
                self.rewrite_type(elem, vis);
                for d in dims.iter_mut() {
                    self.rewrite_expr(d, vis);
                }
            }
            Expr::ArrayInit(items) => {
                for i in items.iter_mut() {
                    self.rewrite_expr(i, vis);
                }
            }
            Expr::Cast { ty, expr } => {
                self.rewrite_type(ty, vis);
                self.rewrite_expr(expr, vis);
            }
            Expr::InstanceOf { expr, ty, .. } => {
                self.rewrite_type(ty, vis);
                self.rewrite_expr(expr, vis);
            }
            Expr::ClassLit(t) => self.rewrite_type(t, vis),
            Expr::Field(recv, name) => {
                // **自底向上**：`A.B.C` 先让内层 `A.B` 拍成 `A$B`，这一层才看得见类名
                self.rewrite_expr(recv, vis);
                if let Expr::Ident(p) = &**recv {
                    // `Color` / `Outer.Inner` 这类**类名**解析成二进制名
                    let p2 = vis.get(p).cloned().unwrap_or_else(|| p.clone());
                    if let Some(base) = self.type_base(&p2, vis) {
                        // `Outer.Inner`（后面还可能再点常量/方法）
                        if let Some(bin) = self.children.get(&base).and_then(|m| m.get(name)) {
                            *e = Expr::Ident(bin.clone());
                            return;
                        }
                    }
                    if p2 != *p {
                        **recv = Expr::Ident(p2);
                    }
                }
            }
            Expr::Call { recv, args, .. } => {
                if let Some(r) = recv {
                    // `Inner.helper()` 里的接收者是个类名：换成 `Outer$Inner`
                    self.rewrite_expr(r, vis);
                    let new = if let Expr::Ident(p) = &**r { vis.get(p).cloned() } else { None };
                    if let Some(bin) = new {
                        **r = Expr::Ident(bin);
                    }
                }
                for a in args.iter_mut() {
                    self.rewrite_expr(a, vis);
                }
            }
            Expr::Binary { lhs, rhs, .. } => {
                self.rewrite_expr(lhs, vis);
                self.rewrite_expr(rhs, vis);
            }
            Expr::Unary { expr, .. } => self.rewrite_expr(expr, vis),
            Expr::Assign { lhs, rhs, .. } => {
                self.rewrite_expr(lhs, vis);
                self.rewrite_expr(rhs, vis);
            }
            Expr::Cond { cond, then, els } => {
                self.rewrite_expr(cond, vis);
                self.rewrite_expr(then, vis);
                self.rewrite_expr(els, vis);
            }
            Expr::ArrayAccess { array, index } => {
                self.rewrite_expr(array, vis);
                self.rewrite_expr(index, vis);
            }
            Expr::MethodRef { recv, .. } => self.rewrite_expr(recv, vis),
            Expr::LambdaStmt { body, .. } => self.rewrite_stmt(body, vis),
            Expr::SwitchExpr(inner) => self.rewrite_stmt(inner, vis),
            _ => {}
        }
    }

    /// 一个名字（类名）对应的二进制名：先看可见的嵌套类型，再看它自己是不是类
    fn type_base(&self, name: &str, vis: &HashMap<String, String>) -> Option<String> {
        if let Some(b) = vis.get(name) {
            return Some(b.clone());
        }
        // 顶层类型（拍平后所有类型都在 children 的键里，或用自身名兜底）
        if self.children.contains_key(name) || name.contains('$') {
            return Some(name.to_string());
        }
        None
    }
}
