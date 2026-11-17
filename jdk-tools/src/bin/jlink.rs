//! `jlink` —— 从模块路径裁剪出一个运行时镜像。
//!
//! **真模块解析**：读每个 `module-info.class` 的 `Module` 属性，从
//! `--add-modules` 的根模块出发沿 `requires` 求传递闭包，`--limit-modules`
//! 再取交集。`--strip-debug` 真的按属性表剥掉调试信息。
//!
//! 镜像布局（真 JDK 用 jimage 容器，我们写目录形式）：
//! ```text
//! <out>/lib/modules/<module>/...    模块目录树（可作 VMLESS_JDK，我们的加载器认这层）
//! <out>/classes/...                 合并后的扁平树（可直接 `-cp <out>/classes`）
//! <out>/release                     MODULES= 等键值
//! ```
//! 边界：不写 `lib/modules` 的 jimage 二进制容器（那需要写索引/行为定义），
//! `--compress` 只影响 jimage 容器，因此这里接受但不起作用（会在 `--verbose` 里说明）。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use vmless_tools::cli::Args;
use vmless_tools::scan;
use vmless_tools::{die, mkdirs, split_list, usage_err, EXIT_USAGE};

const TOOL: &str = "jlink";

fn help() {
    println!("Usage: jlink <options> --module-path <module path> --add-modules <module>[,<module>...] --output <path>");
    println!();
    println!(" Possible options include:");
    println!("      --add-modules <mod>[,<mod>...]    Root modules to resolve");
    println!("      --compress <0|1|2>                Only affects a jimage container; the");
    println!("                                        directory image is always uncompressed");
    println!("      --limit-modules <mod>[,<mod>...]  Limit the universe of observable modules");
    println!("      --module-path <path>              Module path");
    println!("      --output <path>                   Location of output path");
    println!("      --strip-debug                     Strip debug information from the output");
    println!("      --no-header-files                 Exclude header files (there are none here)");
    println!("      --no-man-pages                    Exclude man pages (there are none here)");
    println!("  -v, --verbose                         Trace module resolution");
    println!("      --version                         Version information");
    println!("  -h, --help                            Print this help message");
    println!();
    println!("边界：真 JDK 把模块打进 `lib/modules`（jimage 容器）。本实现写**目录形式**：");
    println!("`lib/modules/<module>/...`（模块树，可作 VMLESS_JDK）+ `classes/`（扁平树，可 -cp）。");
    println!("`--compress` 因此不起作用（目录不压缩）；`--strip-debug` 是真按属性表删的。");
}

fn main() {
    vmless_tools::install_pipe_guard();
    let args = Args::new();
    if args.wants_help() || args.is_empty() {
        help();
        return;
    }
    if args.wants_version() {
        vmless_tools::print_version(TOOL, "25");
        return;
    }
    let takes: &[&str] = &[
        "--module-path",
        "-p",
        "--add-modules",
        "--output",
        "--compress",
        "--limit-modules",
    ];
    let known: &[&str] = &[
        "--module-path",
        "-p",
        "--add-modules",
        "--output",
        "--compress",
        "--limit-modules",
        "--strip-debug",
        "--no-header-files",
        "--no-man-pages",
        "-v",
        "--verbose",
        "--version",
        "-h",
        "--help",
    ];
    if let Err(e) = args.reject_unknown(TOOL, known, takes) {
        eprintln!("{e}");
        std::process::exit(EXIT_USAGE);
    }
    let mpath = match args.value("--module-path") {
        Ok(Some(v)) => v,
        Ok(None) => match args.value("-p") {
            Ok(Some(v)) => v,
            _ => usage_err(TOOL, "缺少 --module-path"),
        },
        Err(e) => usage_err(TOOL, &e),
    };
    let add = match args.value("--add-modules") {
        Ok(Some(v)) => split_list(&v),
        Ok(None) => usage_err(TOOL, "缺少 --add-modules"),
        Err(e) => usage_err(TOOL, &e),
    };
    let out = match args.value("--output") {
        Ok(Some(v)) => v,
        Ok(None) => usage_err(TOOL, "缺少 --output"),
        Err(e) => usage_err(TOOL, &e),
    };
    let limit: Vec<String> = match args.value("--limit-modules") {
        Ok(Some(v)) => split_list(&v),
        _ => Vec::new(),
    };
    let strip = args.flag("--strip-debug");
    let verbose = args.flag("-v") || args.flag("--verbose");
    let compress = match args.value("--compress") {
        Ok(Some(v)) => v,
        _ => "0".to_string(),
    };

    if let Err(e) = run(
        &mpath, &add, &out, &limit, strip, verbose, &compress,
    ) {
        die(TOOL, &e);
    }
}

