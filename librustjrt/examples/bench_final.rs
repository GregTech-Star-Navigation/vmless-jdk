//! 统一基准输出（便于和 JVM 交替对比）：分配 ns/对象 + 一次切片的标记清扫。
use librustjrt::{CompactHeap, CompactMeta, FieldKind, IncrementalState};

const N: usize = 8_000_000;

fn main() {
    let meta = CompactMeta { name: "Node", fields: vec![FieldKind::Prim, FieldKind::Ref] };
    let mut heap = CompactHeap::new(512 << 20);
    let t = std::time::Instant::now();
    let mut root: *mut librustjrt::CompactObject = std::ptr::null_mut();
    for i in 0..N {
        let o = heap.alloc(&meta);
        heap.set_slot(o, 0, i as u32);
        if i & 7 == 0 {
            heap.set_slot(o, 1, if root.is_null() { 0 } else { heap.offset_of(root) });
            root = o;
        }
    }
    let alloc = t.elapsed();
    let r = if root.is_null() { 0 } else { heap.offset_of(root) };

    let mut st = IncrementalState::default();
    let t = std::time::Instant::now();
    heap.begin_cycle(&mut st, &[r]);
    let mut more = true;
    while more {
        more = heap.step(&mut st, &meta, 200);
    }
    let gc = t.elapsed();
    println!("ALLOC_NS={:.1}", alloc.as_nanos() as f64 / N as f64);
    println!("GC_MS={:.1}", gc.as_secs_f64() * 1e3);
    println!("MAX_STEP_MS={:.3}", st.max_step_ns as f64 / 1e6);
    println!("BYTES_PER_OBJ={}", meta.size());
    println!("TOTAL_MB={:.1}", heap.used_bytes() as f64 / 1048576.0);
    println!("LIVE={}", heap.live_count());
}
