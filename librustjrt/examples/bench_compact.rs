//! 紧凑布局基准：与 JVM 同工作量（分配 N 个 Node{int,ref}，1/8 存活）。
use librustjrt::{CompactHeap, CompactMeta, FieldKind};

const N: usize = 8_000_000;

fn main() {
    let meta = CompactMeta { name: "Node", fields: vec![FieldKind::Prim, FieldKind::Ref] };
    println!("紧凑对象大小: {} 字节（头 {}）", meta.size(), librustjrt::COMPACT_HEADER);

    let mut heap = CompactHeap::new(512 << 20);
    let t = std::time::Instant::now();
    let mut root: u32 = 0; // 用 offset 当引用
    for i in 0..N {
        let o = heap.alloc(&meta);
        heap.set_slot(o, 0, i as u32);
        if i & 7 == 0 {
            heap.set_slot(o, 1, root);
            root = o as u32;
        }
    }
    let d = t.elapsed();
    println!(
        "分配      : {:>6.1} ns/对象  吞吐 {:.1} M/s  用 {:.1} MB（{} 字节/对象）",
        d.as_nanos() as f64 / N as f64,
        N as f64 / d.as_secs_f64() / 1e6,
        heap.used_bytes() as f64 / 1048576.0,
        meta.size()
    );
    println!("存活 {} 个", heap.live_count());
}
