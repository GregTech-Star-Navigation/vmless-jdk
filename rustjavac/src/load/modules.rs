//! 模块路径（`--module-path`）+ **模块访问规则**（exports / opens / reads）。
//!
//! 与 `load::ClassPath` 的分工：那边是「一层扁平查找」，这边是「模块 → 包 → 类」的
//! 带边界查找。有模块信息时 `ClassPath` 先问这里，没有再退回扁平查找。
//!
//! 三种模块家：jimage（`lib/modules`）、解包镜像根（`img/java.base/...`）、
//! 单模块目录 / 单模块 jar（根上有 `module-info.class`）。
//!
//! 访问规则**默认拒绝**：一个包只有被目标模块 `exports`（普通访问）或 `opens`
//! （反射访问）给源模块时才允许跨界 —— 唯一例外是 `module` 是 `open module`。

use crate::classfile::attr::{module_info, ModuleInfo};
use crate::load::ClassSource;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::modgraph::ModuleGraph;

/// 未命名模块的规范名
pub const UNNAMED: &str = "<unnamed>";

pub fn is_unnamed(module: &str) -> bool {
    module.is_empty() || module == UNNAMED || module == "UNNAMED"
}

fn unnamed_label(module: &str) -> String {
    if is_unnamed(module) {
        "unnamed module".to_string()
    } else {
        format!("module {module}")
    }
}

/// 类的内部名 → 它所在的包（点分）
pub fn package_of(class: &str) -> String {
    match class.rsplit_once('/') {
        Some((p, _)) => p.replace('/', "."),
        None => String::new(),
    }
}

// ---------------------------------------------------------------- 模块从哪来

#[derive(Debug)]
enum Home {
    /// jimage：条目是 `/<module>/<entry>`
    JImage { image: usize, module: String },
    /// 解包镜像根：文件在 `root/<module>/<entry>`
    ImgRoot { root: PathBuf, module: String },
    /// 单模块目录：文件在 `root/<entry>`
    FlatDir { root: PathBuf },
    /// 单模块 jar：条目在根上
    Jar { image: usize },
}

/// 模块路径：模块名 → 字节来源 + 模块图 + 访问规则
#[derive(Debug)]
pub struct ModuleStore {
    pub graph: ModuleGraph,
    homes: HashMap<String, Home>,
    /// jimage / jar 这类「一个文件装多个模块」的来源（下标即 `Home` 里的 image）
    images: Vec<ClassSource>,
    /// 显式 `--limit-modules` 之后仍然可见的模块
    pub limited: Option<HashSet<String>>,
    /// `--add-exports` / `--add-opens` / `--add-reads`
    pub rules: AccessRules,
}

/// `--add-exports` / `--add-opens` / `--add-reads` 解析出来的规则
#[derive(Clone, Debug, Default)]
pub struct AccessRules {
    pub add_exports: Vec<(String, String, String)>,
    pub add_opens: Vec<(String, String, String)>,
    pub add_reads: Vec<(String, String)>,
}

/// 访问检查需要的全部材料（`ModuleGraph` 是 `Clone`，反射层可以直接拿一份）
#[derive(Clone, Debug, Default)]
pub struct Access {
    pub graph: ModuleGraph,
    pub rules: AccessRules,
}

fn read_module_info(bytes: &[u8], what: &str) -> Result<ModuleInfo, String> {
    let cf = crate::classfile::parse(bytes).map_err(|e| format!("解析 {what} 失败: {e}"))?;
    match module_info(&cf).map_err(|e| format!("解析 {what} 的 Module 属性失败: {e}"))? {
        Some(mi) => Ok(mi),
        None => Err(format!("{what} 里没有 Module 属性（不是模块描述符）")),
    }
}

impl ModuleStore {
    /// 打开模块路径（`a;b;c`，Windows 上按 `;` 切）。
    pub fn open(spec: &str) -> Result<Self, String> {
        let mut store = ModuleStore {
            graph: ModuleGraph::default(),
            homes: HashMap::new(),
            images: Vec::new(),
            limited: None,
            rules: AccessRules::default(),
        };
        let seps: &[char] = if cfg!(windows) { &[';'] } else { &[':', ';'] };
        let mut any = false;
        for part in spec.split(seps) {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            store.push_entry(Path::new(part))?;
            any = true;
        }
        if !any {
            return Err(format!("模块路径为空: {spec:?}"));
        }
        store.finish()?;
        Ok(store)
    }

