//! 模块图：`模块名 → ModuleInfo`，按 `requires` 做拓扑排序 + 环检测。
//!
//! 这一层只关心**模块之间的关系**，不碰字节从哪来（那是 `modules.rs` 的事）。
//! 图里可以出现「`requires` 指向图外模块」的情况：那是未解析的模块，
//! 排序时跳过、访问检查时按「不存在」处理（与 `--limit-modules` 之后的效果一致）。

use crate::classfile::attr::{ModuleInfo, ACC_MODULE_OPEN};
use std::collections::{BTreeMap, HashMap, HashSet};

fn is_runtime_requires(flags: u16) -> bool {
    // `ACC_STATIC_PHASE` 的 requires 只在编译期可见，运行期不参与解析
    flags & crate::classfile::attr::ACC_REQ_STATIC_PHASE == 0
}

/// 模块名 → 描述符 + 依赖序（被依赖的在前）+ 包归属
#[derive(Clone, Debug, Default)]
pub struct ModuleGraph {
    pub modules: BTreeMap<String, ModuleInfo>,
    /// 拓扑序：**依赖在前**（`java.base` 永远第一个）
    pub order: Vec<String>,
    /// 包（点分）→ 模块名
    pub packages: HashMap<String, String>,
}

impl ModuleGraph {
    /// 建图：拓扑排序（依赖在前），顺带做**包唯一性**（split package）检查。
    pub fn build(infos: Vec<ModuleInfo>) -> Result<Self, String> {
        let mut modules = BTreeMap::new();
        for mi in infos {
            if mi.name.is_empty() {
                return Err("module-info 里没有模块名".to_string());
            }
            if modules.insert(mi.name.clone(), mi.clone()).is_some() {
                return Err(format!("模块 {} 在模块路径上出现了两次", mi.name));
            }
        }
        let mut g = ModuleGraph { modules, order: Vec::new(), packages: HashMap::new() };
        g.order = g.topo_sort()?;
        g.index_packages()?;
        Ok(g)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.modules.contains_key(name)
    }

    pub fn get(&self, name: &str) -> Option<&ModuleInfo> {
        self.modules.get(name)
    }

