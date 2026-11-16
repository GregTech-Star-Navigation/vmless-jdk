//! JIT 侧的类表：加载同目录的 .class、字段布局（含继承）、方法解析。
//!
//! 字段布局与编译期（`sema::Resolver`）和解释器（`vm::load`）**必须一致**：
//! 父类字段在前、本类字段在后，偏移 = 8（对象头）+ 8 * 下标。

use crate::ClassFile;
use std::collections::HashMap;
use std::rc::Rc;

use super::rt;

pub(super) struct ClassSet {
    pub(super) classes: HashMap<String, Rc<ClassFile>>,
    /// 类 → (实例字段名 → 下标)
    fields: HashMap<String, HashMap<String, usize>>,
    /// 类 → 实例字段总数（含继承）
    counts: HashMap<String, usize>,
    /// 类 → 实例字段描述符（下标即字段槽下标，含继承；被遮蔽的槽占位但不被访问）
    descs: HashMap<String, Vec<String>>,
}

impl ClassSet {
    pub(super) fn new() -> Self {
        ClassSet {
            classes: HashMap::new(),
            fields: HashMap::new(),
            counts: HashMap::new(),
            descs: HashMap::new(),
        }
    }

    /// 登记一个类（重复登记直接忽略），并刷新它的布局
    pub(super) fn insert(&mut self, cf: ClassFile) {
        let Some(name) = cf.this_class_name() else { return };
        if self.classes.contains_key(&name) {
            return;
        }
        if let Some(sup) = super_of(&cf) {
            rt::register_super(&name, &sup);
        }
        // 接口登记进运行时的「实现」表：instanceof / checkcast / catch 都按它判
        let ifaces: Vec<String> = cf
            .interfaces
            .iter()
            .filter_map(|ix| match cf.constant_pool.get(*ix as usize).and_then(|e| e.as_ref()) {
                Some(crate::CpEntry::Class(n)) => {
                    cf.utf8(*n).map(|b| String::from_utf8_lossy(b).into_owned())
                }
                _ => None,
            })
            .collect();
        for i in &ifaces {
            rt::register_iface(&name, i);
        }
        self.classes.insert(name, Rc::new(cf));
    }

    /// 把一个目录里的 .class 全部读进来（继承链上的类都在这里）
    pub(super) fn load_dir(&mut self, dir: &std::path::Path) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("class") {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&p) {
                if let Ok(cf) = crate::parse(&bytes) {
                    self.insert(cf);
                }
            }
        }
    }

    pub(super) fn get(&self, class: &str) -> Option<Rc<ClassFile>> {
        self.classes.get(class).cloned()
    }

    pub(super) fn super_of(&self, class: &str) -> Option<String> {
        match self.classes.get(class) {
            Some(cf) => super_of(cf),
            None => None,
        }
    }

    /// 本类 → … → 顶层
    pub(super) fn chain(&self, class: &str) -> Vec<String> {
        let mut out = vec![class.to_string()];
        let mut cur = class.to_string();
        for _ in 0..64 {
            match self.super_of(&cur).or_else(|| crate::vm::builtin_super(&cur).map(|s| s.to_string())) {
                Some(s) if s != cur && !out.contains(&s) => {
                    out.push(s.clone());
                    cur = s;
                }
                _ => break,
            }
        }
        out
    }

    pub(super) fn is_subclass(&self, actual: &str, target: &str) -> bool {
        if actual == target || self.chain(actual).iter().any(|c| c == target) {
            return true;
        }
        // 接口：链上任何一个类（或接口）实现了它都算
        self.iface_closure(actual).iter().any(|c| c == target)
    }

    /// 一个类**直接**实现的接口名
    pub(super) fn interfaces_of(&self, class: &str) -> Vec<String> {
        let Some(cf) = self.classes.get(class) else { return Vec::new() };
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

    /// 类（含继承链）实现的所有接口，传递闭包
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

    /// 这个方法是不是抽象的（接口里的方法声明没有 Code，不能编译成机器码）
    pub(super) fn is_abstract_method(&self, owner: &str, idx: usize) -> bool {
        self.classes
            .get(owner)
            .and_then(|cf| cf.methods.get(idx))
            .map(|m| m.access_flags & 0x0400 != 0)
            .unwrap_or(false)
    }

    /// 实例字段名 → 下标（沿继承链找声明它的那一层）
    pub(super) fn field_offset(&self, from: &str, name: &str) -> Option<usize> {
        for c in self.chain(from) {
            if let Some(m) = self.fields.get(&c) {
                if let Some(i) = m.get(name) {
                    return Some(*i);
                }
            }
        }
        None
    }

    /// 实例字段描述符（下标 = 字段槽下标）。JIT 用它把布局注册进 librustjrt：
    /// 引用字段必须进 GC 的位图，否则 GC 追不到它指向的对象。
    pub(super) fn field_descs(&self, class: &str) -> Vec<String> {
        self.descs.get(class).cloned().unwrap_or_default()
    }

    /// 重建 `class` 的字段布局（父类在前）
    pub(super) fn build_layout(&mut self, class: &str) {
        let chain = self.chain(class);
        let mut map = HashMap::new();
        let mut descs: Vec<String> = Vec::new();
        for c in chain.iter().rev() {
            let Some(cf) = self.get(c) else { continue };
            for f in &cf.fields {
                if f.access_flags & 0x0008 != 0 {
                    continue;
                }
                let n = String::from_utf8_lossy(cf.utf8(f.name_index).unwrap_or_default()).into_owned();
                let d =
                    String::from_utf8_lossy(cf.utf8(f.descriptor_index).unwrap_or_default()).into_owned();
                if map.contains_key(&n) {
                    // 被遮蔽的父类字段：占一个槽，但 `field_offset` 永远指不到它
                    descs.push("I".to_string());
                } else {
                    map.insert(n, descs.len());
                    descs.push(d);
                }
            }
        }
        let count = descs.len();
        self.fields.insert(class.to_string(), map);
        self.counts.insert(class.to_string(), count);
        self.descs.insert(class.to_string(), descs);
    }

    /// 沿继承链找方法，返回 (声明类, 方法下标)
    pub(super) fn resolve_method(&self, from: &str, name: &str, desc: &str) -> Option<(String, usize)> {
        for c in self.chain(from) {
            let Some(cf) = self.get(&c) else { continue };
            for (i, m) in cf.methods.iter().enumerate() {
                let n = String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default());
                let d = String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default());
                if n == name && d == desc {
                    return Some((c, i));
                }
            }
        }
        None
    }
}

/// 类文件的直接父类名
fn super_of(cf: &ClassFile) -> Option<String> {
    match cf.constant_pool.get(cf.super_class as usize).and_then(|e| e.as_ref()) {
        Some(crate::CpEntry::Class(n)) => cf.utf8(*n).map(|b| String::from_utf8_lossy(b).into_owned()),
        _ => None,
    }
}

impl ClassSet {
    /// 所有已加载的类名
    pub(super) fn names(&self) -> Vec<String> {
        self.classes.keys().cloned().collect()
    }
}
