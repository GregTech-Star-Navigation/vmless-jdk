//! 跑一棵目录树里的所有 `.java`，走**完整编译流水线**，统计通过率并把失败原因聚成桶。
//!
//! 用法: cover_dir <源码目录> [classpath目录...] [--detail <明细TSV>] [--md <报告MD>]
//!
//! 与 compile_src 的区别：compile_src 是单文件、人工看；这个是批量统计，
//! 失败按「归一化后的错误消息」分桶，直接给出 top-N 分布。
//!
//! 判据只看**这个文件能不能走到流水线末尾**（lex → parse → flatten → erase →
//! desugar → qualify → codegen），不写 class 文件。

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().and_then(|x| x.to_str()) == Some("java") {
                out.push(p);
            }
        }
    }
}

/// 把错误消息归一化成桶键：**具体是谁**全部抹掉，只留骨架。
///
/// - 反引号里的内容 → `X`
/// - 全角括号里的内容 → `X`
/// - 任何「含字母的 token」（标识符、点分名、`L23`）→ `X`
/// - 纯数字 token → `N`
///
/// 于是「未找到实例方法 `size`」和「未找到实例方法 `add`」落进同一个桶。
fn bucket_key(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut it = msg.chars().peekable();
    let is_tok = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '.';
    while let Some(c) = it.next() {
        match c {
            '`' => {
                for d in it.by_ref() {
                    if d == '`' {
                        break;
                    }
                }
                if !out.ends_with('X') {
                    out.push('X');
                }
            }
            '（' => {
                for d in it.by_ref() {
                    if d == '）' {
                        break;
                    }
                }
                out.push_str("（X）");
            }
            c if is_tok(c) => {
                let mut tok = String::from(c);
                while let Some(&d) = it.peek() {
                    if is_tok(d) {
                        tok.push(d);
                        it.next();
                    } else {
                        break;
                    }
                }
                let t = if tok.chars().any(|x| x.is_ascii_alphabetic() || x == '_' || x == '$') {
                    'X'
                } else {
                    'N'
                };
                if !out.ends_with(t) {
                    out.push(t);
                }
            }
            c => {
                let c = if c == '\n' || c == '\t' { ' ' } else { c };
                if c == ' ' && out.ends_with(' ') {
                    continue;
                }
                out.push(c);
            }
        }
    }
    let s = out.trim().to_string();
    if s.chars().count() > 100 {
        s.chars().take(100).collect::<String>() + "…"
    } else {
        s
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("用法: cover_dir <源码目录> [cp...] [--detail <tsv>] [--md <md>]");
    let mut cps: Vec<PathBuf> = Vec::new();
    let mut detail_path: Option<String> = None;
    let mut md_path: Option<String> = None;
    let mut rest: Vec<String> = args.collect();
    let mut i = 0;
    while i < rest.len() {
        if (rest[i] == "--detail" || rest[i] == "--md") && i + 1 < rest.len() {
            let v = rest[i + 1].clone();
            if rest[i] == "--detail" {
                detail_path = Some(v);
            } else {
                md_path = Some(v);
            }
            rest.drain(i..i + 2);
            continue;
        }
        i += 1;
    }
    for r in rest {
        cps.push(PathBuf::from(r));
    }

    let mut files = Vec::new();
    walk(Path::new(&dir), &mut files);
    files.sort();

    let mut n_parse_fail = 0usize;
    let mut n_compile_ok = 0usize;
    let mut n_compile_fail = 0usize;
    // 桶键 → (次数, 样例文件名 3 个)
    let mut buckets: HashMap<String, (usize, Vec<String>)> = HashMap::new();
    let mut detail = String::new();
    let t = std::time::Instant::now();

    for f in &files {
        let src = match std::fs::read_to_string(f) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let rel = f
            .strip_prefix(Path::new(&dir))
            .unwrap_or(f)
            .to_string_lossy()
            .replace('\\', "/");
        let (stage, msg) = match classfile::parser::parse_compilation_unit(&src) {
            Err(e) => ("parse", format!("L{} {}", e.line, e.msg)),
            Ok(cu) => match classfile::compiler::compile_with_path(&cu, &cps) {
                Ok(_) => {
                    n_compile_ok += 1;
                    continue;
                }
                Err(e) => ("compile", e.msg),
            },
        };
        if stage == "parse" {
            n_parse_fail += 1;
        } else {
            n_compile_fail += 1;
        }
        let key = format!("[{stage}] {}", bucket_key(&msg));
        let ent = buckets.entry(key).or_insert((0, Vec::new()));
        ent.0 += 1;
        if ent.1.len() < 3 {
            ent.1.push(rel.clone());
        }
        detail.push_str(&format!("{rel}\t{stage}\t{msg}\n"));
    }

    let total = files.len().max(1);
    let nfail = (n_parse_fail + n_compile_fail).max(1);
    let rate = (n_compile_ok as f64) * 100.0 / total as f64;
    println!(
        "文件 {} 个：完整编译通过 {}，编译失败 {}，解析失败 {}，通过率 {:.1}%，用时 {:?}",
        files.len(),
        n_compile_ok,
        n_compile_fail,
        n_parse_fail,
        rate,
        t.elapsed()
    );

    let mut rows: Vec<(String, usize, Vec<String>)> =
        buckets.into_iter().map(|(k, (n, s))| (k, n, s)).collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    println!("\n{:<3} {:<6} {:<7} {}", "#", "次数", "占失败比", "错误骨架 / 样例");
    for (i, (k, n, samples)) in rows.iter().enumerate() {
        println!(
            "{:<3} {:<6} {:>6.1}% {}   « {}",
            i + 1,
            n,
            (*n as f64) * 100.0 / nfail as f64,
            k,
            samples.join(", ")
        );
    }
    println!("\n桶数 {}", rows.len());

    if let Some(p) = &detail_path {
        if let Some(d) = Path::new(p).parent() {
            let _ = std::fs::create_dir_all(d);
        }
        match std::fs::File::create(p) {
            Ok(mut f) => {
                let _ = f.write_all(detail.as_bytes());
                println!("明细已写 {p}（{} 行）", detail.lines().count());
            }
            Err(e) => println!("明细写失败 {p}: {e}"),
        }
    }

    if let Some(p) = &md_path {
        let mut md = String::new();
        md.push_str("# java.base 编译失败分布\n\n");
        md.push_str("> 由 `examples/cover_dir.rs` 生成，改完代码重跑即可刷新。\n\n");
        md.push_str(&format!("- 源码根：`{dir}`\n"));
        md.push_str(&format!(
            "- classpath：{}\n",
            if cps.is_empty() {
                "（无）".to_string()
            } else {
                cps.iter().map(|p| format!("`{}`", p.display())).collect::<Vec<_>>().join(", ")
            }
        ));
        md.push_str(&format!("- 生成时间：{}\n\n", now_iso()));
        md.push_str("## 总览\n\n");
        md.push_str("| 文件总数 | 通过 | 编译失败 | 解析失败 | 通过率 |\n|---|---|---|---|---|\n");
        md.push_str(&format!(
            "| {} | {} | {} | {} | **{:.1}%** |\n\n",
            files.len(),
            n_compile_ok,
            n_compile_fail,
            n_parse_fail,
            rate
        ));
        md.push_str("## 失败桶\n\n");
        md.push_str("| # | 次数 | 占失败比 | 错误骨架 | 样例文件 |\n|---|---|---|---|---|\n");
        for (i, (k, n, samples)) in rows.iter().enumerate() {
            md.push_str(&format!(
                "| {} | {} | {:.1}% | `{}` | {} |\n",
                i + 1,
                n,
                (*n as f64) * 100.0 / nfail as f64,
                k.replace('|', "\\|"),
                samples.iter().map(|s| format!("`{s}`")).collect::<Vec<_>>().join("<br>")
            ));
        }
        md.push_str(&format!("\n共 {} 个桶。\n", rows.len()));
        if let Some(d) = Path::new(p).parent() {
            let _ = std::fs::create_dir_all(d);
        }
        match std::fs::File::create(p) {
            Ok(mut f) => {
                let _ = f.write_all(md.as_bytes());
                println!("报告已写 {p}");
            }
            Err(e) => println!("报告写失败 {p}: {e}"),
        }
    }
}

/// 报告里的时间戳（不引第三方 crate，手动按 UTC+8 拼）
fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() + 8 * 3600)
        .unwrap_or(0);
    let (mut days, rem) = ((secs / 86400) as i64, secs % 86400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // 1970-01-01 起的天数 → 年月日
    let mut y = 1970i64;
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let dy = if leap { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let mdays = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m = 0usize;
    while days >= mdays[m] {
        days -= mdays[m];
        m += 1;
    }
    format!("{y}-{:02}-{:02} {h:02}:{mi:02}:{s:02} +08:00", m + 1, days + 1)
}