struct ModData {
    module_info: Vec<u8>,
    requires: Vec<String>,
    /// 模块内相对路径 → 字节（含 module-info.class）
    files: BTreeMap<String, Vec<u8>>,
}

fn load_modules(mpath: &str) -> Result<HashMap<String, ModData>, String> {
    let mut out: HashMap<String, ModData> = HashMap::new();
    for p in vmless_tools::split_paths(mpath) {
        let path = Path::new(&p);
        if !path.exists() {
            return Err(format!("--module-path 里的路径不存在: {p}"));
        }
        // 一个模块路径项可能是：模块目录树 / 单个 jmod / jimage / 单模块目录
        if path.is_file() {
            let (kind, classes) = scan::collect_classes(path, &[])?;
            let _ = kind;
            let mut by_mod: HashMap<String, ModData> = HashMap::new();
            // jmod 的类在 classes/ 下，scan 已经剥了前缀但丢了模块名 → 重新按 entry 分组
            let mut mi: Option<(String, Vec<u8>)> = None;
            for e in &classes {
                if e.entry == "module-info.class" || e.entry.ends_with("/module-info.class") {
                    let info = vmless_tools::modinfo::parse_module_info(&e.bytes)?;
                    mi = Some((info.name.clone(), e.bytes.clone()));
                    let d = by_mod.entry(info.name.clone()).or_insert(ModData {
                        module_info: e.bytes.clone(),
                        requires: info.requires.iter().map(|r| r.name.clone()).collect(),
                        files: BTreeMap::new(),
                    });
                    d.files.insert("module-info.class".into(), e.bytes.clone());
                    continue;
                }
                if let Some((name, _)) = &mi {
                    let d = by_mod.entry(name.clone()).or_insert(ModData {
                        module_info: Vec::new(),
                        requires: Vec::new(),
                        files: BTreeMap::new(),
                    });
                    d.files.insert(e.entry.clone(), e.bytes.clone());
                } else {
                    // 没有 module-info：无法归属，直接报出来（jmod 必须有）
                    return Err(format!(
                        "{} 里没有 module-info.class —— 它不是一个模块",
                        path.display()
                    ));
                }
            }
            for (k, v) in by_mod {
                out.insert(k, v);
            }
            continue;
        }
        // 目录：可能是模块树，也可能是单模块根
        let subs: Vec<PathBuf> = std::fs::read_dir(path)
            .map_err(|e| format!("读目录 {} 失败: {e}", path.display()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir() && p.join("module-info.class").is_file())
            .collect();
        let mut roots: Vec<PathBuf> = Vec::new();
        if path.join("module-info.class").is_file() {
            roots.push(path.to_path_buf());
        }
        roots.extend(subs);
        if roots.is_empty() {
            return Err(format!(
                "{} 下没有任何含 module-info.class 的模块（既不是模块树也不是模块根）",
                path.display()
            ));
        }
        for r in roots {
            let mi_bytes = std::fs::read(r.join("module-info.class"))
                .map_err(|e| format!("读 {}/module-info.class 失败: {e}", r.display()))?;
            let info = vmless_tools::modinfo::parse_module_info(&mi_bytes)?;
            let mut files = BTreeMap::new();
            for f in vmless_tools::walk_files(&r, false)? {
                let rel = f
                    .strip_prefix(&r)
                    .map_err(|e| format!("算相对路径失败: {e}"))?
                    .to_string_lossy()
                    .replace('\\', "/");
                let data = std::fs::read(&f).map_err(|e| format!("读 {} 失败: {e}", f.display()))?;
                files.insert(rel, data);
            }
            out.insert(
                info.name.clone(),
                ModData {
                    module_info: mi_bytes,
                    requires: info.requires.iter().map(|r| r.name.clone()).collect(),
                    files,
                },
            );
        }
    }
    Ok(out)
}

fn run(
    mpath: &str,
    add: &[String],
    out: &str,
    limit: &[String],
    strip: bool,
    verbose: bool,
    compress: &str,
) -> Result<(), String> {
    let mods = load_modules(mpath)?;
    if mods.is_empty() {
        return Err("模块路径上没有任何模块".into());
    }
    // 根模块必须都存在
    for a in add {
        if !mods.contains_key(a) {
            let mut known: Vec<&String> = mods.keys().collect();
            known.sort();
            return Err(format!(
                "找不到模块 {a}（模块路径上的模块：{}）",
                known.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
    }
    // 传递闭包
    let mut selected: BTreeSet<String> = BTreeSet::new();
    let mut queue: Vec<String> = add.to_vec();
    // java.base 永远在（JVM 语义）
    if mods.contains_key("java.base") {
        queue.push("java.base".into());
    }
    while let Some(m) = queue.pop() {
        if !selected.insert(m.clone()) {
            continue;
        }
        let Some(d) = mods.get(&m) else { continue };
        for r in &d.requires {
            if mods.contains_key(r) {
                queue.push(r.clone());
            } else if verbose {
                eprintln!("{TOOL}: {m} requires {r}，但模块路径上没有它（跳过）");
            }
        }
    }
    // --limit-modules：取交集（但保留显式根）
    if !limit.is_empty() {
        let lim: BTreeSet<String> = limit.iter().cloned().collect();
        let mut keep: BTreeSet<String> = BTreeSet::new();
        for m in &selected {
            if lim.contains(m) || add.contains(m) {
                keep.insert(m.clone());
            }
        }
        selected = keep;
    }
    if selected.is_empty() {
        return Err("解析后没有任何模块（检查 --add-modules / --limit-modules）".into());
    }

    let out_path = Path::new(out);
    if out_path.exists() && !out_path.is_dir() {
        return Err(format!("--output 已存在且不是目录: {out}"));
    }
    mkdirs(out_path)?;
    let modules_dir = out_path.join("lib").join("modules");
    let classes_dir = out_path.join("classes");
    mkdirs(&modules_dir)?;
    mkdirs(&classes_dir)?;

    let mut written = 0usize;
    let mut total_bytes = 0u64;
    for name in &selected {
        let Some(d) = mods.get(name) else { continue };
        if verbose {
            println!("{TOOL}: linking {name} ({} 个条目)", d.files.len());
        }
        for (rel, data) in &d.files {
            let mut bytes = data.clone();
            if strip && rel.ends_with(".class") {
                if let Ok(mut cf) = classfile::parse(&bytes) {
                    if vmless_tools::classops::strip_debug(&mut cf).is_ok() {
                        bytes = classfile::write(&cf);
                    }
                }
            }
            total_bytes += bytes.len() as u64;
            // 1) 模块树
            let mut t1 = modules_dir.join(name);
            for seg in rel.split('/') {
                if seg.is_empty() || seg == "." {
                    continue;
                }
                t1.push(seg);
            }
            if let Some(p) = t1.parent() {
                mkdirs(p)?;
            }
            std::fs::write(&t1, &bytes).map_err(|e| format!("写 {} 失败: {e}", t1.display()))?;
            // 2) 扁平树（module-info.class 合并时会撞名，只留一份）
            if rel != "module-info.class" {
                let mut t2 = classes_dir.clone();
                for seg in rel.split('/') {
                    if seg.is_empty() || seg == "." {
                        continue;
                    }
                    t2.push(seg);
                }
                if let Some(p) = t2.parent() {
                    mkdirs(p)?;
                }
                std::fs::write(&t2, &bytes).map_err(|e| format!("写 {} 失败: {e}", t2.display()))?;
            } else {
                // 每个模块的 module-info 放到 classes/<module>/module-info.class，避免互相覆盖
                let t3 = classes_dir.join(name).join("module-info.class");
                mkdirs(&t2_parent(&t3))?;
                std::fs::write(&t3, &bytes).map_err(|e| format!("写 {} 失败: {e}", t3.display()))?;
            }
            written += 1;
        }
    }
    // release 文件
    let mods_list: Vec<String> = selected.iter().cloned().collect();
    let release = format!(
        "MODULES=\"{}\"\nIMPLEMENTOR=\"vmless\"\nJAVA_VERSION=\"25\"\nJAVA_VERSION_DATE=\"2025-09-16\"\nOS_NAME=\"Windows\"\nOS_ARCH=\"x86_64\"\nVMLESS_IMAGE_LAYOUT=\"directory\"\n",
        mods_list.join(" ")
    );
    std::fs::write(out_path.join("release"), release.as_bytes())
        .map_err(|e| format!("写 release 失败: {e}"))?;
    println!(
        "{TOOL}: 镜像已生成 {out}：{} 个模块（{}），{written} 个条目，{total_bytes} 字节{}",
        selected.len(),
        mods_list.join(", "),
        if strip { "，已去调试信息" } else { "" }
    );
    if compress != "0" {
        println!("{TOOL}: --compress={compress} 对目录形式镜像不起作用（真 JDK 压的是 jimage 容器）");
    }
    println!("{TOOL}: 用法：VMLESS_JDK={}/lib/modules 或 -cp {}/classes", out.replace('\\', "/"), out.replace('\\', "/"));
    Ok(())
}

fn t2_parent(p: &Path) -> PathBuf {
    p.parent().map(|x| x.to_path_buf()).unwrap_or_else(|| PathBuf::from("."))
}
