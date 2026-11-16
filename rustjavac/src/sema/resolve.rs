//! 类元数据解析：继承链、字段布局、方法表、classpath 查找。
//!
//! 编译期需要父类的信息（字段偏移、方法签名、有没有无参构造器）。来源优先级：
//!
//! 1. 同一编译单元里的类 —— 直接读 AST
//! 2. classpath 目录里的 `.class` —— 用 `classfile::read` 反解
//!
//! **字段布局的约定**（解释器与 JIT 必须与之一致）：父类字段在前、本类字段在后。
//! 于是「子类对象」就是「父类对象 + 追加字段」，同一个字段在父类和子类里的下标完全相同 ——
//! 这正是 `getfield P.x` 作用在 Q 实例上还能取对的原因。
//!
//! 错误一律用 `String` 返回，由调用方（codegen）包成 `CompileError`。

use crate::ast::*;
use crate::codegen::types::{descriptor, internal_name};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// 一个实例字段的元数据
#[derive(Clone, Debug, PartialEq)]
pub struct FieldMeta {
    pub name: String,
    pub desc: String,
}

/// 一个方法（含构造器）的元数据
#[derive(Clone, Debug, PartialEq)]
pub struct MethodMeta {
    pub name: String,
    pub desc: String,
    pub is_static: bool,
}

/// 本类**自己声明**的成员；继承来的部分在查询时沿链拼起来
#[derive(Clone, Debug, Default)]
struct Own {
    super_name: Option<String>,
    /// 直接实现的接口（`implements A, B` / 接口的 `extends A, B`）
    ifaces: Vec<String>,
    /// 是不是接口（决定 class 文件的访问标志与 invokeinterface）
    is_iface: bool,
    /// 只含实例字段，按声明顺序
    fields: Vec<FieldMeta>,
    /// 只含静态字段（`<clinit>` 初始化与 getstatic 用）
    static_fields: Vec<FieldMeta>,
    methods: Vec<MethodMeta>,
}

pub struct Resolver {
    /// 内部类名（`P` / `p/P` / `java/lang/Object`）→ 自己的成员
    own: RefCell<HashMap<String, Own>>,
    /// classpath 目录（找 `X.class`）
    dirs: Vec<PathBuf>,
    /// 已经尝试过、但没找到的类名，避免重复扫目录
    missing: RefCell<HashSet<String>>,
}

/// `java/lang/Object` 的元数据：只有无参构造器（隐式 `super()` 的目标）
fn object_meta() -> Own {
    Own {
        super_name: None,
        ifaces: Vec::new(),
        is_iface: false,
        fields: Vec::new(),
        static_fields: Vec::new(),
        methods: vec![MethodMeta { name: "<init>".into(), desc: "()V".into(), is_static: false }],
    }
}

impl Resolver {
    /// 用一个编译单元 + classpath 目录建解析器
    pub fn new(cu: &CompilationUnit, dirs: &[PathBuf]) -> Self {
        let mut own = HashMap::new();
        own.insert("java/lang/Object".to_string(), object_meta());
        for t in flat_types(&cu.types) {
            let name = internal_type_name(cu.package.as_deref(), &t.name);
            // 接口的 `extends A, B` 是**父接口**（不是父类）：两处合并进 ifaces
            let (super_name, ifaces) = if t.kind == TypeKind::Interface {
                (
                    None,
                    t.extends
                        .iter()
                        .chain(t.implements.iter())
                        .map(|ty| qualify(cu.package.as_deref(), ty))
                        .collect::<Vec<String>>(),
                )
            } else {
                (
                    t.extends.first().map(|ty| qualify(cu.package.as_deref(), ty)),
                    t.implements.iter().map(|ty| qualify(cu.package.as_deref(), ty)).collect(),
                )
            };
            let mut fields = Vec::new();
            let mut static_fields = Vec::new();
            let mut methods = Vec::new();
            for m in &t.members {
                match m {
                    Member::Field(f) => {
                        // 接口里的字段一律是 public static final（Java 语言规范，可不写 static）
                        let is_static_f = f.modifiers.iter().any(|x| x == "static")
                            || t.kind == TypeKind::Interface;
                        if is_static_f {
                            static_fields.push(FieldMeta { name: f.name.clone(), desc: descriptor(&f.ty) });
                        } else {
                            fields.push(FieldMeta { name: f.name.clone(), desc: descriptor(&f.ty) });
                        }
                    }
                    Member::Method(md) => {
                        let is_ctor = md.ret.is_none() && md.name == t.name;
                        methods.push(MethodMeta {
                            name: if is_ctor { "<init>".to_string() } else { md.name.clone() },
                            desc: method_desc(md),
                            is_static: md.modifiers.iter().any(|x| x == "static"),
                        });
                    }
                    _ => {}
                }
            }
            own.insert(
                name.clone(),
                Own {
                    super_name,
                    ifaces,
                    is_iface: t.kind == TypeKind::Interface,
                    fields,
                    static_fields,
                    methods,
                },
            );
        }
        Resolver { own: RefCell::new(own), dirs: dirs.to_vec(), missing: RefCell::new(HashSet::new()) }
    }

