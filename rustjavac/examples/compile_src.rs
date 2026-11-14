//! 源码 → 字节码：读 .java，写出 .class。
fn main() {
    let src_path = std::env::args().nth(1).expect("用法: compile_src <源文件> <输出目录>");
    let out_dir = std::env::args().nth(2).expect("用法: compile_src <源文件> <输出目录>");
    let src = std::fs::read_to_string(&src_path).expect("读源码");
    let cu = classfile::parser::parse_compilation_unit(&src).expect("解析失败");
    let classes = classfile::compiler::compile(&cu).expect("编译失败");
    for (name, bytes) in &classes {
        let file = format!("{}/{}.class", out_dir, name.replace('/', "_"));
        std::fs::write(&file, bytes).expect("写出");
        println!("{name}: {} 字节 → {file}", bytes.len());
    }
}
