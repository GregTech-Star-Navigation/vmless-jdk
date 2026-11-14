//! 跑一棵目录树里的所有 .java，统计语法分析成功率。
use std::path::{Path, PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() { walk(&p, out); }
            else if p.extension().and_then(|x| x.to_str()) == Some("java") { out.push(p); }
        }
    }
}

fn main() {
    let dir = std::env::args().nth(1).expect("用法: parse_dir <目录>");
    let mut files = Vec::new();
    walk(Path::new(&dir), &mut files);
    let (mut ok, mut bad) = (0usize, 0usize);
    let mut fails = Vec::new();
    let t = std::time::Instant::now();
    for f in &files {
        let src = match std::fs::read_to_string(f) { Ok(s) => s, Err(_) => continue };
        match classfile::parser::parse_compilation_unit(&src) {
            Ok(_) => ok += 1,
            Err(e) => { bad += 1; if fails.len() < 8 { fails.push(format!("{}:{} {}", f.file_name().unwrap().to_string_lossy(), e.line, e.msg)); } }
        }
    }
    for f in &fails { println!("  失败 {f}"); }
    println!("java 文件 {} 个：成功 {ok}，失败 {bad}，成功率 {:.1}%，用时 {:?}",
        files.len(), (ok as f64) * 100.0 / (files.len().max(1)) as f64, t.elapsed());
}
