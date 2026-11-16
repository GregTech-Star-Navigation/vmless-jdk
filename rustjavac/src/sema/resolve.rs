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
    /// 只含实例字段，按声明顺序
    fields: Vec<FieldMeta>,
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
        fields: Vec::new(),
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
            let super_name = t.extends.first().map(|ty| qualify(cu.package.as_deref(), ty));
            let mut fields = Vec::new();
            let mut methods = Vec::new();
            for m in &t.members {
                match m {
                    Member::Field(f) => {
                        if !f.modifiers.iter().any(|x| x == "static") {
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
            own.insert(name.clone(), Own { super_name, fields, methods });
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
    pub fn instance_methods(&self, class: &str) -> Result<HashMap<String, String>, String> {
        let mut out = HashMap::new();
        for c in self.chain(class)? {
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
        Err(format!("找不到类 `{}`（cannot find symbol: class {}）", class.replace('/', "."), class.replace('/', ".")))
    }

    fn load_from_disk(&self, class: &str) -> Option<Own> {
        let path = self.find_class_file(class)?;
        let bytes = std::fs::read(&path).ok()?;
        let cf = crate::parse(&bytes).ok()?;
        // 文件名不一定等于类名：以 class 文件里的 this_class 为准
        let _ = cf.this_class_name()?;
        let super_name = match cf.constant_pool.get(cf.super_class as usize).and_then(|e| e.as_ref()) {
            Some(crate::CpEntry::Class(n)) => cf.utf8(*n).map(|b| String::from_utf8_lossy(b).into_owned()),
            _ => None,
        };
        let mut fields = Vec::new();
        for f in &cf.fields {
            if f.access_flags & 0x0008 != 0 {
                continue;
            }
            fields.push(FieldMeta {
                name: String::from_utf8_lossy(cf.utf8(f.name_index).unwrap_or_default()).into_owned(),
                desc: String::from_utf8_lossy(cf.utf8(f.descriptor_index).unwrap_or_default()).into_owned(),
            });
        }
        let mut methods = Vec::new();
        for m in &cf.methods {
            methods.push(MethodMeta {
                name: String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default()).into_owned(),
                desc: String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default()).into_owned(),
                is_static: m.access_flags & 0x0008 != 0,
            });
        }
        Some(Own { super_name, fields, methods })
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