    /// 追加一个模块路径项（目录 / jimage / 模块 jar）
    pub fn push_entry(&mut self, p: &Path) -> Result<(), String> {
        if p.is_dir() {
            let has_info = p.join("module-info.class").is_file();
            if has_info {
                let bytes = std::fs::read(p.join("module-info.class"))
                    .map_err(|e| format!("读 {}/module-info.class 失败: {e}", p.display()))?;
                let mi = read_module_info(&bytes, &format!("{}", p.display()))?;
                self.add_module(mi, Home::FlatDir { root: p.to_path_buf() })?;
                return Ok(());
            }
            // 解包镜像根：每个子目录一个模块
            let mut found = 0usize;
            let mut subs: Vec<PathBuf> = std::fs::read_dir(p)
                .map_err(|e| format!("读目录 {} 失败: {e}", p.display()))?
                .flatten()
                .map(|e| e.path())
                .filter(|q| q.is_dir() && q.join("module-info.class").is_file())
                .collect();
            subs.sort();
            for sub in subs {
                let bytes = std::fs::read(sub.join("module-info.class"))
                    .map_err(|e| format!("读 {}/module-info.class 失败: {e}", sub.display()))?;
                let mi = read_module_info(&bytes, &format!("{}", sub.display()))?;
                self.add_module(
                    mi,
                    Home::ImgRoot { root: p.to_path_buf(), module: String::new() },
                )?;
                found += 1;
            }
            if found == 0 {
                return Err(format!(
                    "{} 既不是模块目录（没有 module-info.class），也不是模块镜像（子目录里也没有）",
                    p.display()
                ));
            }
            return Ok(());
        }
        let src = ClassSource::open(p)?;
        match src {
            ClassSource::JImage(img) => {
                let idx = self.images.len();
                let mut mods: Vec<String> = img
                    .entry_names()
                    .into_iter()
                    .filter_map(|n| {
                        let rest = n.strip_prefix('/')?;
                        let (m, f) = rest.rsplit_once('/')?;
                        if f == "module-info.class" && !m.contains('/') {
                            Some(m.to_string())
                        } else {
                            None
                        }
                    })
                    .collect();
                mods.sort();
                mods.dedup();
                if mods.is_empty() {
                    return Err(format!("{} 里没有 module-info（不是模块镜像）", p.display()));
                }
                self.images.push(ClassSource::JImage(img));
                for m in mods {
                    let entry = format!("/{m}/module-info.class");
                    let bytes = match self.images[idx].find(&entry)? {
                        Some(b) => b,
                        None => return Err(format!("jimage 里读不到 {entry}")),
                    };
                    let mut mi = read_module_info(&bytes, &entry)?;
                    // jimage 的 module-info 一般带 `ModulePackages`；没有就从索引里扫
                    if mi.packages.is_empty() {
                        mi.packages = scan_packages(
                            &Home::JImage { image: idx, module: m.clone() },
                            &self.images,
                        )?;
                    }
                    self.homes.insert(mi.name.clone(), Home::JImage { image: idx, module: m });
                    self.merge_info(mi)?;
                }
            }
            ClassSource::Jar(j) => {
                let idx = self.images.len();
                let bytes = match j.find("module-info.class")? {
                    Some(b) => b,
                    None => {
                        return Err(format!(
                            "{} 是普通 jar，没有 module-info.class（放到 -cp / --class-path 上）",
                            p.display()
                        ))
                    }
                };
                let mi = read_module_info(&bytes, &format!("{}/module-info.class", p.display()))?;
                self.images.push(ClassSource::Jar(j));
                self.add_module(mi, Home::Jar { image: idx })?;
            }
            ClassSource::Dir(_) => return Err(format!("{} 的处理分支不该走到这里", p.display())),
        }
        Ok(())
    }

    fn add_module(&mut self, mut mi: ModuleInfo, home: Home) -> Result<(), String> {
        let name = mi.name.clone();
        let home = match home {
            Home::ImgRoot { root, .. } => Home::ImgRoot { root, module: name.clone() },
            h => h,
        };
        if self.homes.contains_key(&name) {
            return Err(format!("模块 {name} 在模块路径上出现了两次"));
        }
        if mi.packages.is_empty() {
            mi.packages = scan_packages(&home, &self.images)?;
        }
        self.homes.insert(name, home);
        self.merge_info(mi)
    }

