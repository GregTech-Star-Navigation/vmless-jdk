//! 让 VM 执行 **OpenJDK 真字节码**（java.base）的里程碑驱动。
//!
//! 用法：
//! ```text
//! run_jdk <类目录> <主类名>            # 用本机 JDK 的 lib/modules
//! run_jdk -jar <jar> [args...]         # 主类从 META-INF/MANIFEST.MF 的 Main-Class 读
//! VMLESS_JDK=<jimage|目录> run_jdk …    # 换一个 java.base
//! ```
//!
//! 与 `run_jimage.rs` 的区别：那个只**解析** JDK 的类结构，这个**执行**它们。
//! 与 `rustjava` 的区别：`rustjava` 默认是独立模式（`java.*` 由内建替身实现），
//! 这里强制挂上 java.base —— 于是 `Object.<init>`、`String.length`、`ArrayList.add`
//! 跑的都是 javac 编出来的真字节码，缺 native 就报 `UnsatisfiedLinkError`。
//!
//! 另外会累积并打印本次运行遇到的**全部**缺 native（`docs/java-base-native-清单.md`
//! 的那份清单就是它产出的）。

/// 本机 JDK 的 `lib/modules`（`VMLESS_JDK` 没设时的默认）
const DEFAULT_JDK: &str = r"C:\Program Files\Eclipse Adoptium\jdk-25.0.4.101-hotspot\lib\modules";

fn main() {
    let args: Vec<String> =
        classfile::vm::take_property_args(&std::env::args().skip(1).collect::<Vec<_>>());
    let jdk = std::env::var("VMLESS_JDK").unwrap_or_else(|_| DEFAULT_JDK.to_string());
    // `Vm::open` 读的是这个环境变量；命令行也已经把它写进去了的话保持原样
    std::env::set_var("VMLESS_JDK", &jdk);
    // `-jar <jar>`：真 java 的入口形态，主类由清单的 `Main-Class` 给出
    if args.first().map(|s| s.as_str()) == Some("-jar") {
        let Some(jar) = args.get(1) else {
            eprintln!("用法: run_jdk -jar <jar 文件> [参数...]");
            std::process::exit(2);
        };
        println!("java.base: {jdk}");
        match classfile::vm::run_jar(jar, None) {
            Ok(r) => {
                for line in &r.output {
                    println!("{line}");
                }
                if let Some(exc) = &r.exception {
                    eprintln!("{exc}");
                }
                println!("退出值 = {}", r.return_value);
            }
            Err(e) => {
                println!("执行失败: {}", e.msg);
                std::process::exit(1);
            }
        }
        return;
    }
    if args.len() < 2 {
        eprintln!("用法: run_jdk [-D<键>=<值>] <类目录> <主类名>");
        eprintln!("      run_jdk [-D<键>=<值>] -jar <jar 文件> [参数...]");
        eprintln!("      例: run_jdk -Dvmless.indy.stats=1 D:/out Main");
        eprintln!("      例: run_jdk -jar D:/out/app.jar");
        eprintln!("      VMLESS_JDK=<lib/modules 或解包目录>  指定 java.base（默认本机 JDK）");
        std::process::exit(2);
    }
    let dir = &args[0];
    let main_class = &args[1];
    let spec = format!("{dir}");

    let mut vm = match classfile::vm::Vm::open(&spec, main_class) {
        Ok(vm) => vm,
        Err(e) => {
            println!("加载失败: {}", e.msg);
            std::process::exit(1);
        }
    };
    println!("java.base: {jdk}");
    println!(
        "类表: {} 个类（含 java.lang.Object = {}）",
        vm.loaded_classes().len(),
        vm.loaded_classes().iter().any(|c| c == "java/lang/Object")
    );
    let r = vm.run_main();
    match r {
        Ok(r) => {
            for line in &r.output {
                println!("{line}");
            }
            if let Some(exc) = &r.exception {
                eprintln!("{exc}");
            }
            println!("退出值 = {}", r.return_value);
        }
        Err(e) => println!("执行失败: {}", e.msg),
    }
    let missing = vm.missing_natives();
    if missing.is_empty() {
        println!("缺 native: 0");
    } else {
        println!("缺 native: {} 条", missing.len());
        for m in missing {
            println!("  {m}");
        }
    }
}
