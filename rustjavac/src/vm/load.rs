//! 多类加载与运行期类元数据：继承链、字段布局、方法表。
//!
//! 类不再靠「扫一个目录」得到，而是沿 `load::ClassPath` **按需查找**：从入口类出发，
//! 顺着它常量池里的 `Class` 项（父类、接口、字段/方法的参数与返回类型都在里面）一个个
//! `find` 回来，形成一个闭包。没被任何已加载类引用到的类不加载 —— 这也是以后跑 jimage
//! 时不会把三万个 java.base 类全解出来的原因。每个名字只查一次，`ClassPath` 内部还有
//! 字节缓存。
//!
//! 字段布局与编译期 `sema::Resolver` 保持一致：**父类字段在前、本类字段在后**。

use crate::load::ClassPath;
use crate::{ClassFile, CpEntry};
use std::collections::HashMap;
use std::sync::Arc;

use super::class::builtin_super;
use super::VmError;

#[derive(Clone)]
pub(crate) struct ClassMeta {
    /// 实例字段名 → 下标（含继承）
    pub(crate) fields: HashMap<String, usize>,
    /// 实例字段总数（含继承），`new` 分配对象时用
    pub(crate) field_count: usize,
}

#[derive(Clone)]
pub(crate) struct ClassStore {
    pub(super) classes: Vec<Arc<ClassFile>>,
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
fn super_of_of(classes: &[Arc<ClassFile>], by_name: &HashMap<String, usize>, class: &str) -> Option<String> {
    if let Some(&i) = by_name.get(class) {
        let cf = &classes[i];
        return match cf.constant_pool.get(cf.super_class as usize).and_then(|e| e.as_ref()) {
            Some(crate::CpEntry::Class(n)) => cf.utf8(*n).map(|b| String::from_utf8_lossy(b).into_owned()),
            _ => None,
        };
    }
    builtin_super(class).map(|s| s.to_string())
}

/// **闭包里要不要拉 `java.*`**：以前一律排除（那时 java.* 全靠 `natives` 写死）。
///
/// 现在反过来 —— 只要 classpath 里真有 `java/lang/Object`（也就是把 jimage / 解包目录
/// 放进了 classpath，见 `VMLESS_JDK`），就**整条 `java.*` 都拉进来**，让真字节码优先执行。
/// 否则（只给了一个 .class 的独立模式）仍然只拉用户自己的类，`java.*` 继续由内建兜。
///
/// 判据只能是「java.base 到底在不在」，不能是「这个类长得像不像 JDK 的」：前者是事实，
/// 后者是猜。猜错的后果就是「真字节码被静默跳过」——那等于什么都没做。
fn pull_jdk(jdk_present: bool, name: &str) -> bool {
    jdk_present || !name.starts_with("java/")
}

impl ClassStore {
    /// 沿 classpath 按需加载入口类 + 它引用到的类。
    ///
    /// 从 `main_class` 出发，按 `Class` 常量池项把父类/接口/签名里出现的类型一个个
    /// `find` 回来（闭包）。找不到的类不报错 —— 那是 `java.*` 之类，靠 `builtin_super`
    /// 与 natives 兜；只有**入口类**找不到才算错。
    pub(super) fn load_path(cp: &ClassPath, main_class: &str) -> Result<Self, VmError> {
        let mut files: Vec<ClassFile> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut queue: Vec<String> = vec![main_class.to_string()];
        // java.base 在不在 classpath 上 —— 一次探测，决定整条闭包要不要拉 `java.*`
        let jdk_present = cp.find("java/lang/Object").ok().flatten().is_some();
        while let Some(name) = queue.pop() {
            if name.is_empty() || name.starts_with('[') || !seen.insert(name.clone()) {
                continue;
            }
            let bytes = match cp.find(&name) {
                Ok(b) => b,
                Err(e) => {
                    // 常量池里的 Class 项只是一笔**引用**，JVM 是"用到才加载"。
                    // 这里为了字段布局/GC 位图做的是预扫描，扫到解不开的条目
                    // （codegen 留下的 `<SELF>` 占位、Windows 上非法文件名…）就跳过，
                    // 不能整个执行失败 —— 主类例外。
                    if name == main_class {
                        return Err(VmError { msg: e, exc: None });
                    }
                    continue;
                }
            };
            let Some(bytes) = bytes else {
                if name == main_class {
                    return err(format!("classpath 里找不到主类 {main_class}"));
                }
                continue;
            };
            let cf = crate::parse(&bytes)
                .map_err(|e| VmError { msg: format!("解析 {name} 失败: {e}"), exc: None })?;
            // 这个类自己属于哪个模块（模块模式才有；否则是未命名模块 = classpath）
            let cur_module = cp.module_of_class(&name).map(|s| s.to_string());
            // 引用到的类型都在 `Class` 常量池项里（含 super_class 与 interfaces 指过去的那两个）。
            // `<SELF>` 是我们编译器写常量池时用的占位符（字节码生成完会被改写成真实类名），
            // 它不是类，别拿它去 classpath 里找。
            for e in &cf.constant_pool {
                if let Some(CpEntry::Class(ix)) = e {
                    if let Some(raw) = cf.utf8(*ix) {
                        let n = String::from_utf8_lossy(raw).into_owned();
                        if n.starts_with('[') || n.contains('<') || n.contains(';') {
                            continue;
                        }
                        // **模块访问检查**：引用者模块 → 被引用类所在模块的包。
                        // 没有模块模式时 `check_reference` 是 no-op（旧行为不变）；
                        // 有模块模式时**默认拒绝**，不许静默放行。
                        if let Err(msg) = cp.check_reference(cur_module.as_deref(), &name, &n) {
                            return Err(VmError { msg, exc: None });
                        }
                        if pull_jdk(jdk_present, &n) {
                            queue.push(n);
                        }
                    }
                }
            }
            files.push(cf);
        }
        if std::env::var_os("VMLESS_CP_STATS").is_some() {
            eprintln!(
                "[cp] 装载 {} 个类，文件探测 {} 次",
                files.len(),
                crate::load::probe_count()
            );
        }
        if files.is_empty() {
            return err("没有可执行的 class");
        }
        Self::new(files, Some(main_class))
    }