    fn merge_info(&mut self, mi: ModuleInfo) -> Result<(), String> {
        if let Some(prev) = self.graph.modules.insert(mi.name.clone(), mi.clone()) {
            let _ = prev;
            return Err(format!("模块 {} 在模块路径上出现了两次", mi.name));
        }
        Ok(())
    }

    /// 收口：建图（拓扑排序 / split package 检查 / 包索引）
    pub fn finish(&mut self) -> Result<(), String> {
        let infos: Vec<ModuleInfo> = self.graph.modules.values().cloned().collect();
        let g = ModuleGraph::build(infos)?;
        self.graph = g;
        Ok(())
    }

    /// 把另一个 store 的模块合并进来（系统镜像 + `-p` 上的应用模块）
    pub fn merge(&mut self, other: ModuleStore) -> Result<(), String> {
        let base = self.images.len();
        let ModuleStore { graph, homes, images, limited: _, rules } = other;
        for (name, mut home) in homes {
            // image 下标要整体平移（`Home::JImage/Jar` 里的 image 相对 `other.images`）
            match &mut home {
                Home::JImage { image, .. } | Home::Jar { image } => *image += base,
                _ => {}
            }
            if self.homes.contains_key(&name) {
                return Err(format!("模块 {name} 同时出现在系统镜像和模块路径上"));
            }
            self.homes.insert(name, home);
        }
        for src in images {
            self.images.push(src);
        }
        self.rules.add_exports.extend(rules.add_exports);
        self.rules.add_opens.extend(rules.add_opens);
        self.rules.add_reads.extend(rules.add_reads);
        for (_, mi) in graph.modules {
            self.merge_info(mi)?;
        }
        self.finish()
    }

    pub fn access(&self) -> Access {
        Access { graph: self.graph.clone(), rules: self.rules.clone() }
    }

    /// `--limit-modules`：只保留这些模块 + 它们的传递依赖
    pub fn limit(&mut self, names: &[String]) -> Result<(), String> {
        if names.is_empty() {
            return Ok(());
        }
        let mut keep: HashSet<String> = HashSet::new();
        let mut stack: Vec<String> = names.to_vec();
        // java.base 永远保留
        if self.graph.contains("java.base") {
            stack.push("java.base".into());
        }
        while let Some(n) = stack.pop() {
            if !self.graph.contains(&n) {
                return Err(format!("--limit-modules 里的模块 {n} 不在模块图上"));
            }
            if !keep.insert(n.clone()) {
                continue;
            }
            for d in self.graph.deps(&n) {
                if self.graph.contains(d) {
                    stack.push(d.to_string());
                }
            }
        }
        self.limited = Some(keep.clone());
        self.graph.modules.retain(|k, _| keep.contains(k));
        self.graph.order.retain(|k| keep.contains(k));
        self.graph.packages.retain(|_, m| keep.contains(m));
        Ok(())
    }

    /// `--describe-module` 里模块位置（`demo file:///D:/x/`）—— 运行时镜像里的模块不打印
    /// （真 java 的 `isJrt(location)` 过滤）。
    pub fn location(&self, module: &str) -> Option<String> {
        match self.homes.get(module)? {
            Home::FlatDir { root } => Some(file_uri(root, true)),
            Home::ImgRoot { .. } | Home::JImage { .. } => None,
            Home::Jar { image } => match &self.images[*image] {
                ClassSource::Jar(j) => Some(file_uri(j.path(), false)),
                _ => None,
            },
        }
    }

    /// 类是不是由模块路径提供的（是 → 带模块边界）
    pub fn has_module(&self, module: &str) -> bool {
        self.graph.contains(module)
    }

    /// 内部类名 → 模块名（模块路径上找不到就是 None = 未命名模块）
    pub fn module_of_class(&self, class: &str) -> Option<&str> {
        self.graph.module_of_class(class)
    }

