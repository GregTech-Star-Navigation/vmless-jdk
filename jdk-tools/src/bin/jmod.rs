//! `jmod` —— JMOD 容器（`JMOD` 8 字节头 + zip）。
//!
//! 动作：`create` / `extract` / `list` / `describe` / `hash`。
//! `create` 产出的文件**真 JDK 的 `jmod list/describe/extract` 能读**（双向互通已实测），
//! 支持 `--compress zip-0`（stored）与 `zip-1..zip-9`（我们的 deflate 编码器）。

use std::path::Path;

use vmless_tools::cli::Args;
use vmless_tools::zipx::{JmodBuilder, ZipReader};
use vmless_tools::{die, mkdirs, split_list, usage_err, EXIT_USAGE};

const TOOL: &str = "jmod";

fn help() {
    println!("Usage: jmod (create|extract|list|describe|hash) <OPTIONS> <jmod-file>");
    println!();
    println!("Main operation modes:");
    println!("  create    - Creates a new jmod archive");
    println!("  extract   - Extracts all files from the jmod archive");
    println!("  list      - Prints the names of all the entries");
    println!("  describe  - Prints the module details");
    println!("  hash      - Records hashes of tied modules.");
    println!();
    println!(" Option                              Description");
    println!(" ------                              -----------");
    println!(" -?, -h, --help                      Print this help message");
    println!(" --class-path <path>                 Application jar files|dir containing classes");
    println!(" --cmds <path>                       Location of native commands  -> bin/");
    println!(" --compress <compress>               Compression to use in creating the JMOD archive.");
    println!("                                     Supported values are zip-[0-9], zip-0 = no");
    println!("                                     compression, default is zip-6 (deflate)");
    println!(" --config <path>                     Location of user-editable config files -> conf/");
    println!(" --date <date>                       Date and time for entry timestamps, ISO-8601");
    println!(" --dir <path>                        Target directory for extract (default: .)");
    println!(" --exclude <pattern-list>            Exclude files (comma separated glob patterns)");
    println!(" --header-files <path>               Location of header files -> include/");
    println!(" --legal-notices <path>              Location of legal notices -> legal/");
    println!(" --libs <path>                       Location of native libraries -> lib/");
    println!(" --version                           Print version information");
    println!();
    println!("本实现的边界：");
    println!("  * `--main-class` / `--module-version` 需要改写 module-info 的 Module 属性与常量池，");
    println!("    本实现不提供（会明确报错）。其余 create/extract/list/describe 都是真实现。");
    println!("  * `hash` 计算并打印模块内容摘要（SHA-256），但不把 ModuleHashes 属性写回 module-info。");
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
        "--class-path",
        "--cmds",
        "--compress",
        "--config",
        "--date",
        "--dir",
        "--exclude",
        "--header-files",
        "--legal-notices",
        "--libs",
        "--main-class",
        "--module-version",
        "--module-path",
        "--hash-modules",
    ];
    let known: &[&str] = &[
        "--class-path",
        "--cmds",
        "--compress",
        "--config",
        "--date",
        "--dir",
        "--dry-run",
        "--exclude",
        "--header-files",
        "--legal-notices",
        "--libs",
        "--main-class",
        "--module-version",
        "--module-path",
        "--hash-modules",
        "--version",
        "--help-extra",
        "-?",
        "-h",
        "--help",
    ];
    if let Err(e) = args.reject_unknown(TOOL, known, takes) {
        eprintln!("{e}");
        std::process::exit(EXIT_USAGE);
    }
    let pos = match args.positionals(takes) {
        Ok(p) => p,
        Err(e) => usage_err(TOOL, &e),
    };
    if pos.is_empty() {
        usage_err(TOOL, "缺少动作（create|extract|list|describe|hash）");
    }
    let action = pos[0].as_str();
    let files: Vec<String> = pos[1..].to_vec();
    let one_file = |what: &str| -> String {
        if files.len() != 1 {
            usage_err(TOOL, &format!("{what} 需要且只要一个 jmod 文件路径"));
        }
        files[0].clone()
    };

    let r = match action {
        "create" => do_create(&args, &one_file("create")),
        "extract" => {
            let dir = args.value("--dir").ok().flatten().unwrap_or_else(|| ".".to_string());
            do_extract(&one_file("extract"), &dir)
        }
        "list" => do_list(&one_file("list")),
        "describe" => do_describe(&one_file("describe")),
        "hash" => do_hash(&args, &one_file("hash")),
        other => usage_err(TOOL, &format!("不认识的动作 {other}")),
    };
    if let Err(e) = r {
        die(TOOL, &e);
    }
}