    /// 本编译单元里的所有类（内部名），不含 java/lang/Object
    pub fn unit_classes(&self) -> Vec<String> {
        self.own
            .borrow()
            .keys()
            .filter(|k| *k != "java/lang/Object")
            .cloned()
            .collect()
    }

    /// 一个类的直接父类（`None` = 它自己就是 Object）
    pub fn super_of(&self, class: &str) -> Result<Option<String>, String> {
        let own = self.load(class)?;
        Ok(own.super_name.clone())
    }

    /// 从本类到 Object 的继承链（含本类与 Object）
    pub fn chain(&self, class: &str) -> Result<Vec<String>, String> {
        let mut out = vec![class.to_string()];
        let mut cur = class.to_string();
        for _ in 0..64 {
            match self.super_of(&cur)? {
                Some(s) => {
                    if out.contains(&s) {
                        return Err(format!("继承环: {} → {}", cur, s));
                    }
                    out.push(s.clone());
                    cur = s;
                }
                None => return Ok(out),
            }
        }
        Err(format!("继承链过深: {class}"))
    }

    /// 字段布局：父类字段在前、本类字段在后（只含实例字段）
    pub fn layout(&self, class: &str) -> Result<Vec<FieldMeta>, String> {
        let chain = self.chain(class)?;
        let mut out: Vec<FieldMeta> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for c in chain.iter().rev() {
            let own = self.load(c)?;
            for f in &own.fields {
                if !seen.insert(f.name.clone()) {
                    return Err(format!(
                        "字段 `{}` 在父类与子类中重复声明（本编译器不支持字段隐藏，请改名）",
                        f.name
                    ));
                }
                out.push(f.clone());
            }
        }
        Ok(out)
    }

    /// 实例字段：名字 → 下标（含继承）
    pub fn field_index(&self, class: &str) -> Result<HashMap<String, usize>, String> {
        let mut map = HashMap::new();
        for (i, f) in self.layout(class)?.into_iter().enumerate() {
            map.insert(f.name, i);
        }
        Ok(map)
    }

    /// 实例字段：名字 → 描述符（含继承）
    pub fn field_types(&self, class: &str) -> Result<HashMap<String, String>, String> {
        Ok(self.layout(class)?.into_iter().map(|f| (f.name, f.desc)).collect())
    }

    /// 实例方法：名字 → 描述符（本类优先，然后沿继承链；不含静态方法与 `<init>`）
    ///
    /// 接口方法也算：`Shape x; x.area()` 要从接口上取到 `()I` 这个描述符
    /// （`implements` 进来的接口在链上，接口自己的 `extends` 也要跟进去）。
    pub fn instance_methods(&self, class: &str) -> Result<HashMap<String, String>, String> {
        let mut out = HashMap::new();
        for c in self.chain_with_ifaces(class)? {
            let own = self.load(&c)?;
            for m in &own.methods {
                if m.is_static || m.name == "<init>" {
                    continue;
                }
                out.entry(m.name.clone()).or_insert_with(|| m.desc.clone());
            }
        }
        Ok(out)
    }

    /// 静态字段：名字 → (声明类, 描述符)，含继承
    pub fn static_fields(&self, class: &str) -> Result<HashMap<String, (String, String)>, String> {
        let mut out = HashMap::new();
        for c in self.chain_with_ifaces(class)? {
            let own = self.load(&c)?;
            for f in &own.static_fields {
                out.entry(f.name.clone()).or_insert_with(|| (c.clone(), f.desc.clone()));
            }
        }
        Ok(out)
    }

    /// 这个类型是不是接口
    pub fn is_interface(&self, class: &str) -> bool {
        self.load(class).map(|o| o.is_iface).unwrap_or(false)
    }

    /// 直接实现的接口
    pub fn interfaces_of(&self, class: &str) -> Vec<String> {
        self.load(class).map(|o| o.ifaces).unwrap_or_default()
    }

