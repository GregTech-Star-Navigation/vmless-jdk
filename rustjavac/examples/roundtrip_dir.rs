//! 对一棵目录树里的所有 .class 做"读入→写出→比对"。
use std::path::{Path, PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().and_then(|x| x.to_str()) == Some("class") {
                out.push(p);
            }
        }
    }
}

fn main() {
    let dir = std::env::args().nth(1).expect("用法: roundtrip_dir <目录>");
    let mut files = Vec::new();
    walk(Path::new(&dir), &mut files);
    let (mut ok, mut bad, mut err) = (0usize, 0usize, 0usize);
    let t = std::time::Instant::now();
    for f in &files {
        let bytes = std::fs::read(f).expect("read");
        match classfile::parse(&bytes) {
            Ok(cf) => {
                if classfile::write(&cf) == bytes { ok += 1 } else { bad += 1; if bad <= 3 { println!("不一致: {}", f.display()); } }
            }
            Err(e) => { err += 1; if err <= 3 { println!("解析失败 {}: {e}", f.display()); } }
        }
    }
    println!("文件 {} 个：一致 {ok}，不一致 {bad}，解析失败 {err}，用时 {:?}", files.len(), t.elapsed());
}
