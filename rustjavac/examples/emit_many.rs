//! 批量真编译（写 class 文件）：`emit_many <列表文件> <输出目录> <classpath 目录...>`
//!
//! 列表文件每行一个 `.java` 绝对路径。用途：一次性编出一批 class，交给真 JVM 的
//! `-Xverify:all` 抽样装载（单进程，classpath 的类元数据只反解一次）。

fn main() {
    let mut it = std::env::args().skip(1);
    let list = std::path::PathBuf::from(it.next().expect("用法: emit_many <列表> <输出目录> [classpath...]"));
    let out_dir = std::path::PathBuf::from(it.next().expect("缺输出目录"));
    let mut classpath: Vec<std::path::PathBuf> = vec![out_dir.clone()];
    classpath.extend(it.map(std::path::PathBuf::from));
    if classpath.len() == 1 {
        if let Some(p) = std::env::var_os("VMLESS_JDK") {
            let p = std::path::PathBuf::from(p);
            let sub = p.join("java.base");
            if sub.is_dir() { classpath.push(sub); }
            if p.is_dir() { classpath.push(p); }
        }
    }
    let text = std::fs::read_to_string(&list).expect("读列表");
    let mut ok = 0usize;
    let mut fail = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        let path = std::path::PathBuf::from(line);
        let src = match std::fs::read_to_string(&path) { Ok(s) => s, Err(_) => { fail += 1; continue; } };
        let cu = match classfile::parser::parse_compilation_unit(&src) {
            Ok(c) => c,
            Err(_) => { fail += 1; continue; }
        };
        match classfile::compiler::compile_with_path(&cu, &classpath) {
            Ok(cs) => {
                for (name, bytes) in cs {
                    let p = out_dir.join(format!("{name}.class"));
                    if let Some(par) = p.parent() { let _ = std::fs::create_dir_all(par); }
                    if std::fs::write(&p, &bytes).is_err() { fail += 1; continue; }
                }
                ok += 1;
            }
            Err(_) => fail += 1,
        }
    }
    println!("emit_many: 编译成功 {ok} 个文件，失败 {fail}");
}
