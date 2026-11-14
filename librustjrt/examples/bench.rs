//! bump arena vs Box 的对象分配吞吐对比
use librustjrt::{BumpArena, JavaObject};
use std::time::Instant;

const N: usize = 10_000_000;

fn main() {
    // bump arena：一次分配 1GB，逐个 bump
    let arena = BumpArena::new(1 << 30);
    let t = Instant::now();
    for _ in 0..N {
        arena.alloc_object(4).expect("arena full");
    }
    let d = t.elapsed();
    println!(
        "bump   : {:>8.1} ns/对象  总 {:.0} ms  用了 {:.0} MB",
        d.as_nanos() as f64 / N as f64,
        d.as_secs_f64() * 1e3,
        arena.used() as f64 / 1048576.0
    );
    println!("        布局: 头 {} 字节, 4 字段 = {} 字节/对象", std::mem::size_of::<JavaObject>(), JavaObject::size_for(4));

    // Box：每次 malloc
    let t = Instant::now();
    let mut v = Vec::with_capacity(N);
    for _ in 0..N {
        v.push(Box::new([0u64; 4]));
    }
    let d = t.elapsed();
    println!(
        "Box    : {:>8.1} ns/对象  总 {:.0} ms",
        d.as_nanos() as f64 / N as f64,
        d.as_secs_f64() * 1e3
    );
    std::hint::black_box(&v);
}
