//! `jimage` —— 读/解/校验 JDK 的 `lib/modules`（jimage 格式）。
//!
//! 支持 `extract` / `info` / `list` / `verify` 四个动作，选项与真 JDK 对齐
//! （`--dir` / `--include` / `--verbose` / `--version` / `-h`）。
//! 真 JDK 的 `recreate` 需要重写整个镜像，本 crate 不含（`--help` 里写明）。

use vmless_tools::cli::Args;
use vmless_tools::jimagex::Image;
use vmless_tools::{any_match, die, mkdirs, split_list, usage_err, EXIT_USAGE};

const TOOL: &str = "jimage";

fn help() {
    println!("Usage: jimage <extract | info | list | verify> <options> jimage...");
    println!();
    println!("  extract  - Extract all jimage entries and place in a directory specified");
    println!("             by the --dir=<directory> (default=.) option.");
    println!();
    println!("  info     - Prints detailed information contained in the jimage header.");
    println!();
    println!("  list     - Prints the names of all the entries in the jimage.  When used with");
    println!("             --verbose, list will also print entry size and offset attributes.");
    println!();
    println!("  verify   - Reports on any .class entries that dont verify as classes.");
    println!();
    println!("Possible options include:");
    println!("          --dir                        Target directory for extract directive");
    println!("  -?, -h, --help                       Print this help message");
    println!("          --include <pattern-list>     Pattern list for filtering entries.");
    println!("          --verbose                    Listing prints entry size and offset");
    println!("                                       attributes");
    println!("          --version                    Print version information");
    println!();
    println!("For options requiring a <pattern-list>, the value will be a comma separated");
    println!("list of elements each using one the following forms:");
    println!("  <glob-pattern>");
    println!("  glob:<glob-pattern>");
    println!("  regex:<regex-pattern>     [本 crate 未链接正则引擎，给 regex: 会明确报错]");
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
    let takes: &[&str] = &["--dir", "--include"];
    let known: &[&str] = &["--dir", "--include", "--verbose", "--version", "-?", "-h", "--help"];
    if let Err(e) = args.reject_unknown(TOOL, known, takes) {
        eprintln!("{e}");
        std::process::exit(EXIT_USAGE);
    }
    let pos = match args.positionals(takes) {
        Ok(p) => p,
        Err(e) => usage_err(TOOL, &e),
    };
    if pos.is_empty() {
        usage_err(TOOL, "缺少动作（extract | info | list | verify）");
    }
    let action = pos[0].as_str();
    let files: Vec<String> = pos[1..].to_vec();
    if files.is_empty() {
        usage_err(TOOL, "缺少 jimage 文件参数");
    }
    let include: Vec<String> = match args.value("--include") {
        Ok(Some(v)) => split_list(&v),
        Ok(None) => Vec::new(),
        Err(e) => usage_err(TOOL, &e),
    };
    if vmless_tools::has_regex(&include) {
        die(TOOL, "--include 的 regex: 模式需要正则引擎，本实现只支持 glob（可用 glob:<pattern>）");
    }
    let verbose = args.flag("--verbose");

    match action {
        "extract" => {
            let dir = match args.value("--dir") {
                Ok(Some(v)) => v,
                Ok(None) => ".".to_string(),
                Err(e) => usage_err(TOOL, &e),
            };
            let mut failed = 0;
            for f in &files {
                if let Err(e) = do_extract(f, &dir, &include) {
                    eprintln!("{TOOL}: {e}");
                    failed += 1;
                }
            }
            if failed > 0 {
                std::process::exit(1);
            }
        }
        "info" => {
            for f in &files {
                if let Err(e) = do_info(f) {
                    eprintln!("{TOOL}: {e}");
                    std::process::exit(1);
                }
            }
        }
        "list" => {
            for f in &files {
                if let Err(e) = do_list(f, &include, verbose) {
                    eprintln!("{TOOL}: {e}");
                    std::process::exit(1);
                }
            }
        }
        "verify" => {
            let mut bad = 0;
            for f in &files {
                match do_verify(f) {
                    Ok(n) => {
                        println!("{TOOL}: verified {n} entries in {f}");
                    }
                    Err(e) => {
                        eprintln!("{TOOL}: {e}");
                        bad += 1;
                    }
                }
            }
            if bad > 0 {
                std::process::exit(1);
            }
        }
        other => usage_err(TOOL, &format!("不认识的动作 {other}（extract | info | list | verify）")),
    }
}

