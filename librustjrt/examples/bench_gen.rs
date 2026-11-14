//! 分代回收基准：8M 次分配、1/8 存活，对比全堆标记清扫。
use librustjrt::gen::GenerationalHeap;
use librustjrt::{CompactMeta, FieldKind};

const N: u32 = 8_000_000;

fn main() {
    let meta = CompactMeta { name: "Node", fields: vec![FieldKind::Prim, FieldKind::Ref] };
    let mut heap = GenerationalHeap::new(768 << 20);

    let t = std::time::Instant::now();
    let mut root: u32 = 0;
    for i in 0..N {
        let o = heap.alloc(&meta);
        heap.set_slot(o, 0, i);
        if i & 7 == 0 {
            heap.set_slot(o, 1, root);
            root = o;
        }
    }
    let alloc = t.elapsed();
    println!("ALLOC_NS={:.1}", alloc.as_nanos() as f64 / N as f64);
    println!("YOUNG_MB={:.1}", heap.young_used() as f64 / 1048576.0);

    let mut roots = [root];
    let t = std::time::Instant::now();
    let st = heap.minor(&mut roots, &meta);
    let gc = t.elapsed();
    println!("MINOR_MS={:.1}", gc.as_secs_f64() * 1e3);
    println!("COPIED={}", st.copied);
    println!("OLD_LIVE={}", heap.old_live_count());
    println!("OLD_MB={:.1}", heap.old_used() as f64 / 1048576.0);
    println!("MAX_STEP_MS={:.3}", st.max_step_ns as f64 / 1e6);
}
