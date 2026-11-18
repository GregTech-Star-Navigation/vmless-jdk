//! 单文件诊断：`probe_src <源文件> <classpath 目录...>`
//!
//! 与 cover_dir 的区别：只编一个文件，把原始错误打出来（带方法名），用来定位
//! 「这个构造到底哪不支持了」。不写 class 文件。

fn main() {
    let mut args = std::env::args().skip(1);
    let src_path = args.next().expect("用法: probe_src <源文件> <classpath...>");
    let cps: Vec<std::path::PathBuf> = args.map(std::path::PathBuf::from).collect();
    let src = std::fs::read_to_string(&src_path).expect("读源码");
    let cu = match classfile::parser::parse_compilation_unit(&src) {
        Ok(cu) => cu,
        Err(e) => {
            println!("parse 失败 L{}: {}", e.line, e.msg);
            return;
        }
    };
    match classfile::compiler::compile_with_path(&cu, &cps) {
        Ok(v) => println!(
            "编译通过（{} 个类型: {}）",
            v.len(),
            v.iter().map(|(n, b)| format!("{n}({}B)", b.len())).collect::<Vec<_>>().join(", ")
        ),
        Err(e) => println!("compile 失败: {}", e.msg),
    }
}
