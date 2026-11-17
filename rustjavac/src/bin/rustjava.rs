//! rustjava：不依赖任何 JVM 执行 class 文件的 main。
//!
//! 用法：
//! ```text
//! rustjava <class 文件>                    # 目录当 classpath（原来的行为）
//! rustjava -cp <路径[;:路径...]> <主类名>  # 目录 / jar / jimage 都行，按顺序查找
//! rustjava -jar <jar 文件>                 # 从 MANIFEST.MF 读 Main-Class
//! rustjava -Dk=v …                        # 系统属性（目前认 `-Dvmless.indy.stats`）
//!
//! # 模块模式（JPMS）—— 任一开关出现就进模块通道
//! rustjava --describe-module java.base
//! rustjava --print-module-resolution java.base
//! rustjava --add-opens java.base/java.lang.invoke=ALL-UNNAMED -cp out Main
//! rustjava --module-path mods --add-modules app --module app/Main
//! ```
fn usage() -> ! {
    eprintln!("用法:");
    eprintln!("  rustjava <class 文件>");
    eprintln!("  rustjava -cp <路径[;:路径...]> <主类名>");
    eprintln!("  rustjava -jar <jar 文件>");
    eprintln!("  -D<键>=<值> 可以出现在最前面（例如 -Dvmless.indy.stats=1）");
    eprintln!("模块模式:");
    eprintln!("  --module-path <p> | -p <p>        模块路径（jimage / 目录 / 多个用 ;）");
    eprintln!("  --add-modules <a,b>               额外解析的模块（含 ALL-DEFAULT / ALL-SYSTEM）");
    eprintln!("  --limit-modules <a,b>             只看这些模块 + 它们的传递依赖");
    eprintln!("  --add-opens <m>/<p>=<target>      反射开放（target 可以是 ALL-UNNAMED）");
    eprintln!("  --add-exports <m>/<p>=<target>    普通访问开放");
    eprintln!("  --add-reads <m>=<target>          加一条 reads");
    eprintln!("  --module <m>[/<主类>] | -m        以模块方式启动");
    eprintln!("  --describe-module <m>             打印模块描述符");
    eprintln!("  --print-module-resolution [m]     打印 requires 树 + 拓扑序");
    std::process::exit(2);
}

fn print_result(r: classfile::vm::ExecResult) {
    for line in &r.output {
        println!("{line}");
    }
    // 未捕获的 Java 异常：和 JVM 一样打到 stderr，并以退出码 1 结束
    if let Some(exc) = &r.exception {
        eprintln!("{exc}");
        std::process::exit(1);
    }
}

fn main() {
    let args: Vec<String> =
        classfile::vm::take_property_args(&std::env::args().skip(1).collect::<Vec<_>>());
    if args.is_empty() {
        usage();
    }
    // ---- 模块模式：只要出现任何一个 JPMS 开关就走这条通道 ----
    // `-cp` / `-jar` / 位置参数单独出现时仍然走下面的旧路径（103 个样例不受影响）。
    if args.iter().any(|a| classfile::load::cli::is_jpms_flag(a)) {
        let opts = match classfile::load::cli::parse(&args) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        };
        if let Some(name) = &opts.describe_module {
            match classfile::vm::jpms::describe_module(&opts, name) {
                Ok(s) => {
                    print!("{s}");
                    return;
                }
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        if let Some(root) = &opts.print_resolution {
            match classfile::vm::jpms::print_module_resolution(&opts, root.as_deref()) {
                Ok(s) => {
                    print!("{s}");
                    return;
                }
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        match classfile::vm::jpms::run(&opts) {
            Ok(r) => print_result(r),
            Err(e) => {
                // 加载期的模块访问错误本身就是 `java.lang.IllegalAccessError: …` 的形态
                eprintln!("{}", classfile::vm::jpms::format_load_error(&e.msg));
                std::process::exit(1);
            }
        }
        return;
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
            eprintln!("不认识的选项 {flag}（支持 -cp / -jar / --describe-module 等）");
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
        Ok(r) => print_result(r),
        Err(e) => {
            eprintln!("执行失败: {}", e.msg);
            std::process::exit(1);
        }
    }
}
