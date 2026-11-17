fn main() {
    let t = std::time::Instant::now();
    let (_jit, m) = classfile::jit::compile_file("D:/vmless/genj/Perf2.class", "sum", "(I)I").expect("jit");
    eprintln!("JIT ok {:.1}ms r={}", t.elapsed().as_secs_f64()*1e3, m.call(&[1000i64]));
    eprintln!("→ 现在建解释器");
    let t = std::time::Instant::now();
    let vm = classfile::vm::Vm::open("D:/vmless/bjx/t2", "T1");
    eprintln!("Vm::open → {:?} ({:.1}ms)", vm.as_ref().map(|v| v.loaded_classes().len()), t.elapsed().as_secs_f64()*1e3);
}
