//! 增量回收：把 8M 对象 / 1M 存活的回收切成小片，量最大单步停顿。
use librustjrt::{CompactHeap, CompactMeta, FieldKind, IncrementalState};

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

    for budget in [200usize, 2_000, 20_000] {
        let mut st = IncrementalState::default();
        let r = if root.is_null() { 0 } else { heap.offset_of(root) };
        let t = std::time::Instant::now();
        heap.begin_cycle(&mut st, &[r]);
        let mut more = true;
        while more {
            more = heap.step(&mut st, &meta, budget);
        }
        let total = t.elapsed();
        println!(
            "budget={:>6}：{} 步，累计 {:.1} ms，**最大单步 {:.3} ms**，存活 {}",
            budget,
            st.steps,
            total.as_secs_f64() * 1e3,
            st.max_step_ns as f64 / 1e6,
            heap.live_count()
        );

    }
}
