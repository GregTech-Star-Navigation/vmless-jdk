//! 生产级特性的基准：压缩、增量标记、并发分配、弱引用/finalization。
//!
//! 口径与 `bench_full` 一致（8M 次分配、1/8 存活），这样两边的数字可以直接对着看。
//!
//! ```text
//! cargo run --release --example bench_prod
//! ```

use librustjrt::incremental::GcPhase;
use librustjrt::{Handle, HeapConfig, ManagedHeap, SharedHeap, SlotKind};
use std::sync::Arc;
use std::time::Instant;

fn big_cfg() -> HeapConfig {
    HeapConfig {
        initial: 512 << 20,
        max: 2 << 30,
        tenuring_threshold: 1,
        auto_compact: false, // 手动触发，才好分项计时
        ..Default::default()
    }
}

fn main() {
    // ---------------- A. 压缩 ----------------
    let mut heap = ManagedHeap::new(big_cfg());
    let node = heap.register_object("Node", &[SlotKind::Prim, SlotKind::Ref]);
    let n = 1_000_000usize;
    // 全 root → 一轮 minor 全部晋升；再撤掉一半 → 一对一的空洞
    for i in 0..n {
        let o = heap.alloc(node);
        heap.set_i32(o, 0, i as i32);
        heap.push_root(o);
    }
    heap.minor();
    let all = heap.with_roots(|r| r.clone());
    let half: Vec<u32> = all.iter().enumerate().filter(|(i, _)| i % 2 == 0).map(|(_, &o)| o).collect();
    heap.with_roots(|r| {
        r.clear();
        r.extend_from_slice(&half);
    });
    heap.major();

    let frag_before = heap.fragmentation();
    let span_before = heap.old_committed();
    let t = Instant::now();
    let rep = heap.compact();
    let compact_ms = t.elapsed().as_secs_f64() * 1e3;
    println!("# A 压缩（{} 个老对象，一半是洞）", n);
    println!("FRAG_BEFORE={frag_before:.3}");
    println!("FRAG_AFTER={:.3}", heap.fragmentation());
    println!("SPAN_MB_BEFORE={:.1}", span_before as f64 / 1048576.0);
    println!("SPAN_MB_AFTER={:.1}", heap.old_committed() as f64 / 1048576.0);
    println!("COMPACT_MS={compact_ms:.1}");
    println!("COMPACT_MOVED={}", rep.moved);
    // 校验：payload 还在
    let roots = heap.with_roots(|r| r.clone());
    let mut bad = 0usize;
    for (k, &off) in roots.iter().enumerate() {
        if heap.get_i32(Handle(off), 0) != (k * 2) as i32 {
            bad += 1;
        }
    }
    println!("COMPACT_BAD_PAYLOADS={bad}");

    // ---------------- B. 增量标记 ----------------
    let mut heap = ManagedHeap::new(big_cfg());
    let node = heap.register_object("Node", &[SlotKind::Prim, SlotKind::Ref]);
    let n = 1_000_000usize;
    heap.with_roots(|r| r.clear());
    let mut prev = Handle::NULL;
    for i in 0..n {
        let o = heap.alloc(node);
        heap.set_i32(o, 0, i as i32);
        heap.set_ref(o, 1, prev); // 串成一条链，保证全部可达
        prev = o;
    }
    heap.push_root(prev); // 只要链头
    heap.minor();
    let live_before = heap.old_live_count();

    let t = Instant::now();
    heap.begin_incremental();
    let mut steps = 0u32;
    let mut max_step_ms = 0.0f64;
    loop {
        let t1 = Instant::now();
        let r = heap.step(4096);
        let ms = t1.elapsed().as_secs_f64() * 1e3;
        if ms > max_step_ms {
            max_step_ms = ms;
        }
        steps += 1;
        if r.finished {
            break;
        }
        assert!(steps < 1_000_000, "没收敛");
    }
    let inc_ms = t.elapsed().as_secs_f64() * 1e3;
    println!("# B 增量标记（{live_before} 个老对象，预算 4096）");
    println!("INC_STEPS={steps}");
    println!("INC_TOTAL_MS={inc_ms:.1}");
    println!("INC_MAX_STEP_MS={max_step_ms:.3}");
    println!("INC_KEPT={}", heap.old_live_count());
    println!("INC_PHASE={:?}", heap.phase());
    assert_eq!(heap.phase(), GcPhase::Idle);

    // 对照：同一批对象的全停顿 major
    let mut heap2 = ManagedHeap::new(big_cfg());
    let node2 = heap2.register_object("Node", &[SlotKind::Prim, SlotKind::Ref]);
    heap2.with_roots(|r| r.clear());
    let mut prev = Handle::NULL;
    for i in 0..n {
        let o = heap2.alloc(node2);
        heap2.set_i32(o, 0, i as i32);
        heap2.set_ref(o, 1, prev);
        prev = o;
    }
    heap2.push_root(prev);
    heap2.minor();
    let t = Instant::now();
    heap2.major();
    println!("STW_MAJOR_MS={:.1}", t.elapsed().as_secs_f64() * 1e3);

    // ---------------- C. 并发分配 ----------------
    println!("# C 并发分配");
    for threads in [1usize, 2, 4] {
        let shared = Arc::new(SharedHeap::new(HeapConfig {
            initial: 256 << 20,
            min_capacity: 256 << 20,
            max: 2 << 30,
            ..Default::default()
        }));
        let node =
            shared.with_heap_mut(|h| h.register_object("Node", &[SlotKind::Prim, SlotKind::Ref]));
        let per = 2_000_000usize / threads;
        let mut joins = Vec::new();
        let t = Instant::now();
        for _ in 0..threads {
            let sh = shared.clone();
            joins.push(std::thread::spawn(move || {
                let mut acc = 0u64;
                for _ in 0..per {
                    acc += sh.alloc(node) as u64;
                }
                acc
            }));
        }
        for j in joins {
            let _ = j.join().unwrap();
        }
        let el = t.elapsed();
        let total = (per * threads) as f64;
        println!(
            "CONC_{threads}T_ALLOC_NS={:.1}",
            el.as_nanos() as f64 / total
        );
    }

    // ---------------- D. 弱引用 / finalization ----------------
    let mut heap = ManagedHeap::new(big_cfg());
    let node = heap.register_object("Node", &[SlotKind::Prim, SlotKind::Ref]);
    heap.with_roots(|r| r.clear());
    let m = 200_000usize;
    let mut weaks = Vec::with_capacity(m);
    for i in 0..m {
        let o = heap.alloc(node);
        heap.set_i32(o, 0, i as i32);
        weaks.push(heap.weak_new(o));
        if i % 2 == 0 {
            heap.push_root(o);
        }
    }
    heap.minor(); // 一半晋升；没 root 的那一半在 eden 里直接死
    let t = Instant::now();
    heap.major();
    let weak_ms = t.elapsed().as_secs_f64() * 1e3;
    let alive = weaks.iter().filter(|w| heap.weak_get(**w).is_some()).count();
    println!("# D 弱引用 / finalization");
    println!("WEAK_TOTAL={m}");
    println!("WEAK_ALIVE_AFTER_GC={alive}");
    println!("WEAK_GC_MS={weak_ms:.1}");

    let mut heap = ManagedHeap::new(big_cfg());
    let node = heap.register_object("Node", &[SlotKind::Prim, SlotKind::Ref]);
    heap.with_roots(|r| r.clear());
    for i in 0..5000 {
        let o = heap.alloc_finalizable(node);
        heap.set_i32(o, 0, i);
        heap.push_root(o);
    }
    heap.minor();
    heap.with_roots(|r| r.clear());
    heap.major();
    println!("FINALIZED_QUEUED={}", heap.finalized_pending());
    let taken = heap.drain_finalized().len();
    heap.major();
    println!("FINALIZED_TAKEN={taken}");
    println!("FINALIZED_AFTER_SECOND_GC={}", heap.finalized_pending());
    println!("OLD_LIVE_AFTER_FINALIZE={}", heap.old_live_count());
}
