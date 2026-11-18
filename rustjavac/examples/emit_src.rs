//! 单文件「真编译」：`emit_src <源文件> <输出目录> <classpath 目录...>`
//!
//! 与 compile_src 的区别：编译时要能查 classpath 上的类（跨编译单元的类型/方法），
//! 所以编 java.base 这种「依赖几百个类」的目录必须用它。类按内部名落到输出目录。

fn main() {
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