fn do_list(path: &str, include: &[String], verbose: bool) -> Result<(), String> {
    let img = Image::open(path)?;
    println!("jimage: {}", img.path.display());
    let mut last_module: Option<String> = None;
    // 真 JDK 的 list 按模块分组输出：同模块连续打印，遇到新模块才重打 "Module:"
    let mut entries: Vec<_> = img.entries().iter().filter(|e| any_match(include, &e.name)).collect();
    entries.sort_by(|a, b| a.module.cmp(&b.module).then_with(|| a.rel_path().cmp(&b.rel_path())));
    for e in entries {
        if last_module.as_deref() != Some(e.module.as_str()) {
            println!();
            println!("Module: {}", e.module);
            if verbose {
                println!("Offset       Size       Compressed Entry");
            }
            last_module = Some(e.module.clone());
        }
        if verbose {
            println!(
                "{:12} {:10} {:10} {}",
                e.abs_offset,
                e.uncompressed,
                e.compressed,
                e.rel_path()
            );
        } else {
            println!("    {}", e.rel_path());
        }
    }
    Ok(())
}

fn do_info(path: &str) -> Result<(), String> {
    let img = Image::open(path)?;
    let h = &img.header;
    println!(" Major Version:  {}", h.major_version);
    println!(" Minor Version:  {}", h.minor_version);
    println!(" Flags:          {:x}", h.flags);
    println!(" Resource Count: {}", h.resource_count);
    println!(" Table Length:   {}", h.table_length);
    println!(" Offsets Size:   {}", h.table_length as u64 * 4);
    println!(" Redirects Size: {}", h.table_length as u64 * 4);
    println!(" Locations Size: {}", h.locations_size);
    println!(" Strings Size:   {}", h.strings_size);
    println!(" Index Size:     {}", h.index_size);
    Ok(())
}

fn do_extract(path: &str, dir: &str, include: &[String]) -> Result<(), String> {
    let mut img = Image::open(path)?;
    let out_root = std::path::Path::new(dir);
    mkdirs(out_root)?;
    // 先拍一份要解的条目（读的时候要 &mut img）
    let todo: Vec<vmless_tools::jimagex::Entry> = img
        .entries()
        .iter()
        .filter(|e| any_match(include, &e.name))
        .cloned()
        .collect();
    let total = todo.len();
    let mut done = 0;
    let mut bytes_total: u64 = 0;
    for e in &todo {
        let bytes = img.read(e)?;
        bytes_total += bytes.len() as u64;
        let rel = e.rel_path();
        if rel.contains("..") {
            return Err(format!("条目名非法（含 ..）：{}", e.name));
        }
        // 输出布局与真 jimage 一致：<dir>/<module>/<模块内相对路径>
        let mut target = out_root.to_path_buf();
        if !e.module.is_empty() {
            target.push(&e.module);
        }
        for seg in rel.split('/') {
            if seg.is_empty() || seg == "." {
                continue;
            }
            target.push(seg);
        }
        if let Some(parent) = target.parent() {
            mkdirs(parent)?;
        }
        std::fs::write(&target, &bytes).map_err(|err| format!("写 {} 失败: {err}", target.display()))?;
        // 可执行权限在 Windows 上无意义；解出来的 bin/*.dll 不需要额外处理
        done += 1;
    }
    println!("{TOOL}: extracted {done}/{total} entries, {bytes_total} bytes, into {dir}");
    Ok(())
}

fn do_verify(path: &str) -> Result<usize, String> {
    let mut img = Image::open(path)?;
    let (checked, problems) = img.verify_layout();
    // 结构校验过了再逐条把 .class 解析一遍（真 jimage verify 做的正是这件事）
    let classes: Vec<vmless_tools::jimagex::Entry> = img
        .entries()
        .iter()
        .filter(|e| e.name.ends_with(".class"))
        .cloned()
        .collect();
    let mut bad = problems.len();
    for p in &problems {
        println!("{p}");
    }
    for e in &classes {
        match img.read(e) {
            Ok(bytes) => {
                if let Err(err) = classfile::parse(&bytes) {
                    println!("{}: 不是合法的 class 文件（{err}）", e.name);
                    bad += 1;
                }
            }
            Err(err) => {
                println!("{}: {err}", e.name);
                bad += 1;
            }
        }
    }
    if bad > 0 {
        return Err(format!("{bad} 处问题（共检查 {checked} 条索引、{} 个 class）", classes.len()));
    }
    Ok(checked)
}
