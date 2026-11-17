//! 直接用 JIT 跑一个 class 的 main（完全不经过解释器与 JVM）。
//!
//! `VMLESS_GC_STATS=1` 时额外把分代 GC 的统计打到 stderr（GC 有没有在回收的可见证据）。
//!
//! # `invokedynamic` 的降级
//!
//! JIT **不支持** 0xba（调用点缓存要变成机器码里的可替换跳板，没做）。遇到它时
//! `compile_static` 会返回带 `invokedynamic` 字样的 `JitError`，这里就**整体退回解释器**
//! 执行 —— 解释器那条路径是完整的（`vm::indy`）。降级会明说，不静默换路：
//! stderr 打一行 `JIT 不支持 invokedynamic …，已退回解释器执行`。
fn main() {
    let path = std::env::args().nth(1).expect("用法: run_jit <class 文件>");
    let bytes = std::fs::read(&path).expect("read");
    let cf = classfile::parse(&bytes).expect("parse");
    let mut jit = classfile::jit::Jit::new();
    // 继承：把同目录的其它 .class 也读进来（父类/子类的方法都要编）
    jit.load_class_path(&path);
    let t = std::time::Instant::now();
    let compiled = jit.compile_static(&cf, "main", "([Ljava/lang/String;)V");
    let m = match compiled {
        Ok(m) => m,
        Err(e) if e.0.contains("invokedynamic")
            || e.0.contains("java.lang.invoke")
            || e.0.contains("synchronized")
            || e.0.contains("monitorenter")
            || e.0.contains("monitorexit") =>
        {
            // JIT 不支持的字节码：退回解释器（不是把错误吞掉，而是换一条已实现的路径）
            eprintln!("JIT 跳过：{}", e.0);
            eprintln!("→ 退回解释器执行（invokedynamic / java.lang.invoke / 对象监视器由解释器实现）");
            match classfile::vm::run_class_file(&path) {
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
            eprintln!("JIT 编译失败: {}", e.0);
            std::process::exit(1);
        }
    };
    eprintln!("JIT 编译耗时: {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
    let t = std::time::Instant::now();
    let _ = m.call(&[0i64]);
    eprintln!("执行耗时: {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
    if std::env::var_os("VMLESS_GC_STATS").is_some() {
        classfile::jit::rt::print_gc_stats("run_jit");
    }
}
