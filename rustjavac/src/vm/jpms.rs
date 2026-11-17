//! 模块模式（JPMS）的入口：`--describe-module` / `--print-module-resolution` / 执行。
//!
//! 三个入口共用同一套「打开系统镜像 + 模块路径 → 建图 → 挂规则」的流程：
//!
//! ```text
//! 系统镜像（VMLESS_JDK 或本机 JDK 的 lib/modules）
//!        +  -p 上的应用模块              → ModuleStore（模块 → 包 → 类）
//!        +  --limit-modules              → 裁掉不可见模块
//!        +  --add-exports/opens/reads    → AccessRules
//!        → ClassPath::set_module_store() → 类查找与访问检查
//! ```
//!
//! **没有模块开关时不走这里**（`bin/rustjava.rs` 判断），旧行为完全不变。

use super::load::ClassStore;
use super::{ExecResult, Vm, VmError};
use crate::load::cli::JpmsOptions;
use crate::load::modules::ModuleStore;
use crate::load::ClassPath;

fn vm_err(msg: impl Into<String>) -> VmError {
    VmError { msg: msg.into(), exc: None }
}

/// 系统模块镜像：`VMLESS_MODULES` > `VMLESS_JDK`（仅当它是 jimage）> 本机 JDK 的 `lib/modules`。
///
/// 为什么默认不用 `VMLESS_JDK` 里的**解包目录**：那份镜像是给扁平模式打的补丁产物，
/// 里面的 `module-info.class` **没有 `ModulePackages` 属性**，于是模块模式下
/// 「包 → 模块」索引只能靠扫 3 万多个 .class 文件补出来（本机实测 50 秒），
/// 而 jimage 自带这个属性、启动是毫秒级。要强制指定镜像用 `VMLESS_MODULES`。
pub fn system_image() -> String {
    if let Ok(v) = std::env::var("VMLESS_MODULES") {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return v;
        }
    }
    if let Ok(v) = std::env::var("VMLESS_JDK") {
        let v = v.trim().to_string();
        if !v.is_empty() && !std::path::Path::new(&v).is_dir() {
            // 明确给的是 jimage 文件（`lib/modules`），直接用
            return v;
        }
    }
    r"C:\Program Files\Eclipse Adoptium\jdk-25.0.4.101-hotspot\lib\modules".to_string()
}

/// 打开模块路径（系统镜像 + `-p`），套上 `--limit-modules` 与三条 `--add-*` 规则
pub fn open_store(opts: &JpmsOptions) -> Result<ModuleStore, String> {
    let image = system_image();
    let mut store = ModuleStore::open(&image)
        .map_err(|e| format!("打开系统模块镜像 {image} 失败: {e}"))?;
    if !opts.module_path.is_empty() {
        let spec = opts.module_path.join(";");
        let extra = ModuleStore::open(&spec)?;
        store.merge(extra)?;
    }
    for r in &opts.add_exports {
        store.rules.add_exports.push((r.module.clone(), r.package.clone(), r.target.clone()));
    }
    for r in &opts.add_opens {
        store.rules.add_opens.push((r.module.clone(), r.package.clone(), r.target.clone()));
    }
    for r in &opts.add_reads {
        store.rules.add_reads.push((r.module.clone(), r.target.clone()));
    }
    store.limit(&opts.limit_modules)?;
    // `--add-modules` 在这里的作用是「解析根」：校验名字并把 ALL-* 展开
    let _roots = expand_add_modules(&store, &opts.add_modules)?;
    Ok(store)
}

