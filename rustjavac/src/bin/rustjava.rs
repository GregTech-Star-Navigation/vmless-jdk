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
            // 未捕获的 Java 异常：和 JVM 一样打到 stderr，并以退出码 1 结束
            if let Some(exc) = &r.exception {
                eprintln!("{exc}");
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("执行失败: {}", e.msg);
            std::process::exit(1);
        }
    }
}
