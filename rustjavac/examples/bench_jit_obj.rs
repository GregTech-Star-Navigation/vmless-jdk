//! JIT 对象/数组基准
fn main() {
    let path = "D:/vmless/geno/ObjArr.class";
    let bytes = std::fs::read(path).expect("read");
    let cf = classfile::parse(&bytes).expect("parse");
    let mut jit = classfile::jit::Jit::new();
    let m = jit.compile_static(&cf, "run", "(I)I").expect("JIT run");
    let t = std::time::Instant::now();
    let r = m.call(&[100i64]);
    println!("JIT run(1000) = {r}  {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
    let t = std::time::Instant::now();
    let mut acc = 0i64;
    for _ in 0..1000 { acc += m.call(&[100i64]) as i64; }
    println!("JIT x1000 累计 = {acc}  {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
}
