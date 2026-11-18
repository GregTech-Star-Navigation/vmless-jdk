//! `jar` —— jar 包（zip 容器）的建 / 列 / 解 / 更新。
//!
//! 认真 jar 的两套写法：
//! ```text
//! jar cf  <out.jar> [-C dir] <files...>            # 建
//! jar cfm <out.jar> <manifest> [-C dir] <files...> # 建 + 清单
//! jar tf  <jar> [条目...]                          # 列
//! jar xf  <jar> [条目...]                          # 解
//! jar uf  <jar> [-C dir] <files...>                # 更新
//! jar --create --file X --main-class p.B -C dir .
//! ```
//!
//! **双向互通**：写的是 stored / deflate（RFC 1951 固定 Huffman + CRC32 + 中央目录），
//! 真 `java -jar` / `jar tf` 直接能读；读用的是 `ZipReader`（动态 Huffman、data
//! descriptor 都支持，大小按中央目录为准），所以真 jar 打出来的包也能改、能解。
//! 清单自动带 `Manifest-Version: 1.0`，`-m` 给的键与已有清单**合并**（后给覆盖先给）。
//!
//! 边界：不写 zip64、不写条目注释/扩展时间戳；非 ASCII 条目名不带 UTF-8 标志位
//! （真 java 会按 Cp437 解释——Java 类名是 ASCII，正常打包不受影响）。

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use vmless_tools::zipx::{ZipReader, ZipWriter};
use vmless_tools::{die, mkdirs, EXIT_USAGE};

const TOOL: &str = "jar";
const MF_DIR: &str = "META-INF/";
const MF_ENTRY: &str = "META-INF/MANIFEST.MF";
const MF_VERSION: &str = "Manifest-Version";

fn help() {
    println!("用法: jar [OPTION...] [ [-C dir] files] ...");
    println!();
    println!("jar 建包、列包、解包、更新包（容器是 zip）。");
    println!();
    println!("示例:");
    println!(" # 把 jart/out 下的所有文件打进 app.jar（条目名相对 jart/out）:");
    println!(" jar cf app.jar -C jart/out .");
    println!(" # 带清单建包（m.txt 里的 Main-Class 会进 META-INF/MANIFEST.MF）:");
    println!(" jar cfm app.jar m.txt -C jart/out .");
    println!(" # 列 / 解 / 更新:");
    println!(" jar tf app.jar");
    println!(" jar xf app.jar");
    println!(" jar uf app.jar -C jart/out p/B.class");
    println!();
    println!("主要模式:");
    println!("  -c, --create          建包（缺省目录会自动建）");
    println!("  -t, --list            列出条目");
    println!("  -x, --extract         解包（-C 指定解到哪，默认当前目录）");
    println!("  -u, --update          更新已有包");
    println!();
    println!("建包/更新模式有效的选项:");
    println!("  -C DIR                以 DIR 为基准（后面的 files 相对它；`.` 表示 DIR 本身）");
    println!("  -m, --manifest=FILE   用这份清单（键与已有清单合并，后给覆盖先给）");
    println!("      --main-class=C    写 Main-Class 进清单");
    println!("  -0..-9                压缩级别：0 = 不压缩，1-9 = deflate");
    println!();
    println!("两模式通用:");
    println!("  -f, --file=FILE       包文件名");
    println!("  -v, --verbose         打印每一步（added manifest / creating: / inflating:）");
    println!("  -h, --help            打印帮助");
    println!("      --version         版本信息");
    println!();
    println!("清单总是以 META-INF/MANIFEST.MF 进包，自动带 Manifest-Version: 1.0。");
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Create,
    List,
    Extract,
    Update,
}

/// `-C base name` 展开后的一项
struct Spec {
    base: PathBuf,
    name: String,
}

struct Opts {
    mode: Mode,
    file: Option<String>,
    manifest: Option<String>,
    main_class: Option<String>,
    verbose: bool,
    compress: String,
    /// 解包输出目录（`-C`，默认 `.`）
    out_dir: PathBuf,
    specs: Vec<Spec>,
    names: Vec<String>,
}

fn main() {
    vmless_tools::install_pipe_guard();
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.is_empty() || matches!(raw[0].as_str(), "-h" | "-help" | "--help" | "-?") {
        help();
        return;
    }
    if matches!(raw[0].as_str(), "--version" | "-version") {
        vmless_tools::print_version(TOOL, "25");
        return;
    }
    let opts = match parse_args(&raw) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{TOOL}: {e}");
            eprintln!("使用 {TOOL} --help 查看用法");
            std::process::exit(EXIT_USAGE);
        }
    };
    let r = match opts.mode {
        Mode::Create => create(&opts),
        Mode::List => list(&opts),
        Mode::Extract => extract(&opts),
        Mode::Update => update(&opts),
    };
    if let Err(e) = r {
        die(TOOL, &e);
    }
}

