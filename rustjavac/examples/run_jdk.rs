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
    classfile::vm::prof("run_jdk 进入 main");
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
        classfile::vm::set_main_args(args.iter().skip(2).cloned().collect());
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
    // 命令行参数（`run_jdk <dir> <Main> a b c` 的 a b c）
    classfile::vm::set_main_args(args.iter().skip(2).cloned().collect());
    classfile::vm::prof("参数解析完成");
    let t_open = std::time::Instant::now();

    let mut vm = match classfile::vm::Vm::open(&spec, main_class) {
        Ok(vm) => vm,
        Err(e) => {
            println!("加载失败: {}", e.msg);
            std::process::exit(1);
        }
    };
    classfile::vm::prof("Vm::open 返回");
    println!("java.base: {jdk}");
    // 类表是**按需**长的：这里只报「已经解析进内存」的类数，别让人误以为启动就把
    // java.base 全读进来了（以前那句话是「类表: 5421 个类」，那确实是全读了）。
    println!(
        "类表: 已加载 {} 个类（含 java.lang.Object = {}）",
        vm.loaded_class_count(),
        vm.has_class_loaded("java/lang/Object")
    );
    if std::env::var_os("VMLESS_CP_STATS").is_some() {
        eprintln!("[cp] Vm::open {} ms", t_open.elapsed().as_millis());
    }
    let t_run = std::time::Instant::now();
    let r = vm.run_main();
    if std::env::var_os("VMLESS_CP_STATS").is_some() {
        eprintln!("[cp] run_main {} ms", t_run.elapsed().as_millis());
    }
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
    if std::env::var_os("VMLESS_CP_STATS").is_some() {
        eprintln!("[cp] 运行结束时已加载 {} 个类", vm.loaded_class_count());
    }

    if missing.is_empty() {
        println!("缺 native: 0");
    } else {
        println!("缺 native: {} 条", missing.len());
        for m in missing {
            println!("  {m}");
        }
    }
}
