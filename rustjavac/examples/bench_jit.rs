//! JIT vs 解释器：同一个 class 方法，两种执行方式。
fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "D:/vmless/genj/Perf2.class".into());
    let n: i32 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(50_000_000);

    // 1) JIT 编译
    let t = std::time::Instant::now();
    let (_jit, m) = classfile::jit::compile_file(&path, "sum", "(I)I").expect("JIT 编译");
    println!("JIT 编译耗时: {:.2} ms", t.elapsed().as_secs_f64() * 1e3);

    // 2) 跑 JIT 版本
    let t = std::time::Instant::now();
    let r1 = m.call(&[n]);
    let jit_ms = t.elapsed().as_secs_f64() * 1e3;
    println!("JIT      : 结果={r1}  {:.1} ms", jit_ms);

    // 3) 解释器跑同一个方法（通过 main）
    let bytes = std::fs::read(&path).expect("read");
    let t = std::time::Instant::now();
    let r2 = classfile::vm::run_class_bytes(&bytes);
    let interp_ms = t.elapsed().as_secs_f64() * 1e3;
    match r2 {
        Ok(r) => println!("解释器    : 结果={}  {:.1} ms", r.output.join(","), interp_ms),
        Err(e) => println!("解释器失败: {}", e.msg),
    }
    println!("加速比    : {:.1}x", interp_ms / jit_ms.max(0.001));
}