// ------------------------------------------------------------------ 参数

fn parse_args(raw: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        mode: Mode::Create,
        file: None,
        manifest: None,
        main_class: None,
        verbose: false,
        compress: "zip-6".to_string(),
        out_dir: PathBuf::from("."),
        specs: Vec::new(),
        names: Vec::new(),
    };
    let first = &raw[0];
    let mut rest: Vec<String> = Vec::new();
    if first.starts_with("--") || matches!(first.as_str(), "-c" | "-t" | "-x" | "-u") {
        // 长选项形态
        let mut mode: Option<Mode> = None;
        let mut i = 0;
        let mut base = PathBuf::from(".");
        let mut saw_c = false;
        while i < raw.len() {
            let a = raw[i].as_str();
            match a {
                "--create" | "-c" => mode = Some(Mode::Create),
                "--list" | "-t" => mode = Some(Mode::List),
                "--extract" | "-x" => mode = Some(Mode::Extract),
                "--update" | "-u" => mode = Some(Mode::Update),
                "-v" | "--verbose" => o.verbose = true,
                "--file" | "-f" => {
                    i += 1;
                    o.file = Some(take(raw, i, a)?);
                }
                "--manifest" | "-m" => {
                    i += 1;
                    o.manifest = Some(take(raw, i, a)?);
                }
                "--main-class" => {
                    i += 1;
                    o.main_class = Some(take(raw, i, a)?);
                }
                "-C" => {
                    i += 1;
                    let d = take(raw, i, a)?;
                    if !Path::new(&d).is_dir() {
                        return Err(format!("-C 的目录不存在: {d}"));
                    }
                    base = PathBuf::from(d);
                    saw_c = true;
                }
                other if other.starts_with("--file=") => o.file = Some(other["--file=".len()..].to_string()),
                other if other.starts_with("--manifest=") => {
                    o.manifest = Some(other["--manifest=".len()..].to_string())
                }
                other if other.starts_with("--main-class=") => {
                    o.main_class = Some(other["--main-class=".len()..].to_string())
                }
                other if other.starts_with('-') && other.len() > 1 => {
                    return Err(format!("不认识的选项 {other}"))
                }
                other => o.specs.push(Spec { base: base.clone(), name: other.to_string() }),
            }
            i += 1;
        }
        o.mode = mode.ok_or("缺少模式（--create / --list / --extract / --update）")?;
        match o.mode {
            // 列/解模式的位置参数是条目名；`-C` 是解包输出目录
            Mode::List | Mode::Extract => {
                o.names = o.specs.drain(..).map(|s| s.name).collect();
                if saw_c {
                    o.out_dir = base;
                }
            }
            _ => {
                if o.specs.is_empty() {
                    return Err("要打包的文件一个都没给".into());
                }
            }
        }
    } else if first.starts_with('-') {
        return Err(format!("第一个参数应该是模式（如 cf / tf / xf / uf），收到 {first}"));
    } else {
        // 紧凑形态：`cfm out.jar m.txt -C dir .` —— 字母按出现顺序吃掉后面的位置参数
        let mut mode: Option<Mode> = None;
        let mut i = 1;
        for ch in first.chars() {
            match ch {
                'c' => mode = Some(Mode::Create),
                't' => mode = Some(Mode::List),
                'x' => mode = Some(Mode::Extract),
                'u' => mode = Some(Mode::Update),
                'f' => {
                    o.file = Some(take(raw, i, "-f")?);
                    i += 1;
                }
                'm' => {
                    o.manifest = Some(take(raw, i, "-m")?);
                    i += 1;
                }
                'v' => o.verbose = true,
                // 0 = stored，其余都是同一个 deflate 编码器
                d @ '0'..='9' => {
                    o.compress = if d == '0' { "zip-0".into() } else { "zip-6".into() };
                }
                other => return Err(format!("不认识的模式字母 {other}（可用 c/t/x/u/f/m/v/0-9）")),
            }
        }
        o.mode = mode.ok_or("缺少模式字母（c / t / x / u）")?;
        rest = raw[i..].to_vec();
    }
    o.file = Some(o.file.ok_or("缺少包文件名（-f <jar> 或用 cf/tf/xf/uf 里的 f）")?);

    match o.mode {
        Mode::Create | Mode::Update => {
            // `-C` 在文件列表里可以反复出现，直到下一个 `-C` 之前的所有文件都以它为基准
            let mut base = PathBuf::new();
            let mut i = 0;
            while i < rest.len() {
                if rest[i] == "-C" {
                    i += 1;
                    let d = take(&rest, i, "-C")?;
                    if !Path::new(&d).is_dir() {
                        return Err(format!("-C 的目录不存在: {d}"));
                    }
                    base = PathBuf::from(d);
                } else if rest[i].starts_with('-') && rest[i].len() > 1 {
                    return Err(format!("不认识的选项 {}", rest[i]));
                } else {
                    o.specs.push(Spec { base: base.clone(), name: rest[i].clone() });
                }
                i += 1;
            }
            if o.specs.is_empty() {
                return Err("要打包的文件一个都没给".into());
            }
        }
        Mode::List | Mode::Extract => {
            let mut i = 0;
            while i < rest.len() {
                if rest[i] == "-C" {
                    i += 1;
                    o.out_dir = PathBuf::from(take(&rest, i, "-C")?);
                } else {
                    o.names.push(rest[i].clone());
                }
                i += 1;
            }
        }
    }
    Ok(o)
}

