//! 对目录树里的 .java 做"解析 + 建符号表"，统计类型/字段/引用槽。
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
    let dir = std::env::args().nth(1).expect("用法: sema_dir <目录>");
    let mut files = Vec::new();
    walk(Path::new(&dir), &mut files);
    let (mut parsed, mut types, mut fields, mut refs, mut methods) = (0usize, 0usize, 0usize, 0usize, 0usize);
    let t = std::time::Instant::now();
    for f in &files {
        let src = match std::fs::read_to_string(f) { Ok(s) => s, Err(_) => continue };
        if let Ok(cu) = classfile::parser::parse_compilation_unit(&src) {
            parsed += 1;
            let st = classfile::sema::SymbolTable::build(&cu);
            for (_, ti) in &st.types {
                types += 1;
                fields += ti.fields.len();
                refs += ti.ref_fields();
                methods += ti.methods.len();
            }
        }
    }
    println!("文件 {}：解析成功 {parsed}，类型 {types}，字段 {fields}（其中引用槽 {refs}），方法 {methods}，用时 {:?}",
        files.len(), t.elapsed());
}