    /// 运行期依赖（按模块声明顺序，跳过 static-phase）
    pub fn deps(&self, name: &str) -> Vec<&str> {
        self.modules
            .get(name)
            .map(|m| {
                m.requires
                    .iter()
                    .filter(|r| is_runtime_requires(r.flags))
                    .map(|r| r.module.as_str())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// DFS 拓扑排序；遇到环直接报「环上都有谁」。
    fn topo_sort(&self) -> Result<Vec<String>, String> {
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            White,
            Grey,
            Black,
        }
        let mut mark: HashMap<&str, Mark> =
            self.modules.keys().map(|k| (k.as_str(), Mark::White)).collect();
        let mut out: Vec<String> = Vec::with_capacity(self.modules.len());
        // 迭代式 DFS（模块数只有几十，但用显式栈能精确报出环路径）
        for root in self.modules.keys() {
            if mark[root.as_str()] != Mark::White {
                continue;
            }
            let mut stack: Vec<(&str, usize)> = vec![(root.as_str(), 0)];
            let mut path: Vec<String> = vec![root.clone()];
            mark.insert(root.as_str(), Mark::Grey);
            while let Some((node, idx)) = stack.pop() {
                let deps = self.deps(node);
                if idx < deps.len() {
                    stack.push((node, idx + 1));
                    let d = deps[idx];
                    match mark.get(d).copied() {
                        Some(Mark::Grey) => {
                            let start = path.iter().position(|p| p == d).unwrap_or(0);
                            let mut cyc = path[start..].to_vec();
                            cyc.push(d.to_string());
                            return Err(format!("模块依赖成环: {}", cyc.join(" -> ")));
                        }
                        Some(Mark::Black) | None => {}
                        Some(Mark::White) => {
                            mark.insert(d, Mark::Grey);
                            path.push(d.to_string());
                            stack.push((d, 0));
                        }
                    }
                } else {
                    mark.insert(node, Mark::Black);
                    out.push(node.to_string());
                    path.pop();
                }
            }
        }
        Ok(out)
    }

    /// 包 → 模块索引；同一个包出现在两个模块里就是 split package，直接报错。
    fn index_packages(&mut self) -> Result<(), String> {
        let mut map: HashMap<String, String> = HashMap::new();
        for (name, mi) in &self.modules {
            for pkg in &mi.packages {
                if let Some(prev) = map.insert(pkg.clone(), name.clone()) {
                    if prev != *name {
                        // 与真 java 的 boot layer 报错对齐（`LayerInstantiationException`）
                        return Err(format!(
                            "Package {pkg} in both module {prev} and module {name}\
                             \n  （split package：同一个包 {pkg} 同时出现在模块 {prev} 和 {name} 里，\
                             一个包只能属于一个模块）"
                        ));
                    }
                }
            }
        }
        self.packages = map;
        Ok(())
    }

    pub fn module_of_package(&self, pkg: &str) -> Option<&str> {
        self.packages.get(pkg).map(|s| s.as_str())
    }

    /// 内部类名（`java/lang/Object`）→ 模块名
    pub fn module_of_class(&self, class: &str) -> Option<&str> {
        let pkg = class.rsplit_once('/').map(|(p, _)| p.replace('/', ".")).unwrap_or_default();
        self.module_of_package(&pkg)
    }

    /// `--describe-module` 的输出（与真 `java --describe-module` 逐行对齐）。
    ///
    /// 段落顺序是实测出来的（JDK 25）：
    /// `<name>@<version>` → `exports` → `opens` → `requires` → `uses` → `provides`
    /// → `qualified exports` → `qualified opens` → `contains`。
    pub fn describe(&self, name: &str) -> Option<String> {
        let mi = self.modules.get(name)?;
        Some(render_describe(mi))
    }

    /// `--print-module-resolution` 风格的 requires 树
    pub fn requires_tree(&self, root: &str) -> String {
        let mut out = String::new();
        let mut seen: HashSet<String> = HashSet::new();
        self.tree_walk(root, 0, &mut seen, &mut out);
        out
    }

    fn tree_walk(&self, name: &str, depth: usize, seen: &mut HashSet<String>, out: &mut String) {
        let pad = "  ".repeat(depth);
        if depth == 0 {
            out.push_str(&format!("root {name}\n"));
        }
        if !seen.insert(name.to_string()) {
            out.push_str(&format!("{pad}{name} (已展开)\n"));
            return;
        }
        let deps = self.deps(name);
        if deps.is_empty() {
            if depth == 0 {
                out.push_str(&format!("{pad}(没有 requires)\n"));
            }
            return;
        }
        for d in deps {
            if self.contains(d) {
                out.push_str(&format!("{pad}requires {d}\n"));
                self.tree_walk(d, depth + 1, seen, out);
            } else {
                out.push_str(&format!("{pad}requires {d} (未解析)\n"));
            }
        }
    }

    pub fn is_open(&self, name: &str) -> bool {
        self.modules.get(name).map(|m| m.flags & ACC_MODULE_OPEN != 0).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classfile::attr::RequiresDesc;

    fn mi(name: &str, requires: &[&str]) -> ModuleInfo {
        ModuleInfo {
            name: name.into(),
            requires: requires
                .iter()
                .map(|r| RequiresDesc { module: (*r).into(), flags: 0, version: None })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn topo_puts_dependencies_first() {
        let g = ModuleGraph::build(vec![
            mi("app", &["lib", "java.base"]),
            mi("lib", &["java.base"]),
            mi("java.base", &[]),
        ])
        .expect("build");
        assert_eq!(g.order, vec!["java.base", "lib", "app"]);
    }

    #[test]
    fn detects_cycle() {
        let e =
            ModuleGraph::build(vec![mi("a", &["b"]), mi("b", &["c"]), mi("c", &["a"])]).unwrap_err();
        assert!(e.contains("成环"), "{e}");
    }

    #[test]
    fn detects_split_package() {
        let mut a = mi("a", &[]);
        a.packages = vec!["p".into()];
        let mut b = mi("b", &[]);
        b.packages = vec!["p".into()];
        let e = ModuleGraph::build(vec![a, b]).unwrap_err();
        assert!(e.contains("split package"), "{e}");
    }

    #[test]
    fn requires_flags_printed() {
        let mut m = mi("x", &[]);
        m.requires.push(RequiresDesc { module: "y".into(), flags: 0x8020, version: None });
        let g = ModuleGraph::build(vec![m, mi("y", &[])]).unwrap();
        let d = g.describe("x").unwrap();
        assert!(d.contains("requires y transitive mandated"), "{d}");
    }
}

// ------------------------------------------------------- describe-module 的渲染

/// 复刻 `sun.launcher.LauncherHelper.describeModule` 的输出（JDK 25）。
///
/// 顺序不是我们定的，是 JDK 定的：
/// - `exports`（非限定）：按包名排序；
/// - `requires` / `uses` / `provides` / `qualified exports` / `opens`：打的是
///   `ModuleDescriptor` 里 `HashSet` 的迭代顺序（`jhash` 复刻了那条构造链）；
/// - `contains`：`TreeSet` = 未被 export / open 的包，按名字排序。
fn render_describe(mi: &ModuleInfo) -> String {
    use crate::load::jhash::{string_hash, JTable};
    use std::collections::BTreeSet;

    fn mods_sum(flags: u16, names: &[(u16, &str)]) -> i32 {
        let mut h: i32 = 0;
        for (bit, n) in names {
            if flags & bit != 0 {
                h = h.wrapping_add(string_hash(n));
            }
        }
        h
    }

    // 一条 exports/opens 的 hashCode：modsHashCode*43 + source.hashCode() → *43 + targets.hashCode()
    fn export_hash(pkg: &str, flags: u16, targets: &[String]) -> i32 {
        let mods = mods_sum(flags, &[(0x1000, "SYNTHETIC"), (0x8000, "MANDATED")]);
        let tsum = targets
            .iter()
            .fold(0i32, |a, t| a.wrapping_add(string_hash(t)));
        mods.wrapping_mul(43)
            .wrapping_add(string_hash(pkg))
            .wrapping_mul(43)
            .wrapping_add(tsum)
    }

    // Requires.hashCode：name*43 + modsHashCode(mods)
    fn requires_hash(name: &str, flags: u16) -> i32 {
        let mods = mods_sum(
            flags,
            &[
                (0x0020, "TRANSITIVE"),
                (0x0040, "STATIC"),
                (0x1000, "SYNTHETIC"),
                (0x8000, "MANDATED"),
            ],
        );
        string_hash(name).wrapping_mul(43).wrapping_add(mods)
    }

    // Provides.hashCode：service*43 + providers.hashCode()
    fn provides_hash(service: &str, providers: &[String]) -> i32 {
        let psum = providers
            .iter()
            .fold(0i32, |a, p| a.wrapping_add(string_hash(p)));
        string_hash(service).wrapping_mul(43).wrapping_add(psum)
    }

    /// builder 里 `HashMap<String, X>` 按声明顺序 put → `values()` 的迭代顺序
    fn map_values_order(keys: &[String]) -> Vec<usize> {
        let mut t = JTable::hash_map();
        for (i, k) in keys.iter().enumerate() {
            t.add(string_hash(k), i);
        }
        t.order()
    }

    /// `new HashSet<>(collection)`：按 collection 的迭代顺序插入
    fn set_from_order(hashes: &[i32], order: &[usize]) -> Vec<usize> {
        let mut t = JTable::from_collection(order.len());
        for &i in order {
            t.add(hashes[i], i);
        }
        t.order()
    }

    /// 一条 exports 的 targets：`HashSet.newHashSet(k)` → `new HashSet<>(该集合)`
    fn targets_order(targets: &[String]) -> Vec<usize> {
        let mut first = JTable::new_hash_set(targets.len());
        for (i, t) in targets.iter().enumerate() {
            first.add(string_hash(t), i);
        }
        let hashes: Vec<i32> = targets.iter().map(|t| string_hash(t)).collect();
        set_from_order(&hashes, &first.order())
    }

    let mut out = String::new();
    // 一行摘要：`<name>@<version>`（open module 追加 ` open`）
    match &mi.version {
        Some(v) => out.push_str(&format!("{}@{v}\n", mi.name)),
        None => out.push_str(&format!("{}\n", mi.name)),
    }
    if mi.flags & ACC_MODULE_OPEN != 0 {
        // `--describe-module` 在 `toNameAndVersion` 之后追加 ` open`
        out = format!("{} open\n", out.trim_end());
    }

    // ---- exports（非限定，按包名排序）----
    let mut plain: Vec<_> = mi.exports_all().collect();
    plain.sort_by(|a, b| a.package.cmp(&b.package));
    for e in plain {
        out.push_str(&format!("exports {}", e.package));
        out.push_str(&mods_text(e.flags, &[(0x1000, "synthetic"), (0x8000, "mandated")]));
        out.push('\n');
    }

    // ---- requires（HashSet 迭代顺序）----
    let req_keys: Vec<String> = mi.requires.iter().map(|r| r.module.clone()).collect();
    let req_vals = map_values_order(&req_keys);
    let req_hashes: Vec<i32> =
        mi.requires.iter().map(|r| requires_hash(&r.module, r.flags)).collect();
    for i in set_from_order(&req_hashes, &req_vals) {
        let r = &mi.requires[i];
        out.push_str(&format!("requires {}", r.module));
        out.push_str(&mods_text(
            r.flags,
            &[
                (0x0020, "transitive"),
                (0x0040, "static"),
                (0x1000, "synthetic"),
                (0x8000, "mandated"),
            ],
        ));
        out.push('\n');
    }

    // ---- uses（HashSet，插入顺序 = 声明顺序）----
    let mut uses = JTable::hash_map();
    for (i, u) in mi.uses.iter().enumerate() {
        uses.add(string_hash(u), i);
    }
    for i in uses.order() {
        out.push_str(&format!("uses {}\n", mi.uses[i]));
    }

    // ---- provides（HashSet of Provides；providers 自己也是 HashSet）----
    let prov_keys: Vec<String> = mi.provides.iter().map(|p| p.service.clone()).collect();
    let prov_vals = map_values_order(&prov_keys);
    let prov_hashes: Vec<i32> = mi
        .provides
        .iter()
        .map(|p| provides_hash(&p.service, &p.with))
        .collect();
    for i in set_from_order(&prov_hashes, &prov_vals) {
        let p = &mi.provides[i];
        let order = targets_order(&p.with);
        let names: Vec<&str> = order.iter().map(|&j| p.with[j].as_str()).collect();
        out.push_str(&format!("provides {} with {}\n", p.service, names.join(" ")));
    }

    // ---- qualified exports ----
    let exp_keys: Vec<String> = mi.exports.iter().map(|e| e.package.clone()).collect();
    let exp_vals = map_values_order(&exp_keys);
    let exp_hashes: Vec<i32> =
        mi.exports.iter().map(|e| export_hash(&e.package, e.flags, &e.to)).collect();
    for i in set_from_order(&exp_hashes, &exp_vals) {
        let e = &mi.exports[i];
        if e.to.is_empty() {
            continue;
        }
        let order = targets_order(&e.to);
        let names: Vec<&str> = order.iter().map(|&j| e.to[j].as_str()).collect();
        out.push_str(&format!("qualified exports {} to {}\n", e.package, names.join(" ")));
    }

    // ---- opens（非限定的直接 `opens`，限定的加 `qualified` 前缀）----
    let open_keys: Vec<String> = mi.opens.iter().map(|o| o.package.clone()).collect();
    let open_vals = map_values_order(&open_keys);
    let open_hashes: Vec<i32> =
        mi.opens.iter().map(|o| export_hash(&o.package, o.flags, &o.to)).collect();
    for i in set_from_order(&open_hashes, &open_vals) {
        let o = &mi.opens[i];
        out.push_str(if o.to.is_empty() { "opens " } else { "qualified opens " });
        out.push_str(&o.package);
        out.push_str(&mods_text(o.flags, &[(0x1000, "synthetic"), (0x8000, "mandated")]));
        if !o.to.is_empty() {
            let order = targets_order(&o.to);
            let names: Vec<&str> = order.iter().map(|&j| o.to[j].as_str()).collect();
            out.push_str(&format!(" to {}", names.join(" ")));
        }
        out.push('\n');
    }

    // ---- contains：没被 export / open 的包（TreeSet，排序）----
    let mut concealed: BTreeSet<&str> = mi.packages.iter().map(|s| s.as_str()).collect();
    for e in &mi.exports {
        concealed.remove(e.package.as_str());
    }
    for o in &mi.opens {
        concealed.remove(o.package.as_str());
    }
    for p in concealed {
        out.push_str(&format!("contains {p}\n"));
    }
    out
}

/// 修饰符文本（小写，按传入顺序的前缀拼在名字后面）
fn mods_text(flags: u16, names: &[(u16, &str)]) -> String {
    let mut s = String::new();
    for (bit, n) in names {
        if flags & bit != 0 {
            s.push(' ');
            s.push_str(n);
        }
    }
    s
}