fn take(v: &[String], i: usize, opt: &str) -> Result<String, String> {
    v.get(i).cloned().ok_or_else(|| format!("选项 {opt} 后面缺少值"))
}

// ------------------------------------------------------------------ 清单

/// 解析清单：续行（前导空格）拼回去，`键: 值`，空行忽略
fn parse_manifest(bytes: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(cont) = line.strip_prefix(' ') {
            if let Some(last) = lines.last_mut() {
                last.push_str(cont);
                continue;
            }
        }
        if line.is_empty() {
            continue;
        }
        lines.push(line.to_string());
    }
    let mut kv: Vec<(String, String)> = Vec::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            upsert(&mut kv, k.trim().to_string(), v.trim().to_string());
        }
    }
    kv
}

fn upsert(kv: &mut Vec<(String, String)>, k: String, v: String) {
    if let Some(slot) = kv.iter_mut().find(|(kk, _)| kk.eq_ignore_ascii_case(&k)) {
        slot.1 = v;
    } else {
        kv.push((k, v));
    }
}

/// 清单字节：`Manifest-Version` 必须第一条，行尾 CRLF，末尾一个空行
fn manifest_bytes(kv: &[(String, String)]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut has_version = false;
    for (k, v) in kv {
        if k.eq_ignore_ascii_case(MF_VERSION) {
            has_version = true;
        }
        out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    if !has_version {
        let mut head = format!("{MF_VERSION}: 1.0\r\n").into_bytes();
        head.extend_from_slice(&out);
        out = head;
    }
    out.extend_from_slice(b"\r\n");
    out
}

fn merge_manifest_file(kv: &mut Vec<(String, String)>, path: &str) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("读清单 {path} 失败: {e}"))?;
    for (k, v) in parse_manifest(&bytes) {
        upsert(kv, k, v);
    }
    Ok(())
}

fn ensure_version(kv: &mut Vec<(String, String)>) {
    if !kv.iter().any(|(k, _)| k.eq_ignore_ascii_case(MF_VERSION)) {
        kv.insert(0, (MF_VERSION.to_string(), "1.0".to_string()));
    }
}

// ------------------------------------------------------------------ 条目收集

/// `p\A.class` → `p/A.class`；去掉 `./` 前缀；绝对路径只留文件名
fn entry_name(name: &str) -> String {
    let p = Path::new(name);
    if p.is_absolute() {
        return p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    }
    let s = name.replace('\\', "/");
    let s = s.trim_start_matches("./").to_string();
    s.trim_end_matches('/').to_string()
}

fn push_dir(out: &mut Vec<(String, Vec<u8>)>, seen: &mut HashSet<String>, name: &str) {
    if seen.insert(name.to_string()) {
        out.push((name.to_string(), Vec::new()));
    }
}

fn push_file(
    out: &mut Vec<(String, Vec<u8>)>,
    seen: &mut HashSet<String>,
    name: &str,
    data: Vec<u8>,
    kv: &mut Vec<(String, String)>,
) {
    // 用户自己给的 META-INF/MANIFEST.MF：并进清单，不重复进包（真 jar 也是这样）
    if name.eq_ignore_ascii_case(MF_ENTRY) {
        for (k, v) in parse_manifest(&data) {
            upsert(kv, k, v);
        }
        return;
    }
    if seen.insert(name.to_string()) {
        out.push((name.to_string(), data));
        return;
    }
    // 同名后者覆盖（与 zip 的「后写的赢」一致）
    if let Some(slot) = out.iter_mut().find(|(n, _)| n == name) {
        slot.1 = data;
    }
}

