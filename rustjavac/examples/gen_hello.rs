//! 生成一个可运行的 Hello.class（Rust 产出的字节码）。
fn main() {
    let out_dir = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    let bytes = classfile::codegen::simple_main("Hello", "hello from rustjavac");
    let path = std::path::Path::new(&out_dir).join("Hello.class");
    std::fs::write(&path, &bytes).expect("write");
    println!("已写出 {} 字节 → {}", bytes.len(), path.display());
}
