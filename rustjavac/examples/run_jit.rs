//! 直接用 JIT 跑一个 class 的 main（完全不经过解释器与 JVM）。
//!
//! `VMLESS_GC_STATS=1` 时额外把分代 GC 的统计打到 stderr（GC 有没有在回收的可见证据）。
fn main() {
    let path = std::env::args().nth(1).expect("用法: run_jit <class 文件>");
    let bytes = std::fs::read(&path).expect("read");
    let cf = classfile::parse(&bytes).expect("parse");
    let mut jit = classfile::jit::Jit::new();
    // 继承：把同目录的其它 .class 也读进来（父类/子类的方法都要编）
    jit.load_class_path(&path);
    let t = std::time::Instant::now();
    let m = jit.compile_static(&cf, "main", "([Ljava/lang/String;)V").expect("JIT main");
    eprintln!("JIT 编译耗时: {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
    let t = std::time::Instant::now();
    let _ = m.call(&[0i64]);
    eprintln!("执行耗时: {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
    if std::env::var_os("VMLESS_GC_STATS").is_some() {
        classfile::jit::rt::print_gc_stats("run_jit");
    }
}