/// 递归一个目录：目录自己也成条目（`p/`），文件按相对路径进包
fn walk_dir(
    dir: &Path,
    prefix: &str,
    out: &mut Vec<(String, Vec<u8>)>,
    seen: &mut HashSet<String>,
    kv: &mut Vec<(String, String)>,
    depth: usize,
) -> Result<(), String> {
    if depth > 64 {
        return Err(format!("目录层级超过 64 层，可能有符号链接环: {}", dir.display()));
    }
    let rd = std::fs::read_dir(dir).map_err(|e| format!("读目录 {} 失败: {e}", dir.display()))?;
    let mut items: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    items.sort();
    for p in items {
        let Some(fname) = p.file_name().map(|s| s.to_string_lossy().into_owned()) else { continue };
        let name = if prefix.is_empty() { fname } else { format!("{prefix}/{fname}") };
        let md = match std::fs::symlink_metadata(&p) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if md.is_dir() {
            push_dir(out, seen, &format!("{name}/"));
            walk_dir(&p, &name, out, seen, kv, depth + 1)?;
        } else if md.is_file() {
            let data = std::fs::read(&p).map_err(|e| format!("读 {} 失败: {e}", p.display()))?;
            push_file(out, seen, &name, data, kv);
        }
    }
    Ok(())
}

