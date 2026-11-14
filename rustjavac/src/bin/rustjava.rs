//! rustjava：不依赖任何 JVM 执行 class 文件的 main。
//!
//! 用法：rustjava <class 文件> [-q]
fn main() {
    let mut args = std::env::args().skip(1);
    let path = match args.next() {
        Some(p) => p,
        None => {
            eprintln!("用法: rustjava <class 文件>");
            std::process::exit(2);
        }
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("读取 {path} 失败: {e}");
            std::process::exit(1);
        }
    };
    match classfile::vm::run_class_bytes(&bytes) {
        Ok(r) => {
            for line in &r.output {
                println!("{line}");
            }
        }
        Err(e) => {
            eprintln!("执行失败: {}", e.msg);
            std::process::exit(1);
        }
    }
}