    /// 运行期加入一个类（`ClassLoader.defineClass` 的雏形）：同名的换掉，然后重建
    /// 方法表与字段布局（父类新加字段会移动子类字段的偏移，所以只能整体重建）。
    pub(super) fn add_class(&mut self, cf: ClassFile) -> usize {
        let name = cf.this_class_name().unwrap_or_default();
        let idx = match self.by_name.get(&name) {
            Some(&i) => {
                self.classes[i] = Arc::new(cf);
                i
            }
            None => {
                self.classes.push(Arc::new(cf));
                self.classes.len() - 1
            }
        };
        self.by_name.insert(name, idx);
        self.rebuild_methods();
        self.build_metas();
        idx
    }

    fn rebuild_methods(&mut self) {
        self.methods.clear();
        for (ci, cf) in self.classes.iter().enumerate() {
            for (mi, m) in cf.methods.iter().enumerate() {
                let name = String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default()).into_owned();
                let desc = String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default()).into_owned();
                self.methods.insert((cf.this_class_name().unwrap_or_default(), name, desc), (ci, mi));
            }
        }
    }

    /// 用一批已解析的类建表
    pub(super) fn new(files: Vec<ClassFile>, main_class: Option<&str>) -> Result<Self, VmError> {
        let classes: Vec<Arc<ClassFile>> = files.into_iter().map(Arc::new).collect();
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

    /// 已加载的类名（诊断用）
    pub(super) fn class_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.by_name.keys().cloned().collect();
        v.sort();
        v
    }

    /// 这个类的字节码在不在类表里？在就必须执行它，不许拿内建实现顶替
    pub(super) fn has_class(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// 类内部名 → 已解析的 class 文件
    pub(super) fn class_file(&self, name: &str) -> Option<Arc<ClassFile>> {
        self.by_name.get(name).map(|&i| self.classes[i].clone())
    }

    /// 类下标 → 内部名（反射的「调用者是谁」用；越界返回 None）
    pub(super) fn name_of_index(&self, idx: usize) -> Option<String> {
        if idx >= self.classes.len() {
            return None;
        }
        self.classes[idx].this_class_name()
    }

    /// 运行期按需加载一个类**及其引用的闭包**（`Class.forName` 用）。
    ///
    /// 与 `load_path` 是同一套闭包规则，区别是它发生在运行中途：新类直接 `add_class`
    /// 进类表（会顺带重建方法表与字段布局）。已经加载的类不重复加载，所以对
    /// `forName("java.lang.String")` 这类调用是空操作。
    ///
    /// 返回**新加入**的类名。找不到 `root` 自己就返回错误（这是 `ClassNotFoundException`
    /// 的来源），闭包里别的类找不到只跳过（JVMS 允许链接期再报）。
    pub(super) fn load_closure(
        &mut self,
        cp: &ClassPath,
        root: &str,
    ) -> Result<Vec<String>, VmError> {
        let mut added = Vec::new();
        let mut queue: Vec<String> = vec![root.to_string()];
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let jdk_present = self.has_class("java/lang/Object");
        while let Some(name) = queue.pop() {
            if name.is_empty() || name.starts_with('[') || !seen.insert(name.clone()) {
                continue;
            }
            if self.has_class(&name) {
                continue;
            }
            let bytes = match cp.find(&name) {
                Ok(Some(b)) => b,
                Ok(None) => {
                    if name == root {
                        return err(format!("classpath 里找不到 {root}"));
                    }
                    continue;
                }
                Err(e) => return err(e),
            };
            let cf = crate::parse(&bytes)
                .map_err(|e| VmError { msg: format!("解析 {name} 失败: {e}"), exc: None })?;
            for e in &cf.constant_pool {
                if let Some(CpEntry::Class(ix)) = e {
                    if let Some(raw) = cf.utf8(*ix) {
                        let n = String::from_utf8_lossy(raw).into_owned();
                        if !n.starts_with('[')
                            && !n.contains('<')
                            && !n.contains(';')
                            && pull_jdk(jdk_present, &n)
                        {
                            queue.push(n);
                        }
                    }
                }
            }
            self.add_class(cf);
            added.push(name);
        }
        Ok(added)
    }

    /// 从 `from` 开始沿继承链解析方法；类链上找不到再找接口的 **default 实现**。
    ///
    /// **抽象方法不算实现**：它们没有 Code 属性，解析到了也执行不了。跳过它们才能让
    /// 「接口方法调用」正确落到 default 实现（或落到调用点的 lambda/内建分流）。
    pub(super) fn resolve_method(&self, from: &str, name: &str, desc: &str) -> Option<(usize, usize)> {
        let key = (name.to_string(), desc.to_string());
        for c in self.chain(from) {
            if let Some(v) = self.methods.get(&(c, key.0.clone(), key.1.clone())) {
                if !self.is_abstract(*v) {
                    return Some(*v);
                }
            }
        }
        // 接口里的 default 方法：实现类没覆写时落在这里
        for i in self.iface_closure(from) {
            if let Some(v) = self.methods.get(&(i, key.0.clone(), key.1.clone())) {
                // 抽象声明（没有 Code）不能当实现用
                let (ci, mi) = *v;
                if self.classes[ci].methods[mi].attributes.is_empty() || self.is_abstract(*v) {
                    continue;
                }
                return Some(*v);
            }
        }
        None
    }

    /// 这个方法有没有实现体（抽象方法没有）
    fn is_abstract(&self, (ci, mi): (usize, usize)) -> bool {
        const ACC_ABSTRACT: u16 = 0x0400;
        self.classes[ci].methods[mi].access_flags & ACC_ABSTRACT != 0
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

    /// 字段下标 → 字段名（只在这个类**自己**的元数据里找）。
    ///
    /// `Unsafe.staticFieldOffset` 给出「字段下标」，而 `staticFieldBase` 给出的是类对象 ——
    /// 于是 `Unsafe.putInt(base, off)` 落到这里时只剩一个下标，必须能反查名字才能定位静态字段。
    pub(super) fn field_name_at(&self, class: &str, idx: usize) -> Option<String> {
        let m = self.metas.get(class)?;
        m.fields.iter().find(|(_, &i)| i == idx).map(|(n, _)| n.clone())
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
