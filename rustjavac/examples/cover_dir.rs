//! 跑一棵目录树里的所有 .java，走**完整编译流水线**，统计通过率并把失败原因聚成桶。
//!
//! 用法: cover_dir <源码目录> [classpath目录...] [--detail <明细TSV>]
//!
//! 与 compile_src 的区别：compile_src 是单文件、人工看；这个是批量统计，
//! 失败按「归一化后的错误消息」分桶，直接给出 top-N 分布。

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

/// 把错误消息归一化成桶键：反引号里的具体名字、数字、路径都抹掉，
/// 只留「骨架」，这样 `未找到方法 foo` / `未找到方法 bar` 落进同一个桶。
fn bucket_key(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut in_tick = false;
    let mut prev_space = false;
    for ch in msg.chars() {
        match ch {
            '`' => {
                in_tick = !in_tick;
                if !in_tick {
                    out.push('X');
                }
            }
            _ if in_tick => {}
            c if c.is_ascii_digit() => {
                if !out.ends_with('N') {
                    out.push('N');
                }
            }
            c => {
                let c = if c == '\n' { ' ' } else { c };
                if c == ' ' {
                    if prev_space {
                        continue;
                    }
                    prev_space = true;
                } else {
                    prev_space = false;
                }
                out.push(c);
            }
        }
    }
    // 单引号里的类型名也抹掉
    let mut s = String::with_capacity(out.len());
    let mut in_quote = false;
    let mut buf = String::new();
    for ch in out.chars() {
        if ch == '\'' {
            if in_quote {
                s.push('Q');
                buf.clear();
            }
            in_quote = !in_quote;
            continue;
        }
        if in_quote {
            buf.push(ch);
        } else {
            s.push(ch);
        }
    }
    if in_quote {
        s.push_str(&buf);
    }
    let s = s.trim().to_string();
    if s.chars().count() > 110 {
        s.chars().take(110).collect::<String>() + "…"
    } else {
        s
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("用法: cover_dir <源码目录> [cp...] [--detail <tsv>]");
    let mut cps: Vec<PathBuf> = Vec::new();
    let mut detail_path: Option<String> = None;
    let mut rest: Vec<String> = args.collect();
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == "--detail" {
            if i + 1 < rest.len() {
                detail_path = Some(rest[i + 1].clone());
                rest.drain(i..i + 2);
                continue;
            }
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
    println!(
        "文件 {} 个：完整编译通过 {}，编译失败 {}，解析失败 {}，通过率 {:.1}%，用时 {:?}",
        files.len(),
        n_compile_ok,
        n_compile_fail,
        n_parse_fail,
        (n_compile_ok as f64) * 100.0 / total as f64,
        t.elapsed()
    );

    let mut rows: Vec<(String, usize, Vec<String>)> =
        buckets.into_iter().map(|(k, (n, s))| (k, n, s)).collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let nfail = (n_parse_fail + n_compile_fail).max(1);
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

    if let Some(p) = detail_path {
        if let Some(d) = Path::new(&p).parent() {
            let _ = std::fs::create_dir_all(d);
        }
        match std::fs::File::create(&p) {
            Ok(mut f) => {
                let _ = f.write_all(detail.as_bytes());
                println!("明细已写 {p}（{} 行）", detail.lines().count());
            }
            Err(e) => println!("明细写失败 {p}: {e}"),
        }
    }
}