/// `--add-modules` 的取值展开（`ALL-SYSTEM` / `ALL-DEFAULT` / `ALL-MODULE-PATH`）
fn expand_add_modules(store: &ModuleStore, list: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for a in list {
        match a.as_str() {
            "ALL-SYSTEM" | "ALL-MODULE-PATH" => out.extend(store.graph.modules.keys().cloned()),
            "ALL-DEFAULT" => out.extend(
                store.graph.modules.keys().filter(|k| k.starts_with("java.")).cloned(),
            ),
            other => {
                if !store.graph.contains(other) {
                    return Err(format!("--add-modules 里的模块 {other} 不在模块图上"));
                }
                out.push(other.to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// `--describe-module <名>`：输出格式与真 `java --describe-module` 一致
pub fn describe_module(opts: &JpmsOptions, name: &str) -> Result<String, String> {
    let store = open_store(opts)?;
    let Some(body) = store.graph.describe(name) else {
        return Err(format!(
            "找不到模块 {name}（模块路径上共 {} 个模块）",
            store.graph.modules.len()
        ));
    };
    // 第一行：`<name>[@<version>][ <location>]` —— 模块路径上的模块要带位置
    // （运行时镜像里的不带，真 java 用 `isJrt(location)` 过滤）
    let mut lines = body.lines();
    let head = lines.next().unwrap_or("");
    let mut out = String::new();
    match store.location(name) {
        Some(loc) => out.push_str(&format!("{head} {loc}\n")),
        None => out.push_str(&format!("{head}\n")),
    }
    for l in lines {
        out.push_str(l);
        out.push('\n');
    }
    Ok(out)
}

/// `--print-module-resolution [根]`：requires 树 + 拓扑序
pub fn print_module_resolution(opts: &JpmsOptions, root: Option<&str>) -> Result<String, String> {
    let store = open_store(opts)?;
    let root = root.unwrap_or("java.base");
    if !store.graph.contains(root) {
        return Err(format!("找不到模块 {root}"));
    }
    let mut out = String::new();
    out.push_str(&format!("模块数: {}\n", store.graph.modules.len()));
    out.push_str(&store.graph.requires_tree(root));
    out.push_str("\n拓扑序（依赖在前）:\n");
    for (i, m) in store.graph.order.iter().enumerate() {
        out.push_str(&format!("{:>2}. {m}\n", i + 1));
    }
    Ok(out)
}

/// 类名规范化（`a.b.C` / `a/b/C` / `a/b/C.class` → `a/b/C`）
fn internal_name(n: &str) -> String {
    let n = n.trim();
    let n = n.strip_suffix(".class").unwrap_or(n);
    if n.contains('/') {
        n.to_string()
    } else {
        n.replace('.', "/")
    }
}

/// 模块模式执行主类
pub fn run(opts: &JpmsOptions) -> Result<ExecResult, VmError> {
    let store = open_store(opts).map_err(vm_err)?;
    // 主类：`--module m/Main` 优先，其次第一个位置参数
    let (main_class, classpath) = match &opts.launch_module {
        Some((_, Some(main))) => (internal_name(main), opts.class_path.clone()),
        Some((module, None)) => {
            let m = store
                .graph
                .get(module)
                .ok_or_else(|| vm_err(format!("找不到模块 {module}")))?;
            let mc = m.main_class.clone().ok_or_else(|| {
                vm_err(format!("模块 {module} 没有 ModuleMainClass，用 --module {module}/<主类> 指定"))
            })?;
            (internal_name(&mc), opts.class_path.clone())
        }
        None => {
            let main = opts.rest.first().ok_or_else(|| vm_err("没有指定主类"))?;
            (internal_name(main), opts.class_path.clone())
        }
    };
    let mut cp = match classpath {
        Some(spec) => ClassPath::from_spec(&spec).map_err(vm_err)?,
        None => ClassPath::new(),
    };
    cp.set_module_store(store);
    let store = ClassStore::load_path(&cp, &main_class)?;
    let mut vm = Vm::from_store(store);
    // classpath 留着：`Class.forName` 运行期按需加载走的是它（与 `Vm::open` 同一套）
    *vm.sh.cp.lock().unwrap_or_else(|e| e.into_inner()) = Some(cp);
    vm.run_main()
}

/// 模块模式下加载失败时，错误消息本身可能已经是 `java.lang.XxxError: ...` 的形态。
/// 这个函数负责把它变成真 java 的那一行（`Exception in thread "main" …`）。
pub fn format_load_error(msg: &str) -> String {
    if msg.starts_with("java.lang.") || msg.starts_with("java.lang.reflect.") {
        format!("Exception in thread \"main\" {msg}")
    } else {
        format!("执行失败: {msg}")
    }
}
