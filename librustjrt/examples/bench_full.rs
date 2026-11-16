//! 完整内存层基准：8M 次分配、1/8 存活，随后一轮 minor（全部晋升）。
//!
//! 旧基线（供对照）：ALLOC_NS≈4.0、MINOR_MS≈35.8、MAX_STEP_MS≈1.36。

use librustjrt::{Handle, HeapConfig, ManagedHeap, SlotKind};
use std::time::Instant;

const N: usize = 8_000_000;

fn main() {
    // eden 必须装得下 8M × 24B = 192MB，这样只跑一轮 minor，和旧数据同口径
    let mut heap = ManagedHeap::new(HeapConfig {
        initial: 512 << 20,
        max: 2 << 30,
        tenuring_threshold: 1, // 一轮全部晋升，PROMOTED 才有意义
        ..Default::default()
    });
    let node = heap.register_object("Node", &[SlotKind::Prim, SlotKind::Ref]);
    let obj_bytes = heap.layout().class(node).size;
    println!("# Node = {obj_bytes} 字节（头 8 + 2×4，8 字节对齐）");

    // ---------- 分配阶段 ----------
    let mut root = Handle::NULL;
    let cap_before = heap.capacity();
    let t = Instant::now();
    for i in 0..N {
        let o = heap.alloc(node);
        heap.set_i32(o, 0, i as i32);
        if i & 7 == 0 {
            heap.set_ref(o, 1, root);
            root = o;
        }
    }
    let alloc = t.elapsed();
    let alloc_ns = alloc.as_nanos() as f64 / N as f64;

    // ---------- 一轮 minor ----------
    let scope = heap.root_scope();
    scope.root(root);
    let eden_mb = heap.eden_used() as f64 / 1048576.0;
    let t = Instant::now();
    let rep = heap.minor();
    let minor = t.elapsed();
    let root_after = scope.handle(0);

    // 链完整性抽查：1/8 存活 = 1,000,001 个对象
    let mut walk = 0usize;
    let mut cur = root_after;
    while !cur.is_null() && walk < 16 {
        walk += 1;
        cur = heap.get_ref(cur, 1);
    }

    let st = heap.stats();

    // ---------- 老年代一轮 major（附加信息） ----------
    let t = Instant::now();
    heap.major();
    let major = t.elapsed();

    println!("ALLOC_NS={alloc_ns:.1}");
    println!("MINOR_MS={:.1}", minor.as_secs_f64() * 1e3);
    println!("PROMOTED={}", rep.promoted);
    println!("OLD_LIVE={}", heap.old_live_count());
    println!("MAX_STEP_MS={:.3}", rep.max_step_ns as f64 / 1e6);
    // ---- 附加 ----
    println!("MAJOR_MS={:.1}", major.as_secs_f64() * 1e3);
    println!("EDEN_MB={eden_mb:.1}");
    println!("PROMOTED_MB={:.1}", rep.promoted_bytes as f64 / 1048576.0);
    println!("OLD_MB={:.1}", heap.old_live_bytes() as f64 / 1048576.0);
    println!("CAPACITY_MB={:.1}", heap.capacity() as f64 / 1048576.0);
    println!("CAPACITY_BEFORE_MB={:.1}", cap_before as f64 / 1048576.0);
    println!("GROWTHS={}", st.growth_events);
    println!("COPIED={}", rep.copied);
    println!("COLLECTED={}", rep.collected);
    println!("MINOR_COUNT={}", st.minor_count);
    println!("TOTAL_PAUSE_MS={:.1}", st.total_pause_ns as f64 / 1e6);
    println!("CHAIN_HEAD={walk}");
}