    /// 继承链 + 链上每个类/接口实现（含继承）的接口，去重
    fn chain_with_ifaces(&self, class: &str) -> Result<Vec<String>, String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue: Vec<String> = vec![class.to_string()];
        while let Some(c) = queue.pop() {
            if !seen.insert(c.clone()) {
                continue;
            }
            out.push(c.clone());
            let own = match self.load(&c) {
                Ok(o) => o,
                // 接口没跟到（java.* 的接口我们没有 .class）就跳过，别让整条链失败
                Err(e) => {
                    if c == class {
                        return Err(e);
                    }
                    continue;
                }
            };
            if let Some(s) = own.super_name.clone() {
                queue.push(s);
            }
            queue.extend(own.ifaces.iter().cloned());
        }
        Ok(out)
    }

    /// 静态方法：名字 → 描述符（含继承；返回 (描述符, 声明类)）
    pub fn static_methods(&self, class: &str) -> Result<HashMap<String, (String, String)>, String> {
        let mut out = HashMap::new();
        for c in self.chain(class)? {
            let own = self.load(&c)?;
            for m in &own.methods {
                if !m.is_static || m.name == "<init>" {
                    continue;
                }
                out.entry(m.name.clone()).or_insert_with(|| (m.desc.clone(), c.clone()));
            }
        }
        Ok(out)
    }

    /// 沿继承链找方法（返回**声明它的那个类**）
    pub fn find_method(&self, class: &str, name: &str, desc: &str) -> Result<Option<String>, String> {
        for c in self.chain(class)? {
            let own = self.load(&c)?;
            if own.methods.iter().any(|m| m.name == name && m.desc == desc) {
                return Ok(Some(c));
            }
        }
        Ok(None)
    }

    /// 有没有无参构造器（没写构造器的类算有）
    pub fn has_default_ctor(&self, class: &str) -> Result<bool, String> {
        let own = self.load(class)?;
        let ctors: Vec<&MethodMeta> = own.methods.iter().filter(|m| m.name == "<init>").collect();
        if ctors.is_empty() {
            return Ok(true); // 没声明任何构造器 → 编译器补一个无参的
        }
        Ok(ctors.iter().any(|m| m.desc == "()V"))
    }

    /// 类里声明的所有构造器描述符（`super(args)` 校验用）
    pub fn ctor_descs(&self, class: &str) -> Result<Vec<String>, String> {
        let own = self.load(class)?;
        Ok(own.methods.iter().filter(|m| m.name == "<init>").map(|m| m.desc.clone()).collect())
    }

    /// 取一个类的元数据；本单元里没有就去 classpath 找
    fn load(&self, class: &str) -> Result<Own, String> {
        if let Some(o) = self.own.borrow().get(class) {
            return Ok(o.clone());
        }
        if let Some(o) = self.load_from_disk(class) {
            self.own.borrow_mut().insert(class.to_string(), o.clone());
            return Ok(o);
        }
        // java.* 的类/接口我们一个字节码文件都没有（运行时内建，见 `vm::builtin_super`
        // 与 `codegen::types::BUILTIN_METHODS`）。按**空元数据**放行，别让
        // `class Names implements Iterable {}` 这种声明在毫无关系的地方炸掉。
        if class.starts_with("java/") {
            return Ok(Own::default());
        }
        Err(format!(
            "找不到类 `{}`（cannot find symbol: class {}）",
            class.replace('/', "."),
            class.replace('/', ".")
        ))
    }

    fn load_from_disk(&self, class: &str) -> Option<Own> {
        let path = self.find_class_file(class)?;
        let bytes = std::fs::read(&path).ok()?;
        self.own_from_bytes(&bytes)
    }

    /// 把 classpath 目录里的 `.class` 全部预加载进符号表。
    ///
    /// 跨编译单元引用（`PkgB b = new PkgB(...)`）需要知道对方的字段/方法表，
    /// 而 codegen 的 `type_fields` / `type_methods` 是**一次性**建好的 —— 所以先把
    /// classpath 上的类都读进来，后面查表就不会缺项。
    pub fn preload(&self) {
        let mut files: Vec<PathBuf> = Vec::new();
        for d in &self.dirs {
            collect_class_files(d, 0, &mut files);
        }
        for p in files {
            let Ok(bytes) = std::fs::read(&p) else { continue };
            let Some(o) = self.own_from_bytes(&bytes) else { continue };
            let Some(name) = class_name_of_bytes(&bytes) else { continue };
            if self.own.borrow().contains_key(&name) {
                continue;
            }
            self.own.borrow_mut().insert(name, o);
        }
    }

    /// 所有已知的类名：本编译单元的 + classpath 预加载进来的
    pub fn all_classes(&self) -> Vec<String> {
        self.own
            .borrow()
            .keys()
            .filter(|k| *k != "java/lang/Object")
            .cloned()
            .collect()
    }

    fn own_from_bytes(&self, bytes: &[u8]) -> Option<Own> {
        let cf = crate::parse(bytes).ok()?;
        let _ = cf.this_class_name()?;
        let super_name = match cf.constant_pool.get(cf.super_class as usize).and_then(|e| e.as_ref()) {
            Some(crate::CpEntry::Class(n)) => cf.utf8(*n).map(|b| String::from_utf8_lossy(b).into_owned()),
            _ => None,
        };
        let mut fields = Vec::new();
        let mut static_fields = Vec::new();
        for f in &cf.fields {
            let meta = FieldMeta {
                name: String::from_utf8_lossy(cf.utf8(f.name_index).unwrap_or_default()).into_owned(),
                desc: String::from_utf8_lossy(cf.utf8(f.descriptor_index).unwrap_or_default()).into_owned(),
            };
            if f.access_flags & 0x0008 != 0 {
                static_fields.push(meta);
            } else {
                fields.push(meta);
            }
        }
        let mut methods = Vec::new();
        for m in &cf.methods {
            methods.push(MethodMeta {
                name: String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default()).into_owned(),
                desc: String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default()).into_owned(),
                is_static: m.access_flags & 0x0008 != 0,
            });
        }
        let ifaces = cf
            .interfaces
            .iter()
            .filter_map(|i| match cf.constant_pool.get(*i as usize).and_then(|e| e.as_ref()) {
                Some(crate::CpEntry::Class(n)) => cf.utf8(*n).map(|b| String::from_utf8_lossy(b).into_owned()),
                _ => None,
            })
            .collect();
        Some(Own {
            super_name,
            ifaces,
            is_iface: cf.access_flags & 0x0200 != 0,
            fields,
            static_fields,
            methods,
        })
    }

    fn find_class_file(&self, class: &str) -> Option<PathBuf> {
        if self.missing.borrow().contains(class) {
            return None;
        }
        // compile_src 把包名的 `/` 换成 `_` 写盘，两种命名都试
        let flat = class.replace('/', "_");
        for d in &self.dirs {
            for f in [format!("{class}.class"), format!("{flat}.class")] {
                let p = d.join(f);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
        self.missing.borrow_mut().insert(class.to_string());
        None
    }
}

/// 递归收集目录里的 `.class`（包目录最多四层，够用且不会扫进奇怪的东西）
fn collect_class_files(dir: &std::path::Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_class_files(&p, depth + 1, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("class") {
            out.push(p);
        }
    }
}

/// class 文件里的内部类名
fn class_name_of_bytes(bytes: &[u8]) -> Option<String> {
    let cf = crate::parse(bytes).ok()?;
    cf.this_class_name()
}

/// 类型声明名（带包前缀）
fn internal_type_name(package: Option<&str>, simple: &str) -> String {
    match package {
        Some(p) if !p.is_empty() => format!("{}/{}", p.replace('.', "/"), simple),
        _ => simple.to_string(),
    }
}

/// 父类名：`Type::Named` → 内部名（`P` / `java/lang/Object`）
fn qualify(package: Option<&str>, ty: &Type) -> String {
    match ty {
        Type::Named { path, .. } => {
            let n = internal_name(path);
            if path.len() == 1 && !n.contains('/') {
                // 同包的父类：先按裸名试，找不到的话 codegen 那边的 qualify_path 会补包
                let _ = package;
                n
            } else {
                n
            }
        }
        other => descriptor(other),
    }
}

/// 方法描述符（构造器返回 V）
fn method_desc(md: &MethodDecl) -> String {
    let mut s = String::from("(");
    for p in &md.params {
        s.push_str(&descriptor(&p.ty));
    }
    s.push(')');
    s.push_str(&descriptor(md.ret.as_ref().unwrap_or(&Type::Prim("void"))));
    s
}

/// 拍平嵌套类型（内部类也算本单元里的类）
fn flat_types(types: &[TypeDecl]) -> Vec<&TypeDecl> {
    let mut out = Vec::new();
    for t in types {
        out.push(t);
        for m in &t.members {
            if let Member::Type(inner) = m {
                out.extend(flat_types(std::slice::from_ref(inner)));
            }
        }
    }
    out
}
