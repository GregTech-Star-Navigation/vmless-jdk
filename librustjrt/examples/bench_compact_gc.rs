//! 紧凑堆：分配 + 标记清扫（与 JVM 同工作量）
use librustjrt::{CompactHeap, CompactMeta, FieldKind};

const N: usize = 8_000_000;

fn main() {
    let meta = CompactMeta { name: "Node", fields: vec![FieldKind::Prim, FieldKind::Ref] };
    let mut heap = CompactHeap::new(512 << 20);
    let mut root: *mut librustjrt::CompactObject = std::ptr::null_mut();
    for i in 0..N {
        let o = heap.alloc(&meta);
        heap.set_slot(o, 0, i as u32);
        if i & 7 == 0 {
            heap.set_slot(o, 1, if root.is_null() { 0 } else { heap.offset_of(root) });
            root = o;
        }
    }
    println!("分配完成：{} 个对象，{:.1} MB", heap.live_count(), heap.used_bytes() as f64 / 1048576.0);
    let t = std::time::Instant::now();
    heap.collect(&[root], &meta);
    let gc = t.elapsed();
    println!(
        "标记清扫  : {:.1} ms  回收 {} 个  存活 {} 个",
        gc.as_secs_f64() * 1e3,
        heap.collected,
        heap.live_count()
    );
}