fn collect_entries(
    specs: &[Spec],
    kv: &mut Vec<(String, String)>,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for s in specs {
        let path = if s.base.as_os_str().is_empty() { PathBuf::from(&s.name) } else { s.base.join(&s.name) };
        if !path.exists() {
            return Err(format!("找不到文件: {}", path.display()));
        }
        // `.` = 基准目录本身：条目名不带前缀（`-C dir .` 的常见形态）
        let prefix = if s.name == "." { String::new() } else { entry_name(&s.name) };
        if path.is_dir() {
            if !prefix.is_empty() {
                push_dir(&mut out, &mut seen, &format!("{prefix}/"));
            }
            walk_dir(&path, &prefix, &mut out, &mut seen, kv, 0)?;
        } else {
            let data = std::fs::read(&path).map_err(|e| format!("读 {} 失败: {e}", path.display()))?;
            push_file(&mut out, &mut seen, &prefix, data, kv);
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------ 各模式

/// 写包：条目按给定顺序；清单条目本身不打印（用 `added manifest` 代替）
fn write_jar(
    file: &str,
    compress: &str,
    entries: &[(String, Vec<u8>)],
    verbose: bool,
) -> Result<usize, String> {
    let mut w = ZipWriter::new(compress)?;
    let mut total = 0usize;
    for (name, data) in entries {
        w.add(name, data)?;
        total += data.len();
        if verbose && name != MF_DIR && name != MF_ENTRY {
            if name.ends_with('/') {
                println!("  creating: {name}");
            } else {
                println!("  adding: {name}");
            }
        }
    }
    let bytes = w.finish()?;
    let p = Path::new(file);
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            mkdirs(parent)?;
        }
    }
    std::fs::write(p, &bytes).map_err(|e| format!("写 {} 失败: {e}", p.display()))?;
    Ok(total)
}

fn create(o: &Opts) -> Result<(), String> {
    let file = o.file.as_deref().unwrap_or_default();
    let mut kv: Vec<(String, String)> = Vec::new();
    if let Some(m) = &o.manifest {
        merge_manifest_file(&mut kv, m)?;
    }
    // 先收文件：用户自己给的 `META-INF/MANIFEST.MF` 会在这一步被并进 `kv`
    let files = collect_entries(&o.specs, &mut kv)?;
    // `--main-class` 最后写，保证它压过输入清单里的同名键
    if let Some(mc) = &o.main_class {
        upsert(&mut kv, "Main-Class".to_string(), mc.clone());
    }
    ensure_version(&mut kv);
    if o.verbose {
        println!("added manifest");
    }
    let mut entries: Vec<(String, Vec<u8>)> =
        vec![(MF_DIR.to_string(), Vec::new()), (MF_ENTRY.to_string(), manifest_bytes(&kv))];
    entries.extend(files);
    write_jar(file, &o.compress, &entries, o.verbose)?;
    Ok(())
}

fn list(o: &Opts) -> Result<(), String> {
    let file = o.file.as_deref().unwrap_or_default();
    let z = ZipReader::open(file)?;
    let stdout = std::io::stdout();
    let mut w = stdout.lock();
    for e in &z.entries {
        if !o.names.is_empty() && !o.names.iter().any(|n| *n == e.name) {
            continue;
        }
        writeln!(w, "{}", e.name).map_err(|e| format!("写 stdout 失败: {e}"))?;
    }
    Ok(())
}

fn extract(o: &Opts) -> Result<(), String> {
    let file = o.file.as_deref().unwrap_or_default();
    let z = ZipReader::open(file)?;
    mkdirs(&o.out_dir)?;
    for e in &z.entries {
        if !o.names.is_empty() && !o.names.iter().any(|n| *n == e.name) {
            continue;
        }
        let mut dest = o.out_dir.clone();
        for seg in e.name.split('/') {
            if seg.is_empty() {
                continue;
            }
            // zip-slip：条目名里的 `..` 不许把文件写到解包目录外面
            if seg == ".." {
                return Err(format!("条目 {} 含 `..`，拒绝解包（防目录穿越）", e.name));
            }
            dest.push(seg);
        }
        if e.name.ends_with('/') {
            mkdirs(&dest)?;
            if o.verbose {
                println!("  creating: {}", e.name);
            }
            continue;
        }
        if let Some(parent) = dest.parent() {
            mkdirs(parent)?;
        }
        let data = z.read(e)?;
        // 目录条目之外，正常文件：stored 的写 `extracting`、deflate 的写 `inflating`
        std::fs::write(&dest, &data).map_err(|e| format!("写 {} 失败: {e}", dest.display()))?;
        if o.verbose {
            if e.method == 8 {
                println!("  inflating: {}", e.name);
            } else {
                println!("  extracting: {}", e.name);
            }
        }
    }
    Ok(())
}

fn update(o: &Opts) -> Result<(), String> {
    let file = o.file.as_deref().unwrap_or_default();
    let z = ZipReader::open(file)?;
    let mut kv: Vec<(String, String)> = Vec::new();
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    for e in &z.entries {
        if e.name == MF_DIR || e.name == MF_ENTRY {
            if e.name == MF_ENTRY {
                kv = parse_manifest(&z.read(e)?);
            }
            continue;
        }
        let data = if e.name.ends_with('/') { Vec::new() } else { z.read(e)? };
        entries.push((e.name.clone(), data));
    }
    if let Some(m) = &o.manifest {
        merge_manifest_file(&mut kv, m)?;
    }
    let new = collect_entries(&o.specs, &mut kv)?;
    if let Some(mc) = &o.main_class {
        upsert(&mut kv, "Main-Class".to_string(), mc.clone());
    }
    ensure_version(&mut kv);
    for (name, data) in new {
        if let Some(slot) = entries.iter_mut().find(|(n, _)| *n == name) {
            slot.1 = data;
        } else {
            entries.push((name, data));
        }
    }
    if o.verbose {
        println!("added manifest");
    }
    let mut all: Vec<(String, Vec<u8>)> =
        vec![(MF_DIR.to_string(), Vec::new()), (MF_ENTRY.to_string(), manifest_bytes(&kv))];
    all.extend(entries);
    write_jar(file, &o.compress, &all, o.verbose)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_roundtrip_keeps_main_class() {
        let kv = parse_manifest(b"Manifest-Version: 1.0\r\nMain-Class: p.B\r\n\r\n");
        let bytes = manifest_bytes(&kv);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("Manifest-Version: 1.0\r\n"));
        assert!(text.contains("Main-Class: p.B\r\n"));
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn manifest_upsert_is_case_insensitive() {
        let mut kv = parse_manifest(b"Main-Class: a.B\r\n");
        upsert(&mut kv, "main-class".to_string(), "c.D".to_string());
        let text = String::from_utf8(manifest_bytes(&kv)).unwrap();
        assert!(text.contains("Main-Class: c.D"), "{text}");
        assert!(!text.contains("a.B"), "{text}");
        assert!(text.starts_with("Manifest-Version: 1.0"), "{text}");
    }

    #[test]
    fn entry_names_normalize() {
        assert_eq!(entry_name("./p/A.class"), "p/A.class");
        assert_eq!(entry_name("p\\A.class"), "p/A.class");
        assert_eq!(entry_name("p/"), "p");
    }
}