    /// 在**某个模块**里找类
    pub fn find_in_module(&self, module: &str, class: &str) -> Result<Option<Vec<u8>>, String> {
        let Some(home) = self.homes.get(module) else { return Ok(None) };
        let entry = format!("{class}.class");
        match home {
            Home::JImage { image, module } => {
                self.images[*image].find(&format!("/{module}/{entry}"))
            }
            Home::ImgRoot { root, module } => {
                let p = root.join(module).join(entry.replace('/', std::path::MAIN_SEPARATOR_STR));
                match std::fs::read(&p) {
                    Ok(b) => Ok(Some(b)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(format!("读 {} 失败: {e}", p.display())),
                }
            }
            Home::FlatDir { root } => {
                let p = root.join(entry.replace('/', std::path::MAIN_SEPARATOR_STR));
                match std::fs::read(&p) {
                    Ok(b) => Ok(Some(b)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(format!("读 {} 失败: {e}", p.display())),
                }
            }
            Home::Jar { image } => self.images[*image].find(&entry),
        }
    }

    /// 按「包 → 模块」找类（模块路径上的类只可能属于一个模块）
    pub fn find(&self, class: &str) -> Result<Option<Vec<u8>>, String> {
        let Some(module) = self.graph.module_of_class(class) else { return Ok(None) };
        self.find_in_module(module, class)
    }
}

// ---------------------------------------------------------------- 访问规则

impl Access {
    fn exports_of(&self, module: &str, pkg: &str) -> Vec<String> {
        self.graph
            .get(module)
            .map(|m| {
                m.exports
                    .iter()
                    .filter(|e| e.package == pkg)
                    .flat_map(|e| e.to.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn has_unqualified_export(&self, module: &str, pkg: &str) -> bool {
        self.graph
            .get(module)
            .map(|m| m.exports.iter().any(|e| e.package == pkg && e.to.is_empty()))
            .unwrap_or(false)
    }

    fn has_unqualified_open(&self, module: &str, pkg: &str) -> bool {
        self.graph
            .get(module)
            .map(|m| m.opens.iter().any(|o| o.package == pkg && o.to.is_empty()))
            .unwrap_or(false)
    }

    fn opens_to_listed(&self, module: &str, pkg: &str, to: &str) -> bool {
        self.graph
            .get(module)
            .map(|m| {
                m.opens.iter().any(|o| {
                    o.package == pkg && o.to.iter().any(|t| t == to || (is_unnamed(to) && t == "ALL-UNNAMED"))
                })
            })
            .unwrap_or(false)
    }

    fn target_matches(t: &str, to: &str) -> bool {
        t == to || (is_unnamed(to) && t == "ALL-UNNAMED")
    }

    /// 模块 `module` 的包 `pkg` 是否**导出**给 `to`
    pub fn exported_to(&self, module: &str, pkg: &str, to: &str) -> bool {
        if module == to {
            return true;
        }
        if self.graph.is_open(module) {
            return true;
        }
        if self.has_unqualified_export(module, pkg) {
            return true;
        }
        if self
            .exports_of(module, pkg)
            .iter()
            .any(|t| Self::target_matches(t, to))
        {
            return true;
        }
        self.rules
            .add_exports
            .iter()
            .any(|(m, p, t)| m == module && p == pkg && Self::target_matches(t, to))
    }

    /// 模块 `module` 的包 `pkg` 是否**开放**（反射）给 `to`
    pub fn opened_to(&self, module: &str, pkg: &str, to: &str) -> bool {
        if module == to {
            return true;
        }
        if self.graph.is_open(module) {
            return true;
        }
        if self.has_unqualified_open(module, pkg) {
            return true;
        }
        if self.opens_to_listed(module, pkg, to) {
            return true;
        }
        self.rules
            .add_opens
            .iter()
            .any(|(m, p, t)| m == module && p == pkg && Self::target_matches(t, to))
    }

    /// 模块 `from` 是否声明了读 `to`（`--add-reads` 也算）
    pub fn reads(&self, from: &str, to: &str) -> bool {
        if from == to || is_unnamed(from) {
            return true; // 未命名模块读所有模块
        }
        if self.graph.deps(from).iter().any(|d| *d == to) {
            return true;
        }
        self.rules
            .add_reads
            .iter()
            .any(|(m, t)| m == from && (t == to || t == "ALL-UNNAMED"))
    }

    /// **字节码引用**的访问检查（对应 HotSpot 的 `Reflection`/`LinkResolver` 检查）。
    ///
    /// 返回 `Err(IllegalAccessError 的消息)`；消息里写清「哪个模块的哪个包、被谁挡住」。
    pub fn check_class_access(
        &self,
        requester: &str,
        requester_class: &str,
        target_class: &str,
    ) -> Result<(), String> {
        if self.graph.modules.is_empty() {
            return Ok(());
        }
        let Some(tm) = self.graph.module_of_class(target_class) else {
            return Ok(()); // 目标是未命名模块（classpath 上的类）：谁都能访问
        };
        if tm == requester {
            return Ok(());
        }
        let pkg = package_of(target_class);
        if self.exported_to(tm, &pkg, requester) {
            return Ok(());
        }
        Err(format!(
            "java.lang.IllegalAccessError: class {} (in {}) cannot access class {} (in module {}) \
             because module {} does not export {} to {}",
            requester_class.replace('/', "."),
            unnamed_label(requester),
            target_class.replace('/', "."),
            tm,
            tm,
            pkg,
            unnamed_label(requester)
        ))
    }

    /// **反射访问**的检查（`setAccessible` / `Field.get` / `Method.invoke`）。
    ///
    /// `is_public`：目标成员是 public 且声明类是 public 时，只要包被 **exports**
    /// 就够；否则要 **opens**。规则与 JDK 的 `AccessibleObject.checkCanSetAccessible` 一致。
    pub fn check_reflective(
        &self,
        requester: &str,
        declaring_class: &str,
        is_public: bool,
        what: &str,
    ) -> Result<(), String> {
        if self.graph.modules.is_empty() {
            return Ok(());
        }
        let Some(dm) = self.graph.module_of_class(declaring_class) else {
            return Ok(()); // 声明类在未命名模块里：它本身就是 open 的
        };
        if dm == requester {
            return Ok(());
        }
        let pkg = package_of(declaring_class);
        if is_public && self.exported_to(dm, &pkg, requester) {
            return Ok(());
        }
        if self.opened_to(dm, &pkg, requester) {
            return Ok(());
        }
        // 措辞对齐 `AccessibleObject.throwInaccessibleObjectException`：
        // public 成员缺的是 exports，非 public 成员缺的是 opens
        let verb = if is_public { "exports" } else { "opens" };
        Err(format!(
            "Unable to make {what} accessible: module {dm} does not \"{verb} {pkg}\" to {}",
            unnamed_label(requester)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classfile::attr::ExportDesc;

    fn access_with(exports: Vec<(&str, Vec<&str>)>, opens: Vec<(&str, Vec<&str>)>) -> Access {
        let mut java_base = ModuleInfo {
            name: "java.base".into(),
            packages: vec!["java.lang".into(), "java.lang.invoke".into(), "jdk.internal.misc".into()],
            ..Default::default()
        };
        for (p, to) in exports {
            java_base.exports.push(ExportDesc {
                package: p.into(),
                flags: 0,
                to: to.into_iter().map(|s| s.to_string()).collect(),
            });
        }
        for (p, to) in opens {
            java_base.opens.push(ExportDesc {
                package: p.into(),
                flags: 0,
                to: to.into_iter().map(|s| s.to_string()).collect(),
            });
        }
        let graph = ModuleGraph::build(vec![java_base]).unwrap();
        Access { graph, rules: AccessRules::default() }
    }

    #[test]
    fn same_module_always_ok() {
        let a = access_with(vec![], vec![]);
        assert!(a.check_class_access("java.base", "java/lang/Object", "jdk/internal/misc/Unsafe").is_ok());
    }

    #[test]
    fn exported_package_ok_others_denied() {
        let a = access_with(vec![("java.lang", vec![])], vec![]);
        assert!(a.check_class_access(UNNAMED, "App", "java/lang/Object").is_ok());
        let e = a.check_class_access(UNNAMED, "App", "jdk/internal/misc/Unsafe").unwrap_err();
        assert!(e.contains("does not export jdk.internal.misc"), "{e}");
    }

    #[test]
    fn qualified_export_only_for_named_target() {
        let a = access_with(vec![("jdk.internal.misc", vec!["jdk.unsupported"])], vec![]);
        assert!(a.check_class_access("jdk.unsupported", "x/Y", "jdk/internal/misc/Unsafe").is_ok());
        assert!(a.check_class_access(UNNAMED, "App", "jdk/internal/misc/Unsafe").is_err());
    }

    #[test]
    fn add_exports_opens_the_door() {
        let mut a = access_with(vec![], vec![]);
        a.rules.add_exports.push((
            "java.base".into(),
            "jdk.internal.misc".into(),
            "ALL-UNNAMED".into(),
        ));
        assert!(a.check_class_access(UNNAMED, "App", "jdk/internal/misc/Unsafe").is_ok());
    }

    #[test]
    fn reflect_needs_opens_for_private() {
        let mut a = access_with(vec![("java.lang.invoke", vec![])], vec![]);
        let e = a
            .check_reflective(UNNAMED, "java/lang/invoke/MethodHandles$Lookup", false, "field lookupClass")
            .unwrap_err();
        assert!(e.contains("does not \"opens java.lang.invoke\""), "{e}");
        assert!(e.contains("unnamed module"), "{e}");
        // public 成员只要 exports 就够
        assert!(a.check_reflective(UNNAMED, "java/lang/invoke/MethodHandles$Lookup", true, "x").is_ok());
        a.rules.add_opens.push((
            "java.base".into(),
            "java.lang.invoke".into(),
            "ALL-UNNAMED".into(),
        ));
        assert!(a
            .check_reflective(UNNAMED, "java/lang/invoke/MethodHandles$Lookup", false, "field lookupClass")
            .is_ok());
    }
}

// ---------------------------------------------------------------- 包扫描

/// 从模块的**实际条目**扫出它包含哪些包。
///
/// 为什么要这个：`ModulePackages` 属性不是必须的 —— 本机 `VMLESS_JDK` 用的那份
/// 解包镜像（jar/patch 产物）里的 `module-info.class` 就没有它。只信属性的话，
/// 「模块 → 包」索引会是空的（表现：模块模式下所有类都找不到，退回内建替身）。
/// 属性在就用属性（快、准），不在就扫目录 / 索引。
fn scan_packages(home: &Home, images: &[ClassSource]) -> Result<Vec<String>, String> {
    let mut pkgs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut add = |rel: &str| {
        // rel 是 `java/lang/Object.class`
        let rel = rel.trim_start_matches('/');
        if !rel.ends_with(".class") || rel.ends_with("module-info.class") {
            return;
        }
        if let Some((dir, _)) = rel.rsplit_once('/') {
            pkgs.insert(dir.replace('/', "."));
        }
    };
    match home {
        Home::JImage { image, module } => {
            let prefix = format!("/{module}/");
            for name in source_names(&images[*image]) {
                if let Some(rest) = name.strip_prefix(&prefix) {
                    if !rest.contains("module-info") {
                        add(rest);
                    }
                }
            }
        }
        Home::ImgRoot { root, module } => {
            walk_class_files(&root.join(module), &mut |rel| add(rel))?;
        }
        Home::FlatDir { root } => {
            walk_class_files(root, &mut |rel| add(rel))?;
        }
        Home::Jar { image } => {
            for name in source_names(&images[*image]) {
                add(&name);
            }
        }
    }
    Ok(pkgs.into_iter().collect())
}

/// 一个来源里的全部条目名（jimage / jar 才有；目录来源用 `walk_class_files`）
fn source_names(src: &ClassSource) -> Vec<String> {
    match src {
        ClassSource::JImage(s) => s.entry_names(),
        ClassSource::Jar(s) => s.names(),
        ClassSource::Dir(_) => Vec::new(),
    }
}

/// 递归走一个模块目录，回调相对路径（`java/lang/Object.class`）
fn walk_class_files(root: &Path, f: &mut impl FnMut(&str)) -> Result<(), String> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("读 {} 失败: {e}", dir.display())),
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|s| s.to_str()) == Some("class") {
                if let Ok(rel) = p.strip_prefix(root) {
                    let rel = rel.to_string_lossy().replace('\\', "/");
                    f(&rel);
                }
            }
        }
    }
    Ok(())
}

/// 本地路径 → `file:///…` URI（目录带尾斜杠），与 `ModuleReference.location()` 一致
fn file_uri(p: &Path, dir: bool) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let mut out = if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    };
    if dir && !out.ends_with('/') {
        out.push('/');
    }
    out
}