fn val(args: &Args, name: &str) -> Option<String> {
    match args.value(name) {
        Ok(v) => v,
        Err(e) => usage_err(TOOL, &e),
    }
}

fn do_create(args: &Args, out: &str) -> Result<(), String> {
    for (opt, why) in [
        ("--main-class", "改写 module-info 的 ModuleMainClass 属性"),
        ("--module-version", "改写 module-info 的 Module 属性"),
        ("--hash-modules", "把 ModuleHashes 属性写进 module-info"),
    ] {
        if val(args, opt).is_some() {
            return Err(format!(
                "{opt} 需要{why}（要重建常量池），本实现不提供；请先用别的工具生成 module-info 再打包"
            ));
        }
    }
    let compress = val(args, "--compress").unwrap_or_else(|| "zip-6".to_string());
    let mut builder = JmodBuilder::new(&compress)?;
    if let Some(d) = val(args, "--date") {
        builder.zip.set_date(&d)?;
    }
    let exclude = val(args, "--exclude").map(|v| split_list(&v)).unwrap_or_default();
    let dry = args.flag("--dry-run");
    if exclude.iter().any(|p| p.trim_start().starts_with("regex:")) {
        return Err("--exclude 的 regex: 模式需要正则引擎，本实现只支持 glob".into());
    }

    // (选项, jmod 内的目录前缀)
    let sections: [(&str, &str); 5] = [
        ("--class-path", "classes"),
        ("--header-files", "include"),
        ("--config", "conf"),
        ("--cmds", "bin"),
        ("--libs", "lib"),
    ];
    let mut total = 0usize;
    let mut bytes_total = 0u64;
    let mut has_module_info = false;
    let mut module_name = String::new();
    for (opt, prefix) in sections {
        let Some(paths) = val(args, opt) else { continue };
        for path in paths.split([';']) {
            let p = Path::new(path.trim());
            if p.as_os_str().is_empty() {
                continue;
            }
            if !p.exists() {
                return Err(format!("{opt} 指向的路径不存在: {}", p.display()));
            }
            let files = if p.is_file() {
                // 单个 jar：条目取 jar 里的内容
                let z = ZipReader::open(p)?;
                let mut v = Vec::new();
                for e in &z.entries {
                    if e.name.ends_with('/') {
                        continue;
                    }
                    v.push((e.name.clone(), z.read(e)?));
                }
                v
            } else {
                let mut v = Vec::new();
                for f in vmless_tools::walk_files(p, false)? {
                    let rel = f
                        .strip_prefix(p)
                        .map_err(|e| format!("算相对路径失败: {e}"))?
                        .to_string_lossy()
                        .replace('\\', "/");
                    v.push((
                        rel,
                        std::fs::read(&f).map_err(|e| format!("读 {} 失败: {e}", f.display()))?,
                    ));
                }
                v
            };
            for (rel, data) in files {
                let entry = format!("{prefix}/{rel}");
                if !exclude.is_empty() && vmless_tools::any_match(&exclude, &entry) {
                    continue;
                }
                if prefix == "classes" && rel == "module-info.class" {
                    has_module_info = true;
                    module_name = vmless_tools::modinfo::module_name_of(&data).unwrap_or_default();
                }
                bytes_total += data.len() as u64;
                total += 1;
                if !dry {
                    builder.add_file(&entry, &data)?;
                }
            }
        }
    }
    if !has_module_info {
        return Err("--class-path 里没有 module-info.class：jmod 必须是模块，请先提供它".into());
    }
    if dry {
        println!(
            "{TOOL}: dry-run 校验通过：模块 {module_name}，{total} 个条目，{bytes_total} 字节，压缩={compress}"
        );
        return Ok(());
    }
    let bytes = builder.finish()?;
    let out_path = Path::new(out);
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            mkdirs(parent)?;
        }
    }
    std::fs::write(out_path, &bytes).map_err(|e| format!("写 {} 失败: {e}", out_path.display()))?;
    println!(
        "{TOOL}: 已创建 {}（模块 {module_name}，{total} 个条目，{} 字节，压缩={compress}）",
        out_path.display(),
        bytes.len()
    );
    Ok(())
}

