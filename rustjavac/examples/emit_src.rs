//! 单文件「真编译」：`emit_src <源文件> <输出目录> <classpath 目录...>`
//!
//! 与 compile_src 的区别：编译时要能查 classpath 上的类（跨编译单元的类型/方法），
//! 所以编 java.base 这种「依赖几百个类」的目录必须用它。类按内部名落到输出目录。

fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() { walk(&p, out); }
            else if p.extension().and_then(|x| x.to_str()) == Some("java") { out.push(p); }
        }
    }
}

fn main() {
    // --dir <源目录> <输出目录> <classpath...>：批量发射（platform() 每进程只反解一次，
    // 逐个文件起进程会把 7350 个 .class 反解几千遍）
    if std::env::args().nth(1).as_deref() == Some("--dir") {
        let src_dir = std::env::args().nth(2).expect("用法: --dir <源目录> <输出目录> <cp...>");
        let out_dir = std::env::args().nth(3).expect("输出目录");
        let cps: Vec<std::path::PathBuf> = std::env::args().skip(4).map(std::path::PathBuf::from).collect();
        let mut files = Vec::new();
        walk(std::path::Path::new(&src_dir), &mut files);
        files.sort();
        let (mut ok, mut bad) = (0usize, 0usize);
        for f in &files {
            let src = match std::fs::read_to_string(f) { Ok(s) => s, Err(_) => continue };
            let cu = match classfile::parser::parse_compilation_unit(&src) { Ok(c) => c, Err(_) => { bad += 1; continue } };
            let classes = match classfile::compiler::compile_with_path(&cu, &cps) { Ok(v) => v, Err(_) => { bad += 1; continue } };
            for (name, bytes) in &classes {
                let file = format!("{out_dir}/{name}.class");
                if let Some(d) = std::path::Path::new(&file).parent() { let _ = std::fs::create_dir_all(d); }
                if std::fs::write(&file, bytes).is_ok() { ok += 1; }
            }
        }
        println!("文件 {} 个：发射类 {} 个，跳过 {} 个", files.len(), ok, bad);
        return;
    }
    let mut args = std::env::args().skip(1);
    let src_path = args.next().expect("用法: emit_src <源文件> <输出目录> <classpath...>");
    let out_dir = args.next().expect("用法: emit_src <源文件> <输出目录> <classpath...>");
    let cps: Vec<std::path::PathBuf> = args.map(std::path::PathBuf::from).collect();
    let src = std::fs::read_to_string(&src_path).expect("读源码");
    let cu = match classfile::parser::parse_compilation_unit(&src) {
        Ok(cu) => cu,
        Err(e) => {
            println!("parse 失败 L{}: {}", e.line, e.msg);
            std::process::exit(1);
        }
    };
    let classes = match classfile::compiler::compile_with_path(&cu, &cps) {
        Ok(v) => v,
        Err(e) => {
            println!("compile 失败: {}", e.msg);
            std::process::exit(1);
        }
    };
    for (name, bytes) in &classes {
        let file = format!("{out_dir}/{name}.class");
        if let Some(dir) = std::path::Path::new(&file).parent() {
            std::fs::create_dir_all(dir).expect("建输出目录");
        }
        std::fs::write(&file, bytes).expect("写出");
        println!("{name}: {} 字节 → {file}", bytes.len());
    }
}
