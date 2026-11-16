//! rustjava：不依赖任何 JVM 执行 class 文件的 main。
//!
//! 用法：
//! ```text
//! rustjava <class 文件>                    # 目录当 classpath（原来的行为）
//! rustjava -cp <路径[;:路径...]> <主类名>  # 目录 / jar / jimage 都行，按顺序查找
//! rustjava -jar <jar 文件>                 # 从 MANIFEST.MF 读 Main-Class
//! ```
fn usage() -> ! {
    eprintln!("用法:");
    eprintln!("  rustjava <class 文件>");
    eprintln!("  rustjava -cp <路径[;:路径...]> <主类名>");
    eprintln!("  rustjava -jar <jar 文件>");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        usage();
    }
    let result = match args[0].as_str() {
        "-h" | "--help" | "-help" => usage(),
        "-cp" | "-classpath" => {
            let cp = args.get(1).unwrap_or_else(|| usage());
            let main = args.get(2).unwrap_or_else(|| {
                eprintln!("-cp 后面要跟主类名：rustjava -cp <路径...> <主类名>");
                std::process::exit(2);
            });
            if let Some(extra) = args.get(3) {
                eprintln!("暂不支持给 main 传参数（多出来的是 {extra:?}）");
                std::process::exit(2);
            }
            classfile::vm::run_classpath(cp, main)
        }
        "-jar" => {
            let jar = args.get(1).unwrap_or_else(|| {
                eprintln!("-jar 后面要跟 jar 文件");
                std::process::exit(2);
            });
            if let Some(extra) = args.get(2) {
                eprintln!("暂不支持给 main 传参数（多出来的是 {extra:?}）");
                std::process::exit(2);
            }
            classfile::vm::run_jar(jar, None)
        }
        flag if flag.starts_with('-') => {
            eprintln!("不认识的选项 {flag}（支持 -cp / -jar）");
            std::process::exit(2);
        }
        path => {
            if let Some(extra) = args.get(1) {
                eprintln!("只跑一个入口 class 时只接收一个参数（多出来的是 {extra:?}）");
                std::process::exit(2);
            }
            classfile::vm::run_class_file(path)
        }
    };
    match result {
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
