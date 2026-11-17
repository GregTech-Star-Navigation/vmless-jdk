//! `javaw` —— 与 `java` 完全相同，只是 Windows 上走 GUI 子系统（不弹控制台）。
//!
//! 参数语义与 `rustjava`（我们已有的 java 实现）保持一致：
//!   javaw <class 文件> | javaw -cp <路径...> <主类> | javaw -jar <jar>
//! 区别只有一个：`#![windows_subsystem = "windows"]` —— 双击运行时不打开控制台窗口，
//! 这也是真 JDK 里 javaw 与 java 的唯一区别（输出仍然写到 stdout，被重定向时照常可见）。

#![windows_subsystem = "windows"]

fn usage() -> ! {
    eprintln!("用法:");
    eprintln!("  javaw <class 文件>");
    eprintln!("  javaw -cp <路径[;:路径...]> <主类名>");
    eprintln!("  javaw -jar <jar 文件>");
    std::process::exit(2);
}

fn main() {
    vmless_tools::install_pipe_guard();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        usage();
    }
    // jps 的数据源：启动时登记本进程，退出时删除（launcher 被强杀时会留下僵尸项，jps 会剔除）
    let pid = std::process::id();
    let main_class = match args[0].as_str() {
        "-cp" | "-classpath" => args.get(2).cloned().unwrap_or_default(),
        "-jar" => args.get(1).cloned().unwrap_or_default(),
        f if f.starts_with('-') => String::new(),
        p => std::path::Path::new(p).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
    };
    let _ = vmless_tools::jpsreg::register(pid, &main_class, &args.join(" "), "javaw");
    let result = match args[0].as_str() {
        "-h" | "--help" | "-help" | "-?" => {
            println!("用法:");
            println!("  javaw <class 文件>");
            println!("  javaw -cp <路径[;:路径...]> <主类名>");
            println!("  javaw -jar <jar 文件>");
            println!();
            println!("与 java 相同，只是 Windows 下使用 GUI 子系统（不弹控制台）。");
            return;
        }
        "-cp" | "-classpath" => {
            let cp = args.get(1).unwrap_or_else(|| usage());
            let main = args.get(2).unwrap_or_else(|| {
                eprintln!("-cp 后面要跟主类名：javaw -cp <路径...> <主类名>");
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
    // 跑完就撤销 jps 登记（正常退出路径；被强杀时留下僵尸项，jps 会按 pid 存活剔除）
    vmless_tools::jpsreg::unregister(std::process::id());
    match result {
        Ok(r) => {
            for line in &r.output {
                println!("{line}");
            }
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