fn do_list(file: &str) -> Result<(), String> {
    let z = ZipReader::open(file)?;
    for e in &z.entries {
        println!("{}", e.name);
    }
    Ok(())
}

fn do_extract(file: &str, dir: &str) -> Result<(), String> {
    let z = ZipReader::open(file)?;
    let root = Path::new(dir);
    mkdirs(root)?;
    let mut n = 0usize;
    let mut total = 0u64;
    for e in &z.entries {
        if e.name.ends_with('/') {
            continue;
        }
        if e.name.contains("..") {
            return Err(format!("条目名非法（含 ..）：{}", e.name));
        }
        let data = z.read(e)?;
        total += data.len() as u64;
        let mut target = root.to_path_buf();
        for seg in e.name.split('/') {
            if seg.is_empty() || seg == "." {
                continue;
            }
            target.push(seg);
        }
        if let Some(parent) = target.parent() {
            mkdirs(parent)?;
        }
        std::fs::write(&target, &data).map_err(|err| format!("写 {} 失败: {err}", target.display()))?;
        n += 1;
    }
    println!("{TOOL}: 从 {file} 解出 {n} 个条目，{total} 字节，到 {dir}");
    Ok(())
}

fn do_describe(file: &str) -> Result<(), String> {
    let z = ZipReader::open(file)?;
    let mi = z
        .get("classes/module-info.class")
        .or_else(|| z.get("module-info.class"))
        .ok_or_else(|| format!("{file} 里没有 classes/module-info.class，不是合法的 jmod"))?;
    let bytes = z.read(mi)?;
    let info = vmless_tools::modinfo::parse_module_info(&bytes)?;
    print!("{}", vmless_tools::modinfo::describe_text(&info));
    if let Some(mc) = &info.main_class {
        println!("  main-class {mc}");
    }
    Ok(())
}

fn do_hash(args: &Args, file: &str) -> Result<(), String> {
    // 对 jmod 里每个条目算 SHA-256，再对整个模块算一个内容摘要。
    // 真 JDK 会把结果写回 module-info 的 ModuleHashes 属性；这一步要重建常量池，
    // 本实现只打印（help 里已写明）。
    let z = ZipReader::open(file)?;
    let mut names: Vec<&str> = z.entries.iter().map(|e| e.name.as_str()).collect();
    names.sort();
    let mut all = vmless_tools::sha1::Sha256::new();
    let mut per_entry = Vec::new();
    for name in &names {
        let e = match z.get(name) {
            Some(e) => e,
            None => continue,
        };
        let data = z.read(e)?;
        let d = vmless_tools::sha1::sha256(&data);
        all.update(name.as_bytes());
        all.update(&d);
        per_entry.push((name.to_string(), d));
    }
    let module_digest = all.finish();
    let dry = args.flag("--dry-run");
    println!(
        "{TOOL}: {} 条目 {} 个",
        file,
        per_entry.len()
    );
    println!("module content digest (SHA-256 over sorted entry-name+entry-digest):");
    println!("  {}", vmless_tools::sha1::hex(&module_digest));
    if dry {
        return Ok(());
    }
    // 非 dry-run：把逐条目摘要写到一个同名 `.hashes` 侧车文件（JDK 写进 module-info）
    let side = format!("{file}.hashes");
    let mut text = String::new();
    for (n, d) in &per_entry {
        text.push_str(&format!("{} {}\n", vmless_tools::sha1::hex(d), n));
    }
    std::fs::write(&side, text).map_err(|e| format!("写 {side} 失败: {e}"))?;
    println!("{TOOL}: 逐条目摘要已写入 {side}（ModuleHashes 属性写回 module-info 未做，见 --help）");
    Ok(())
}
