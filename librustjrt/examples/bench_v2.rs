//! v2 优化对比：4 字节对齐、尺寸分桶空闲表、整体重置快路径。
use librustjrt::{CompactHeap, CompactMeta, FieldKind};

fn main() {
    let one = CompactMeta { name: "One", fields: vec![FieldKind::Prim] };
    let two = CompactMeta { name: "Two", fields: vec![FieldKind::Prim, FieldKind::Ref] };
    println!("单字段对象 {} 字节（对齐 4）", one.size());
    println!("双字段对象 {} 字节", two.size());

    // 混合尺寸分配：5M 单字段 + 3M 双字段
    let mut heap = CompactHeap::new(512 << 20);
    let t = std::time::Instant::now();
    for i in 0..5_000_000u32 {
        let o = heap.alloc(&one);
        heap.set_slot(o, 0, i);
    }
    for i in 0..3_000_000u32 {
        let o = heap.alloc(&two);
        heap.set_slot(o, 0, i);
    }
    let d = t.elapsed();
    let n = 8_000_000u32;
    println!(
        "混合分配  : {:>6.1} ns/对象  吞吐 {:.1} M/s  占用 {:.1} MB",
        d.as_nanos() as f64 / n as f64,
        n as f64 / d.as_secs_f64() / 1e6,
        heap.used_bytes() as f64 / 1048576.0
    );

    // 全量回收（无根，全部可回收）
    let t = std::time::Instant::now();
    let mut st = librustjrt::IncrementalState::default();
    heap.begin_cycle(&mut st, &[]);
    let mut more = true;
    while more {
        more = heap.step(&mut st, &two, 2000);
    }
    let gc = t.elapsed();
    println!(
        "全量回收  : {:.1} ms  回收 {}  存活 {}  最大单步 {:.3} ms",
        gc.as_secs_f64() * 1e3,
        heap.collected,
        heap.live_count(),
        st.max_step_ns as f64 / 1e6
    );

    // 复用：再分配 5M 单字段对象，应当立刻命中空闲桶
    let t = std::time::Instant::now();
    for i in 0..5_000_000u32 {
        let o = heap.alloc(&one);
        heap.set_slot(o, 0, i);
    }
    let d = t.elapsed();
    println!(
        "复用分配  : {:>6.1} ns/对象  吞吐 {:.1} M/s",
        d.as_nanos() as f64 / 5_000_000f64,
        5_000_000f64 / d.as_secs_f64() / 1e6
    );
}
