//! 多类加载与运行期类元数据：继承链、字段布局、方法表。
//!
//! 解释器原来只能跑「一个 class 文件」。继承要求同时知道父类，所以这里把
//! `main` 所在目录里所有 `.class` 都读进来（我们的编译器一个编译单元产出多个类，
//! 都落在同一个目录），建成一张按类名索引的表。
//!
//! 字段布局与编译期 `sema::Resolver` 保持一致：**父类字段在前、本类字段在后**。

use crate::ClassFile;
use std::collections::HashMap;
use std::rc::Rc;

use super::class::builtin_super;
use super::VmError;

pub(crate) struct ClassMeta {
    /// 实例字段名 → 下标（含继承）
    pub(crate) fields: HashMap<String, usize>,
    /// 实例字段总数（含继承），`new` 分配对象时用
    pub(crate) field_count: usize,
}

pub(crate) struct ClassStore {
    pub(super) classes: Vec<Rc<ClassFile>>,
    /// 内部类名 → 类下标
    pub(super) by_name: HashMap<String, usize>,
    pub(super) metas: HashMap<String, ClassMeta>,
    /// (声明类, 方法名, 描述符) → (类下标, 方法下标)
    pub(super) methods: HashMap<(String, String, String), (usize, usize)>,
    /// main 方法所在的类下标
    pub(super) main: usize,
}

fn err<T>(msg: impl Into<String>) -> Result<T, VmError> {
    Err(VmError { msg: msg.into(), exc: None })
}

/// 类的直接父类：已加载的类看 `super_class`，其余靠 java.* 写死的表
fn super_of_of(classes: &[Rc<ClassFile>], by_name: &HashMap<String, usize>, class: &str) -> Option<String> {
    if let Some(&i) = by_name.get(class) {
        let cf = &classes[i];
        return match cf.constant_pool.get(cf.super_class as usize).and_then(|e| e.as_ref()) {
            Some(crate::CpEntry::Class(n)) => cf.utf8(*n).map(|b| String::from_utf8_lossy(b).into_owned()),
            _ => None,
        };
    }
    builtin_super(class).map(|s| s.to_string())
}

