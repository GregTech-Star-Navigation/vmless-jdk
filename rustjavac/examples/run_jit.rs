//! 直接用 JIT 跑一个 class 的 main（完全不经过 JVM）。
//!
//! `VMLESS_GC_STATS=1` 时额外把分代 GC 的统计打到 stderr（GC 有没有在回收的可见证据）。
//!
//! # 方法级回退（不是程序级）
//!
//! 编译是**逐方法**的：某个方法编不了（`invokedynamic` 的引导方法 JIT 不接、
//! `synchronized`、被调方跨边界不安全）**只让它自己**交给解释器，别的方法照旧编。
//! 两套运行时之间的通道见 `jit::rt` / `vm::bridge`：
//!
//! * 入口 `main` 编不了 → 整个程序从解释器入口跑，但**已经编好的纯数值方法**
//!   由解释器回调机器码（`vm::install_jit`）；
//! * 入口编得了、被调方法编不了 → 那个调用点发解释器桩（`rt::interp_call`），
//!   其余机器码不变。
//!
//! 统计（编译几个、解释几个、为什么）一律打到 stderr，不静默换路。
//!
//! # 解释器必须**预先**建好
//!
//! `Vm::open` 要加载整个 java.base（~95MB + 数秒）。它一旦落在生成代码的调用路径上，
//! 任何岔子都会变成「每次调用重建一个解释器」，工作集按 90MB/s 涨到把机器吃光。
//! 所以解释器只在这里、跑任何机器码之前建一次；回调里查不到就报错退出，绝不分配。
fn main() {
    let path = std::env::args().nth(1).expect("用法: run_jit <class 文件>");
    let bytes = std::fs::read(&path).expect("read");
    let cf = classfile::parse(&bytes).expect("parse");
    // 字节码校验：JIT 直接编机器码、**不经过** `Vm::execute`，所以这里必须先验一遍，
    // 否则非法 class 会被编成机器码照跑（`R4` 就是这样「算出了对的答案」的）。
    // 与 HotSpot 一样：验证失败 → `VerifyError`，一条指令都不执行。
    if let Err(why) = classfile::vm::verify_class(&cf) {
        eprintln!("Exception in thread \"main\" java.lang.VerifyError: {why}");
        std::process::exit(1);
    }
    let mut jit = classfile::jit::Jit::new();
    // 继承：把同目录的其它 .class 也读进来（父类/子类的方法都要编）
    jit.load_class_path(&path);

    // 解释器回退通道要加载同一份类路径（真正组织成「包根目录 + 主类名」）
    let class_name = cf.this_class_name().unwrap_or_default();
    let dir = std::path::Path::new(&path)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| ".".into());
    let root = classfile::vm::package_root(&dir, &class_name);

    let t = std::time::Instant::now();
    let compiled = jit.compile_static(&cf, "main", "([Ljava/lang/String;)V");
    let compile_ms = t.elapsed().as_secs_f64() * 1e3;
    eprintln!("{}", jit.stats_line());
    if std::env::var_os("VMLESS_JIT_VERBOSE").is_some() {
        for line in jit.interpreted_lines() {
            eprintln!("{line}");
        }
    }

    let m = match compiled {
        Ok(m) => m,
        Err(e) => {
            // 入口方法自己编不了：它交给解释器，但程序里别的方法照旧逐方法试一遍，
            // 能编的都由解释器回调机器码（回退是方法级的，不是程序级的）。
            eprintln!("JIT 无法编译入口方法：{}", e.0);
            if let Err(e) = jit.compile_all() {
                eprintln!("逐方法编译时出错: {}", e.0);
            }
            eprintln!("{}", jit.stats_line());
            if std::env::var_os("VMLESS_JIT_VERBOSE").is_some() {
                for line in jit.interpreted_lines() {
                    eprintln!("{line}");
                }
            }
            let table = jit.pure_table();
            eprintln!(
                "→ 入口方法交给解释器执行；其中 {} 个纯数值方法仍由 JIT 执行{}",
                table.len(),
                if table.is_empty() {
                    String::new()
                } else {
                    let names: Vec<String> = table.iter().map(|(c, n, d, _)| format!("{c}.{n}{d}")).collect();
                    format!("（{}）", names.join("、"))
                }
            );
            match classfile::vm::Vm::open(&root.to_string_lossy(), &class_name) {
                Ok(mut vm) => {
                    classfile::vm::install_jit(table);
                    match vm.run_main() {
                        Ok(r) => {
                            for line in &r.output {
                                println!("{line}");
                            }
                            if let Some(exc) = &r.exception {
                                eprintln!("{exc}");
                                std::process::exit(1);
                            }
                            return;
                        }
                        Err(e) => {
                            eprintln!("执行失败: {}", e.msg);
                            std::process::exit(1);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("解释器加载失败（无法回退）: {}", e.msg);
                    std::process::exit(1);
                }
            }
        }
    };

    // 有方法要在解释器里跑：**现在**把解释器建好（跑机器码之前，一次性）
    if classfile::vm::interp_targets() > 0 {
        if let Err(e) = classfile::vm::open_interp(&root.to_string_lossy(), &class_name) {
            eprintln!("JIT 回退的解释器加载失败: {}", e.msg);
            std::process::exit(1);
        }
        eprintln!(
            "JIT 回退通道就绪：{} 个方法交给解释器执行",
            classfile::vm::interp_targets()
        );
    }

    eprintln!("JIT 编译耗时: {compile_ms:.2} ms");
    let t = std::time::Instant::now();
    let _ = m.call(&[0i64]);
    eprintln!("执行耗时: {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
    if std::env::var_os("VMLESS_GC_STATS").is_some() {
        classfile::jit::rt::print_gc_stats("run_jit");
    }
}
