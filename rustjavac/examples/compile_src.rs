//! 源码 → 字节码：读 .java，写出 .class。
fn main() {
    let src_path = std::env::args().nth(1).expect("用法: compile_src <源文件> <输出目录>");
    let out_dir = std::env::args().nth(2).expect("用法: compile_src <源文件> <输出目录>");
    let src = std::fs::read_to_string(&src_path).expect("读源码");
    let cu = classfile::parser::parse_compilation_unit(&src).expect("解析失败");
    // 输出目录同时当 classpath：父类在别的编译单元时从这里读它的 .class
    let classes = classfile::compiler::compile_with_path(&cu, &[out_dir.clone().into()]).expect("编译失败");
    for (name, bytes) in &classes {
        // 带包名的类要落到 `out/p/C.class`（JVM 的 -cp 目录按这个布局找类）
        let file = format!("{}/{}.class", out_dir, name);
        if let Some(dir) = std::path::Path::new(&file).parent() {
            std::fs::create_dir_all(dir).expect("建输出目录");
        }
        std::fs::write(&file, bytes).expect("写出");
        println!("{name}: {} 字节 → {file}", bytes.len());
    }
}