impl ClassStore {
    /// 从一个目录里的所有 `.class` 建表；`main` 是入口类名
    pub(super) fn load_dir(dir: &std::path::Path, main_class: &str) -> Result<Self, VmError> {
        let mut files: Vec<ClassFile> = Vec::new();
        let entries = std::fs::read_dir(dir)
            .map_err(|e| VmError { msg: format!("读目录 {} 失败: {e}", dir.display()), exc: None })?;
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("class") {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&p) {
                if let Ok(cf) = crate::parse(&bytes) {
                    files.push(cf);
                }
            }
        }
        Self::new(files, Some(main_class))
    }

    /// 用一批已解析的类建表
    pub(super) fn new(files: Vec<ClassFile>, main_class: Option<&str>) -> Result<Self, VmError> {
        let classes: Vec<Rc<ClassFile>> = files.into_iter().map(Rc::new).collect();
        // 入口类必须排在最前面？不必 —— 按名字找即可
        let mut by_name = HashMap::new();
        for (i, cf) in classes.iter().enumerate() {
            if let Some(n) = cf.this_class_name() {
                by_name.insert(n, i);
            }
        }
        if classes.is_empty() {
            return err("没有可执行的 class");
        }
        let main = match main_class {
            Some(n) => match by_name.get(n) {
                Some(&i) => i,
                None => 0,
            },
            None => 0,
        };
        // 方法表：(声明类, 名字, 描述符)
        let mut methods = HashMap::new();
        for (ci, cf) in classes.iter().enumerate() {
            let owner = cf.this_class_name().unwrap_or_default();
            for (mi, m) in cf.methods.iter().enumerate() {
                let name = String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default()).into_owned();
                let desc = String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default()).into_owned();
                methods.insert((owner.clone(), name, desc), (ci, mi));
            }
        }
        let mut store = ClassStore {
            classes,
            by_name,
            metas: HashMap::new(),
            methods,
            main,
        };
        store.build_metas();
        Ok(store)
    }

    /// 类名 → 继承链（本类 → … → 顶层；未知的祖先名也留在链上）
    pub(super) fn chain(&self, class: &str) -> Vec<String> {
        let mut out = vec![class.to_string()];
        let mut cur = class.to_string();
        for _ in 0..64 {
            match super_of_of(&self.classes, &self.by_name, &cur) {
                Some(s) if s != cur && !out.contains(&s) => {
                    out.push(s.clone());
                    cur = s;
                }
                _ => break,
            }
        }
        out
    }

    /// 一个类**直接**实现的接口（class 文件的 `interfaces` 表）
    pub(super) fn interfaces_of(&self, class: &str) -> Vec<String> {
        let Some(&i) = self.by_name.get(class) else { return Vec::new() };
        let cf = &self.classes[i];
        cf.interfaces
            .iter()
            .filter_map(|ix| match cf.constant_pool.get(*ix as usize).and_then(|e| e.as_ref()) {
                Some(crate::CpEntry::Class(n)) => {
                    cf.utf8(*n).map(|b| String::from_utf8_lossy(b).into_owned())
                }
                _ => None,
            })
            .collect()
    }

    /// 类（含继承链上每个类）实现的全部接口，传递闭包（接口 extends 接口也算）
    pub(super) fn iface_closure(&self, class: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut queue: Vec<String> = self.chain(class);
        while let Some(c) = queue.pop() {
            for i in self.interfaces_of(&c) {
                if seen.insert(i.clone()) {
                    out.push(i.clone());
                    queue.push(i);
                }
            }
        }
        out
    }

    fn build_metas(&mut self) {
        let names: Vec<String> = self.by_name.keys().cloned().collect();
        for name in names {
            let chain = self.chain(&name);
            let mut fields = HashMap::new();
            let mut count = 0usize;
            for c in chain.iter().rev() {
                let Some(&ci) = self.by_name.get(c) else { continue };
                let cf = self.classes[ci].clone();
                for f in &cf.fields {
                    if f.access_flags & 0x0008 != 0 {
                        continue; // 静态字段不在对象里
                    }
                    let n = String::from_utf8_lossy(cf.utf8(f.name_index).unwrap_or_default()).into_owned();
                    fields.entry(n).or_insert(count);
                    count += 1;
                }
            }
            self.metas.insert(name.clone(), ClassMeta { fields, field_count: count });
        }
    }

    pub(crate) fn meta(&self, class: &str) -> Option<&ClassMeta> {
        self.metas.get(class)
    }

    /// 从 `from` 开始沿继承链解析方法
    pub(super) fn resolve_method(&self, from: &str, name: &str, desc: &str) -> Option<(usize, usize)> {
        for c in self.chain(from) {
            if let Some(v) = self.methods.get(&(c, name.to_string(), desc.to_string())) {
                return Some(*v);
            }
        }
        None
    }

    /// 只在这个类**自己**里找方法（`<clinit>` 不继承，不能用 resolve_method）
    pub(super) fn method_in_class(&self, class: &str, name: &str, desc: &str) -> Option<(usize, usize)> {
        self.methods.get(&(class.to_string(), name.to_string(), desc.to_string())).copied()
    }

    /// 直接父类名（已加载的类看 super_class，java.* 靠内置表）
    pub(super) fn super_of(&self, class: &str) -> Option<String> {
        super_of_of(&self.classes, &self.by_name, class)
    }

    /// 从 `from` 开始的继承链上，字段名 → 下标
    pub(super) fn field_index(&self, from: &str, name: &str) -> Option<usize> {
        for c in self.chain(from) {
            if let Some(m) = self.metas.get(&c) {
                if let Some(i) = m.fields.get(name) {
                    return Some(*i);
                }
            }
        }
        None
    }

    pub(super) fn field_count(&self, class: &str) -> usize {
        self.metas.get(class).map(|m| m.field_count).unwrap_or(0)
    }

    /// main 方法
    pub(super) fn main_method(&self) -> Option<usize> {
        let cf = &self.classes[self.main];
        cf.methods.iter().position(|m| {
            let n = String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default());
            let d = String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default());
            n == "main" && d == "([Ljava/lang/String;)V"
        })
    }
}
