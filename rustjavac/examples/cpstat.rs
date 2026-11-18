//! 一次性统计：内存里到底被什么吃掉了（临时工具，做完就删）
use classfile::CpEntry;
fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| "D:/vmless/patch/img/java.base".into());
    let mut files = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(&root)];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().and_then(|s| s.to_str()) == Some("class") {
                    files.push(p);
                }
            }
        }
    }
    let (mut ncp, mut nutf, mut nutf_bytes, mut nattr, mut nattr_bytes, mut nmeth, mut nfld, mut raw) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    let mut parse_us = std::time::Duration::ZERO;
    let mut utf8_seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    for f in &files {
        let bytes = std::fs::read(f).unwrap_or_default();
        raw += bytes.len();
        let t = std::time::Instant::now();
        let Ok(cf) = classfile::parse(&bytes) else { continue };
        parse_us += t.elapsed();
        ncp += cf.constant_pool.len();
        for e in &cf.constant_pool {
            if let Some(CpEntry::Utf8(b)) = e {
                nutf += 1;
                nutf_bytes += b.len();
                utf8_seen.insert(b.clone());
            }
        }
        for m in cf.methods.iter().chain(cf.fields.iter()) {
            if m.access_flags & 0x0008 != 0 && false { continue }
            nattr += m.attributes.len();
            nattr_bytes += m.attributes.iter().map(|a| a.info.len()).sum::<usize>();
        }
        for a in &cf.attributes {
            nattr += 1;
            nattr_bytes += a.info.len();
        }
        nmeth += cf.methods.len();
        nfld += cf.fields.len();
    }
    println!("文件 {}  原始 {} MB", files.len(), raw / 1048576);
    println!("CP 条目 {ncp}  → 24B/条 = {} MB (32B = {} MB)", ncp * 24 / 1048576, ncp * 32 / 1048576);
    println!("Utf8 条目 {nutf}  内容 {nutf_bytes} B = {} MB  独立(uninterned) {} 条 {} MB", nutf_bytes / 1048576, utf8_seen.len(), utf8_seen.iter().map(|s| s.len() + 24).sum::<usize>() / 1048576);
    println!("attr {nattr} 条 info {nattr_bytes} B = {} MB", nattr_bytes / 1048576);
    println!("methods {nmeth}  fields {nfld}");
    println!("methods map 键: 3 String × {nmeth} ≈ {} MB", nmeth * 3 * 32 / 1048576);
    println!("parse 总耗时 {:?} → 每类 {:?}", parse_us, parse_us / files.len().max(1) as u32);
}
