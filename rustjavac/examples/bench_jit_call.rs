//! JIT 方法调用基准：invokestatic + 递归。
fn main() {
    let path = "D:/vmless/genc/CallTest.class";
    let (_, m) = classfile::jit::compile_file(path, "work", "(I)I").expect("JIT work");
    let t = std::time::Instant::now();
    let r = m.call(&[50_000_000i64]);
    println!("JIT work(50M) = {r}  {:.1} ms", t.elapsed().as_secs_f64() * 1e3);

    let (_, f) = classfile::jit::compile_file(path, "fib", "(I)I").expect("JIT fib");
    let t = std::time::Instant::now();
    let fr = f.call(&[30i64]);
    println!("JIT fib(30) = {fr}  {:.1} ms", t.elapsed().as_secs_f64() * 1e3);

    // 解释器对照
    let bytes = std::fs::read(path).expect("read");
    let t = std::time::Instant::now();
    let r2 = classfile::vm::run_class_bytes(&bytes);
    println!("解释器 work(50M) = {:?}  {:.1} ms", r2.map(|x| x.output), t.elapsed().as_secs_f64() * 1e3);
}
