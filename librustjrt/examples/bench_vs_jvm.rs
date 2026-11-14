//! 与 JVM 同工作量的对比基准：
//! 分配 N 个 Node{int v; Node next}，其中 1/8 串成链保留，其余成为垃圾；然后标记清扫。
use librustjrt::{BumpArena, ClassMeta, FieldKind, Heap, JavaObject};

const N: usize = 8_000_000;

fn main() {
    let meta = ClassMeta { name: "Node".into(), fields: vec![FieldKind::Prim, FieldKind::Ref] };

    // ---- 场景 A：纯 bump 分配（无回收）----
    let arena = BumpArena::new(512 << 20);
    let t = std::time::Instant::now();
    let mut last: *mut JavaObject = std::ptr::null_mut();
    for i in 0..N {
        let o = arena.alloc_object(2).expect("arena full");
        unsafe {
            let f = (o as *mut u8).add(16) as *mut u64;
            *f = i as u64;
            *f.add(1) = last as u64;
        }
        if i & 7 == 0 { last = o; }
    }
    let bump = t.elapsed();
    println!(
        "bump      : {:>6.1} ns/对象  吞吐 {:.1} M/s  用 {:.1} MB",
        bump.as_nanos() as f64 / N as f64,
        N as f64 / bump.as_secs_f64() / 1e6,
        arena.used() as f64 / 1048576.0
    );

    // ---- 场景 B：带真实回收的 Heap ----
    let mut heap = Heap::new(512 << 20);
    let t = std::time::Instant::now();
    let mut root: *mut JavaObject = std::ptr::null_mut();
    for i in 0..N {
        let o = heap.alloc(&meta);
        unsafe {
            let f = (o as *mut u8).add(16) as *mut u64;
            *f = i as u64;
        }
        if i & 7 == 0 {
            heap.set_ref(o, 1, root);
            root = o;
        }
    }
    let alloc = t.elapsed();
    println!(
        "heap 分配 : {:>6.1} ns/对象  吞吐 {:.1} M/s  arena {:.1} MB",
        alloc.as_nanos() as f64 / N as f64,
        N as f64 / alloc.as_secs_f64() / 1e6,
        heap.arena_used() as f64 / 1048576.0
    );

    // ---- 场景 C：标记清扫（1/8 存活）----
    let t = std::time::Instant::now();
    heap.collect(&[root], &[&meta]);
    let gc = t.elapsed();
    println!(
        "标记清扫  : {:.1} ms  回收 {} 个  存活 {} 个  单对象 {} 字节",
        gc.as_secs_f64() * 1e3,
        heap.collected,
        heap.live_count(),
        meta.size()
    );
}
