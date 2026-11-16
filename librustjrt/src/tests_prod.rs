//! 生产级特性的测试：压缩、增量标记、并发分配、弱引用/finalization、200 轮压力。
//!
//! 每个机制都尽量配一条**反向实验**（把机制绕过去，结论必须翻转），因为
//! 「测试跑通了」和「这条测试真的在测它」是两件事。
//!
//! 造碎片/造死对象都要注意：**没被 root 的年轻对象根本进不了老年代**
//! （minor 直接把它们丢在 eden 里），所以先全 root 晋升、再撤根，才造得出老年代空洞。

use crate::handles::StableHandle;
use crate::heap::{Heap as ManagedHeap, HeapConfig};
use crate::incremental::GcPhase;
use crate::layout::SlotKind;
use crate::roots::Handle;

fn small_cfg() -> HeapConfig {
    HeapConfig { initial: 8 << 20, min_capacity: 8 << 20, max: 256 << 20, ..Default::default() }
}

/// `[Prim, Ref]` 两槽节点，`tenuring_threshold = 1`（一轮 minor 就晋升）
fn node_heap() -> (ManagedHeap, u32) {
    let mut heap = ManagedHeap::new(HeapConfig {
        tenuring_threshold: 1,
        auto_compact: false,
        compact_ratio: 0.4,
        ..small_cfg()
    });
    let node = heap.register_object("Node", &[SlotKind::Prim, SlotKind::Ref]);
    (heap, node)
}

/// 造 `n` 个老对象（payload = i），然后把**奇数号**变成垃圾。
///
/// 返回存活对象（偶数号）的当前偏移，按 payload 升序。
fn fragmented_old_gen(heap: &mut ManagedHeap, node: u32, n: usize) -> Vec<u32> {
    heap.with_roots(|r| r.clear());
    for i in 0..n {
        let o = heap.alloc(node);
        heap.set_i32(o, 0, i as i32);
        heap.push_root(o);
    }
    heap.minor(); // 全部晋升到老年代
    let promoted = heap.with_roots(|r| r.clone());
    let kept: Vec<u32> =
        promoted.iter().enumerate().filter(|(i, _)| i % 2 == 0).map(|(_, &o)| o).collect();
    heap.with_roots(|r| {
        r.clear();
        r.extend_from_slice(&kept);
    });
    heap.major(); // 奇数号变空洞
    let now = heap.with_roots(|r| r.clone());
    assert_eq!(now.len(), n / 2);
    now
}

// ===================== A. 压缩式老年代 =====================

#[test]
fn compaction_shrinks_span_and_keeps_payloads() {
    let (mut heap, node) = node_heap();
    let n = 4000;
    let kept = fragmented_old_gen(&mut heap, node, n);

    let committed_before = heap.old_committed();
    let frag_before = heap.fragmentation();
    assert!(frag_before > 0.35, "一对一的空洞，碎片率应该很高：{frag_before}");

    // 每个存活对象建稳定句柄（压缩改了偏移，靠它们找回来）
    let stable: Vec<StableHandle> = kept.iter().map(|&off| heap.handle_new(Handle(off))).collect();

    let rep = heap.compact();
    assert!(rep.compacted);
    assert!(rep.moved > 0, "压缩应该真的搬了对象");
    assert_eq!(rep.kept, kept.len());

    let committed_after = heap.old_committed();
    assert!(
        committed_after < committed_before * 7 / 10,
        "跨度应该显著下降：{committed_before} → {committed_after}"
    );
    assert!(heap.fragmentation() < 0.1, "压缩后碎片率应该接近 0：{}", heap.fragmentation());

    for (k, s) in stable.iter().enumerate() {
        let h = heap.resolve(*s).unwrap_or_else(|| panic!("第 {k} 个存活对象在压缩后丢了"));
        assert_eq!(heap.get_i32(h, 0), (k * 2) as i32, "payload 被搬坏了");
    }
    // 根表也被就地改写了
    let roots = heap.with_roots(|r| r.clone());
    for (k, &off) in roots.iter().enumerate() {
        assert_eq!(heap.get_i32(Handle(off), 0), (k * 2) as i32);
    }
}

#[test]
fn compaction_remaps_reference_graph() {
    let (mut heap, node) = node_heap();
    let junk = heap.register_object("Junk", &[SlotKind::Prim]);
    heap.with_roots(|r| r.clear());

    // 200 个对象全部晋升，然后中间插入垃圾制造空洞
    for i in 0..200 {
        let o = heap.alloc(node);
        heap.set_i32(o, 0, i);
        heap.push_root(o);
    }
    heap.minor();
    let mut offsets = heap.with_roots(|r| r.clone());
    // 每两个保留一个 → 老年代一半是洞
    offsets = offsets.iter().enumerate().filter(|(i, _)| i % 2 == 0).map(|(_, &o)| o).collect();
    heap.with_roots(|r| {
        r.clear();
        r.extend_from_slice(&offsets);
    });
    heap.major();
    let live = heap.with_roots(|r| r.clone());
    let frag = heap.fragmentation();
    assert!(frag > 0.3, "应该已经碎了：{frag}");

    // 用保留下来的对象串成一条链：第 k 个 → 第 k-1 个
    let mut prev = Handle::NULL;
    for &off in &live {
        heap.set_ref(Handle(off), 1, prev);
        prev = Handle(off);
    }
    let _ = heap.alloc(junk);

    let rep = heap.compact();
    assert!(rep.moved > 0);

    // 顺着链走：payload 必须严格递减
    let mut cur = Handle(heap.with_roots(|r| r[r.len() - 1]));
    let mut expect = ((live.len() - 1) * 2) as i32;
    let mut steps = 0;
    while !cur.is_null() {
        assert_eq!(heap.get_i32(cur, 0), expect, "第 {steps} 步 payload 不对");
        cur = heap.get_ref(cur, 1);
        expect -= 2;
        steps += 1;
        assert!(steps <= live.len(), "链成环了");
    }
    assert_eq!(steps, live.len(), "链长度变了");
}

#[test]
fn compaction_clears_handle_of_collected_object() {
    let (mut heap, node) = node_heap();
    heap.with_roots(|r| r.clear());
    let victim = heap.alloc(node);
    heap.set_i32(victim, 0, 7);
    heap.push_root(victim);
    heap.minor();
    let s_victim = heap.handle_new(heap.with_roots(|r| Handle(r[0])));
    let keeper = heap.alloc(node);
    heap.set_i32(keeper, 0, 8);
    heap.push_root(keeper);
    heap.minor();
    let s_keeper = heap.handle_new(heap.with_roots(|r| Handle(r[1])));

    // 只留 keeper（撤根 = 只保留一个元素）
    let k = heap.with_roots(|r| r[1]);
    heap.with_roots(|r| {
        r.clear();
        r.push(k);
    });
    heap.major();
    heap.compact();

    assert!(heap.resolve(s_victim).is_none(), "已回收对象的句柄必须解析为空");
    let k = heap.resolve(s_keeper).expect("存活对象的句柄必须还能解析");
    assert_eq!(heap.get_i32(k, 0), 8);
}

#[test]
fn fragmentation_threshold_triggers_auto_compaction() {
    let mut heap = ManagedHeap::new(HeapConfig {
        tenuring_threshold: 1,
        auto_compact: true,
        compact_ratio: 0.3,
        ..small_cfg()
    });
    let node = heap.register_object("Node", &[SlotKind::Prim, SlotKind::Ref]);
    let _ = fragmented_old_gen(&mut heap, node, 2000);
    let before = heap.old_committed();
    assert!(heap.fragmentation() >= 0.3, "碎片率 {}", heap.fragmentation());
    let rep = heap.full();
    assert!(rep.compacted, "碎片率过阈值，full() 应该顺带压缩");
    assert!(heap.old_committed() <= before * 7 / 10);
    assert_eq!(heap.stats().compactions, 1);
}

// ===================== B. 增量标记 =====================

/// 400 个老对象全部 root（`chain = true` 时串成一条链）
fn old_gen(heap: &mut ManagedHeap, node: u32, n: usize) -> Vec<u32> {
    heap.with_roots(|r| r.clear());
    let mut prev = Handle::NULL;
    for i in 0..n {
        let o = heap.alloc(node);
        heap.set_i32(o, 0, i as i32);
        heap.set_ref(o, 1, prev);
        prev = o;
        heap.push_root(o);
    }
    heap.minor();
    let now = heap.with_roots(|r| r.clone());
    assert_eq!(heap.old_live_count(), n);
    now
}

#[test]
fn incremental_marking_runs_in_budgeted_steps() {
    let (mut heap, node) = node_heap();
    old_gen(&mut heap, node, 300);

    assert_eq!(heap.phase(), GcPhase::Idle);
    heap.begin_incremental();
    assert_eq!(heap.phase(), GcPhase::Marking);

    let mut steps = 0;
    let mut saw_sweeping = false;
    let report;
    loop {
        let r = heap.step(1);
        steps += 1;
        if r.phase == GcPhase::Sweeping {
            saw_sweeping = true;
        }
        if r.finished {
            report = r.report;
            break;
        }
        assert!(steps < 100_000, "增量标记没有收敛");
    }
    assert!(steps > 300, "预算 1 却只走了 {steps} 步，说好的分步呢");
    assert!(saw_sweeping, "应该观察到 Sweeping 阶段");
    assert_eq!(heap.phase(), GcPhase::Idle);
    let rep = report.expect("跑完要给报告");
    assert_eq!(rep.freed, 0, "全部可达，不该回收任何东西");
    assert_eq!(heap.old_live_count(), 300);
    let roots = heap.with_roots(|r| r.clone());
    for (i, &off) in roots.iter().enumerate() {
        assert_eq!(heap.get_i32(Handle(off), 0), i as i32);
    }
}

#[test]
fn incremental_gc_collects_unreachable_and_keeps_reachable() {
    let (mut heap, node) = node_heap();
    heap.with_roots(|r| r.clear());
    let mut keeps = Vec::new();
    for i in 0..60 {
        let o = heap.alloc(node);
        heap.set_i32(o, 0, i);
        heap.push_root(o);
        if i < 10 {
            keeps.push(i as i32);
        }
    }
    heap.minor();
    let all = heap.with_roots(|r| r.clone());
    assert_eq!(all.len(), 60);
    // 只留前 10 个：其余变垃圾（它们已经在老年代里了）
    heap.with_roots(|r| {
        r.clear();
        r.extend_from_slice(&all[..10]);
    });
    let s_dead = heap.handle_new(Handle(all[40]));

    heap.collect_incremental();
    assert_eq!(heap.old_live_count(), 10, "只该剩那 10 个 root");
    let roots = heap.with_roots(|r| r.clone());
    for (k, &off) in roots.iter().enumerate() {
        assert_eq!(heap.get_i32(Handle(off), 0), keeps[k]);
    }
    assert!(heap.resolve(s_dead).is_none(), "垃圾的稳定句柄要失效");
}

#[test]
fn satb_keeps_object_deleted_during_marking() {
    // 只 root a；a.f = d；d.f = b（b 只经 d 可达）
    let (mut heap, node) = node_heap();
    heap.with_roots(|r| r.clear());
    let a = heap.alloc(node);
    let d = heap.alloc(node);
    let b = heap.alloc(node);
    heap.set_i32(b, 0, 99);
    heap.set_ref(d, 1, b);
    heap.set_ref(a, 1, d);
    heap.push_root(a);
    heap.minor();
    let a = heap.with_roots(|r| Handle(r[0]));
    let s_b = heap.handle_new(heap.get_ref(heap.get_ref(a, 1), 1));

    heap.begin_incremental();
    // 一步：扫 a（唯一根）→ d 进灰队列，d 还没被扫，b 还没被标
    assert_eq!(heap.phase(), GcPhase::Marking);
    assert_eq!(heap.marker().work_left(), 1, "灰队列里应该只有 d，实际 {}", heap.marker().work_left());
    let d = heap.get_ref(a, 1);

    // 在 d 被扫之前删掉 d → b 这条边
    heap.set_ref(d, 1, Handle::NULL);

    heap.finish_incremental();

    // SATB 语义：b 在周期开始时活着 → 这一轮必须活下来
    let b_now = heap.resolve(s_b).expect("标记期被删掉的目标 b 不能被回收");
    assert_eq!(heap.get_i32(b_now, 0), 99);
}

#[test]
fn without_satb_deleted_target_is_lost() {
    // 反向实验：绕过写屏障删引用 → 同一个目标会被扫掉
    let (mut heap, node) = node_heap();
    heap.with_roots(|r| r.clear());
    let a = heap.alloc(node);
    let d = heap.alloc(node);
    let b = heap.alloc(node);
    heap.set_i32(b, 0, 99);
    heap.set_ref(d, 1, b);
    heap.set_ref(a, 1, d);
    heap.push_root(a);
    heap.minor();
    let a = heap.with_roots(|r| Handle(r[0]));
    let s_b = heap.handle_new(heap.get_ref(heap.get_ref(a, 1), 1));

    heap.begin_incremental();
    assert_eq!(heap.marker().work_left(), 1);
    let d = heap.get_ref(a, 1);
    heap.set_ref_raw(d, 1, Handle::NULL); // 不过屏障
    heap.finish_incremental();

    assert!(
        heap.resolve(s_b).is_none(),
        "没有删除屏障，b 应该在这轮被回收 —— 说明上一条测试确实靠屏障"
    );
}

#[test]
fn incremental_update_shades_new_reference() {
    let (mut heap, node) = node_heap();
    heap.with_roots(|r| r.clear());
    // 造 8 个老对象，a 是唯一根，其余互相指着，让标记至少要走几步
    let mut objs = Vec::new();
    for _ in 0..8 {
        objs.push(heap.alloc(node));
    }
    for w in objs.windows(2) {
        heap.set_ref(w[0], 1, w[1]);
    }
    heap.push_root(objs[0]);
    heap.minor();
    let a = heap.with_roots(|r| Handle(r[0]));
    // c：老对象、不可达（不 root、也没人指向它）
    // c 必须真的在**老年代**且不可达：先 root 让它晋升，再撤根
    let (s_c, c_off) = old_orphan(&mut heap, node, 77);
    let c = Handle(c_off);
    assert!(heap.resolve(s_c).is_some());

    heap.begin_incremental();
    assert_eq!(heap.phase(), GcPhase::Marking, "8 个对象的链，一步扫不完");
    // a 已经被扫过（它是唯一根、第一个被弹出），此刻 a.f 还指着链上的下一个
    heap.set_ref(a, 1, c);
    heap.finish_incremental();

    let c_now = heap.resolve(s_c).expect("标记期新装的引用必须被着色，不能漏标");
    assert_eq!(heap.get_i32(c_now, 0), 77);
}

#[test]
fn without_shading_new_reference_is_lost() {
    let (mut heap, node) = node_heap();
    heap.with_roots(|r| r.clear());
    let mut objs = Vec::new();
    for _ in 0..8 {
        objs.push(heap.alloc(node));
    }
    for w in objs.windows(2) {
        heap.set_ref(w[0], 1, w[1]);
    }
    heap.push_root(objs[0]);
    heap.minor();
    let a = heap.with_roots(|r| Handle(r[0]));
    let (s_c, c_off) = old_orphan(&mut heap, node, 77);
    let c = Handle(c_off);

    heap.begin_incremental();
    assert_eq!(heap.phase(), GcPhase::Marking);
    heap.set_ref_raw(a, 1, c); // 绕过着色
    heap.finish_incremental();

    assert!(heap.resolve(s_c).is_none(), "没有着色，c 应该被漏标回收");
}

#[test]
fn minor_during_marking_is_serialized() {
    let (mut heap, node) = node_heap();
    let roots = old_gen(&mut heap, node, 64);
    heap.begin_incremental();
    assert_eq!(heap.phase(), GcPhase::Marking);
    heap.step(1);
    assert_eq!(heap.phase(), GcPhase::Marking);

    heap.minor(); // 必须先把增量标记推完，否则灰队列里的偏移会野
    assert_eq!(heap.phase(), GcPhase::Idle, "minor 应该顺手收掉在跑的增量标记");
    let now = heap.with_roots(|r| r.clone());
    assert_eq!(now.len(), roots.len());
    for &off in &now {
        assert!(heap.get_i32(Handle(off), 0) >= 0);
    }
}

// ===================== C. 并发分配 =====================

use crate::concurrent::SharedHeap;
use std::collections::HashSet;
use std::sync::Arc;

fn shared_heap() -> (Arc<SharedHeap>, u32) {
    let shared = Arc::new(SharedHeap::new(HeapConfig {
        initial: 64 << 20,
        min_capacity: 64 << 20,
        max: 512 << 20,
        ..Default::default()
    }));
    let node = shared.with_heap_mut(|h| h.register_object("Node", &[SlotKind::Prim, SlotKind::Ref]));
    (shared, node)
}

#[test]
fn concurrent_allocations_are_disjoint_and_in_eden() {
    let (shared, node) = shared_heap();
    let (eden_lo, eden_hi) = shared.eden_span();

    let threads = 4;
    let per = 20_000usize;
    let mut joins = Vec::new();
    for _ in 0..threads {
        let sh = shared.clone();
        joins.push(std::thread::spawn(move || {
            let mut v = Vec::with_capacity(per);
            for _ in 0..per {
                v.push(sh.alloc(node));
            }
            v
        }));
    }
    let mut all: Vec<u32> = Vec::with_capacity(threads * per);
    for j in joins {
        all.extend(j.join().unwrap());
    }
    assert_eq!(all.len(), threads * per);
    // `alloc_count()` 是**汇总值**：每线程的计数攒在本块里，取新块时才汇总，
    // 所以线程退出时最后一块内还没汇总（每线程最多一个块的量）。
    let reported = shared.alloc_count() as usize;
    let total = threads * per;
    assert!(reported <= total);
    assert!(
        reported + threads * crate::concurrent::CHUNK as usize / 16 >= total,
        "汇总的分配次数差得太多：{reported} vs {total}"
    );

    let set: HashSet<u32> = all.iter().copied().collect();
    assert_eq!(set.len(), all.len(), "并发分配出现了重复地址");
    for &off in &all {
        assert!(off >= eden_lo && off < eden_hi, "偏移 {off} 不在 eden [{eden_lo},{eden_hi}) 里");
        assert_eq!(off % 8, 0, "对象起始地址必须 8 字节对齐");
    }
}

#[test]
fn concurrent_allocs_survive_gc_intact() {
    let (shared, node) = shared_heap();
    let threads = 4;
    let per = 5_000usize;
    let mut joins = Vec::new();
    for _ in 0..threads {
        let sh = shared.clone();
        joins.push(std::thread::spawn(move || (0..per).map(|_| sh.alloc(node)).collect::<Vec<_>>()));
    }
    let mut all = Vec::new();
    for j in joins {
        all.extend(j.join().unwrap());
    }

    shared.with_heap_mut(|h| {
        for (i, &off) in all.iter().enumerate() {
            h.set_i32(Handle(off), 0, i as i32);
            h.push_root(Handle(off));
        }
    });

    shared.minor();
    shared.full();

    let roots = shared.with_heap(|h| h.with_roots(|r| r.clone()));
    assert_eq!(roots.len(), all.len());
    let set: HashSet<u32> = roots.iter().copied().collect();
    assert_eq!(set.len(), roots.len(), "GC 之后两个根指向了同一地址");
    let payloads = shared.with_heap(|h| {
        roots.iter().map(|&off| h.get_i32(Handle(off), 0)).collect::<Vec<_>>()
    });
    for (i, v) in payloads.iter().enumerate() {
        assert_eq!(*v, i as i32, "第 {i} 个并发分配的 payload 在 GC 后不对");
    }
}

#[test]
fn concurrent_alloc_after_gc_uses_fresh_space() {
    let (shared, node) = shared_heap();
    let first: Vec<u32> = (0..1000).map(|_| shared.alloc(node)).collect();
    shared.with_heap_mut(|h| {
        for &off in &first {
            h.push_root(Handle(off));
        }
    });
    shared.minor();
    let survivors = shared.with_heap(|h| h.with_roots(|r| r.clone()));

    let second: Vec<u32> = (0..1000).map(|_| shared.alloc(node)).collect();
    let survivors_set: HashSet<u32> = survivors.iter().copied().collect();
    for &off in &second {
        assert!(!survivors_set.contains(&off), "GC 后新分配拿到了存活对象占着的地址");
    }
    let all: HashSet<u32> = survivors.iter().chain(second.iter()).copied().collect();
    assert_eq!(all.len(), survivors.len() + second.len());
}

// ===================== D. 弱引用 / finalization =====================

#[test]
fn weak_ref_is_cleared_when_target_dies() {
    let (mut heap, node) = node_heap();
    heap.with_roots(|r| r.clear());
    let live = heap.alloc(node);
    heap.set_i32(live, 0, 11);
    let doomed = heap.alloc(node);
    heap.set_i32(doomed, 0, 22);
    heap.push_root(live);
    heap.minor(); // 两个都晋升（have not been rooted 的那个也会被 promote？ 不：只复制可达）
    // doomed 没被 root → minor 之后它已经在 eden 里消失了；重新造一个老年代的垃圾
    let doomed = heap.alloc(node);
    heap.set_i32(doomed, 0, 22);
    heap.push_root(doomed);
    heap.minor();

    let live = heap.with_roots(|r| Handle(r[0]));
    let doomed = heap.with_roots(|r| Handle(r[1]));
    let w_live = heap.weak_new(live);
    let w_dead = heap.weak_new(doomed);
    // 撤掉 doomed 的根
    heap.with_roots(|r| {
        r.clear();
        r.push(live.raw());
    });
    heap.full();

    assert!(heap.weak_get(w_dead).is_none(), "目标死了，弱引用必须解析为空");
    let l = heap.weak_get(w_live).expect("强引用还活着，弱引用不该被清");
    assert_eq!(heap.get_i32(l, 0), 11);
}

#[test]
fn weak_ref_to_young_object_follows_minor_then_dies() {
    let mut heap = ManagedHeap::new(small_cfg());
    let node = heap.register_object("Node", &[SlotKind::Prim, SlotKind::Ref]);
    heap.with_roots(|r| r.clear());
    let live = heap.alloc(node);
    heap.set_i32(live, 0, 5);
    heap.push_root(live);
    let garbage = heap.alloc(node);
    let w_live = heap.weak_new(live);
    let w_garbage = heap.weak_new(garbage);

    heap.minor();

    let l = heap.weak_get(w_live).expect("年轻对象活着搬到 to-space，弱引用要跟过去");
    assert_eq!(heap.get_i32(l, 0), 5, "跟过去的地址必须是对的");
    assert!(heap.weak_get(w_garbage).is_none(), "年轻代垃圾在 minor 后弱引用应为空");
}

#[test]
fn finalizer_queue_fires_once_then_object_is_collected() {
    let (mut heap, node) = node_heap();
    heap.with_roots(|r| r.clear());
    let f = heap.alloc_finalizable(node);
    heap.set_i32(f, 0, 1234);
    heap.push_root(f);
    heap.minor(); // 晋升到老年代

    // 撤掉根：对象不可达，但下一轮 GC 只能「入队 + 复活一轮」
    heap.with_roots(|r| r.clear());
    heap.major();
    assert_eq!(heap.finalized_pending(), 1);

    let h = heap.take_finalized().expect("应该有对象进 finalization 队列");
    assert_eq!(heap.get_i32(h, 0), 1234, "复活一轮就是让 finalizer 能安全读它");
    assert!(heap.take_finalized().is_none(), "只该入队一次");
    assert_eq!(heap.finalized_pending(), 0);

    // 再一轮：这次真的回收
    heap.major();
    assert_eq!(heap.finalized_pending(), 0, "FLAG_FINALIZED 保证不会二次入队");
    assert_eq!(heap.old_live_count(), 0, "复活的那一轮结束后应该真的被回收");
}

// ===================== E. 200 轮混合压力 =====================

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 11
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[derive(Clone)]
struct ModelNode {
    handle: StableHandle,
    payload: i32,
    /// 指向 model 里其它节点的下标；`usize::MAX` = 该槽为空
    refs: Vec<usize>,
}

const EMPTY: usize = usize::MAX;

#[test]
fn stress_200_rounds_mixed_gc_modes() {
    let mut heap = ManagedHeap::new(HeapConfig {
        tenuring_threshold: 2,
        auto_compact: true,
        compact_ratio: 0.35,
        incremental_budget: 64,
        ..small_cfg()
    });
    // [Prim, Prim, Ref, Ref]：两个 payload 槽 + 两个引用槽
    let wide =
        heap.register_object("Wide", &[SlotKind::Prim, SlotKind::Prim, SlotKind::Ref, SlotKind::Ref]);
    let rng = &mut Lcg(0x5EED_1234_ABCD);
    let mut model: Vec<ModelNode> = Vec::new();
    let mut next_payload = 0i32;
    heap.with_roots(|r| r.clear());

    for round in 0..200 {
        // --- 1) 加节点 ---
        for _ in 0..rng.below(24) {
            let h = heap.alloc(wide);
            next_payload += 1;
            heap.set_i32(h, 0, next_payload);
            heap.set_i32(h, 1, -next_payload);
            // 立刻登记为根：分配本身可能触发 minor，没 root 的新对象随时会被回收
            heap.push_root(h);
            let s = heap.handle_new(h);
            model.push(ModelNode { handle: s, payload: next_payload, refs: Vec::new() });
        }
        // --- 2) 随机连边（走写屏障） ---
        if model.len() >= 2 {
            for _ in 0..rng.below(16) {
                let a = rng.below(model.len());
                let b = rng.below(model.len());
                let slot = rng.below(2);
                let ah = heap.resolve(model[a].handle).expect("活节点必须能解析");
                let bh = heap.resolve(model[b].handle).unwrap();
                heap.set_ref(ah, 2 + slot as u32, bh);
                if model[a].refs.len() <= slot {
                    model[a].refs.resize(slot + 1, EMPTY);
                }
                model[a].refs[slot] = b;
            }
        }
        verify(&heap, &model, round, 9, "·写边前");
        // --- 3) 随机删节点：先把它在别人那里的引用真正清空，再从 model 里抹掉 ---
        if model.len() > 8 && rng.below(2) == 0 {
            let victim = rng.below(model.len());
            let mut cleared: Vec<usize> = Vec::new();
            for (i, m) in model.iter().enumerate() {
                for (s, &t) in m.refs.iter().enumerate() {
                    if t == victim {
                        if let Some(h) = heap.resolve(m.handle) {
                            heap.set_ref(h, 2 + s as u32, Handle::NULL);
                        }
                        cleared.push(i * 8 + s);
                    }
                }
            }
            for code in cleared {
                model[code / 8].refs[code % 8] = EMPTY;
            }
            model.remove(victim);
            for m in model.iter_mut() {
                for r in m.refs.iter_mut() {
                    if *r != EMPTY && *r > victim {
                        *r -= 1;
                    }
                }
            }
        }
        verify(&heap, &model, round, 9, "·写边后");
        // --- 4) 重建根表（全部存活节点都是根） ---
        let mut roots: Vec<u32> = Vec::with_capacity(model.len());
        for m in &model {
            roots.push(heap.resolve(m.handle).expect("活节点必须能解析").raw());
        }
        heap.with_roots(|r| {
            r.clear();
            r.extend_from_slice(&roots);
        });
        // --- 5) 随机 GC：minor / major / full / compact / 增量 / collect ---
        let mode = rng.below(6);
        match mode {
            0 => {
                heap.minor();
            }
            1 => {
                heap.major();
            }
            2 => {
                let _ = heap.full();
            }
            3 => {
                let _ = heap.compact();
            }
            4 => {
                heap.begin_incremental();
                let mut guard = 0;
                while !heap.step(64).finished {
                    guard += 1;
                    assert!(guard < 100_000, "第 {round} 轮增量标记没收敛");
                }
            }
            _ => heap.collect(),
        }
        // 先检查布局不变式：存活对象不能重叠（重叠 = 要么双重释放要么惨案）
        check_layout(&heap, round, mode);
        // --- 6) 校验：payload 与全部引用 ---
        verify(&heap, &model, round, mode, phase_label(mode));
    }

    assert!(!model.is_empty());
    // 收尾：全部撤根再压一轮，确认没有残留标记位/多余存活
    heap.with_roots(|r| r.clear());
    heap.full();
    assert_eq!(heap.old_live_count(), 0, "全部撤根之后老年代应该清空");
    assert_eq!(heap.finalized_pending(), 0);
}

/// 造一个「老年代孤岛」：先 root 让它晋升到老年代，再撤掉它的根。
///
/// 返回它的稳定句柄与当前偏移。c 之后不可达，但是**真的在老年代里**，
/// 所以只有 `major` 能收它 —— 这才测得到标记/清扫。
fn old_orphan(heap: &mut ManagedHeap, node: u32, payload: i32) -> (StableHandle, u32) {
    let saved = heap.with_roots(|r| r.clone());
    let o = heap.alloc(node);
    heap.set_i32(o, 0, payload);
    heap.push_root(o);
    heap.minor(); // 晋升（老的根在 minor 里被就地改写）
    let all = heap.with_roots(|r| r.clone());
    let off = *all.last().unwrap();
    let restored: Vec<u32> = all[..saved.len()].to_vec();
    heap.with_roots(|r| {
        r.clear();
        r.extend_from_slice(&restored);
    });
    (heap.handle_new(Handle(off)), off)
}

/// 布局不变式：`old_objects()` 里的对象按偏移升序必须互不重叠，且都在尾部区间内。
///
/// 重叠的两种来源都有意义：① 同一个槽被释放了两次（double free），
/// ② 压缩把两个对象搬到了同一块内存上。两者都会让引用静默指错对象。
fn check_layout(heap: &ManagedHeap, round: usize, mode: usize) {
    let mut live: Vec<u32> = heap.old_objects().to_vec();
    live.sort_unstable();
    let tail = heap.old_committed();
    for w in live.windows(2) {
        let size = heap.size_of(Handle(w[0])) as usize;
        assert!(
            (w[0] as usize) + size <= w[1] as usize,
            "第 {round} 轮（mode {mode}）：对象 {:#x}(+{size}) 与 {:#x} 重叠",
            w[0],
            w[1]
        );
    }
    if let Some(&last) = live.last() {
        let size = heap.size_of(Handle(last)) as usize;
        assert!(
            (last as usize) + size <= heap.capacity(),
            "第 {round} 轮（mode {mode}）：对象 {last:#x}(+{size}) 越过堆上限（tail={tail}）"
        );
    }
}

/// 逐节点校验 payload 与全部引用槽
fn verify(heap: &ManagedHeap, model: &[ModelNode], round: usize, mode: usize, phase: &str) {
    for (i, m) in model.iter().enumerate() {
        let h = heap
            .resolve(m.handle)
            .unwrap_or_else(|| panic!("第 {round} 轮{phase}：节点 {i}（payload {}）被误回收", m.payload));
        assert_eq!(heap.get_i32(h, 0), m.payload, "第 {round} 轮{phase}：节点 {i} payload 坏了");
        assert_eq!(heap.get_i32(h, 1), -m.payload, "第 {round} 轮{phase}：节点 {i} payload2 坏了");
        for (s, &t) in m.refs.iter().enumerate() {
            let got = heap.get_ref(h, 2 + s as u32);
            if t == EMPTY {
                assert!(got.is_null(), "第 {round} 轮{phase}（mode {mode}）：节点 {i} 槽 {s} 应该为空");
            } else {
                let want = heap.resolve(model[t].handle).unwrap();
                if got.raw() != want.raw() {
                    let mut whose = String::from("未知对象");
                    for (j, m2) in model.iter().enumerate() {
                        if let Some(h2) = heap.resolve(m2.handle) {
                            if h2.raw() == got.raw() {
                                whose = format!("model[{j}] payload={}", m2.payload);
                            }
                        }
                    }
                    panic!(
                        "第 {round} 轮{phase}（mode {mode}）：节点 {i}({:#x}) 槽 {s} 指向 {:#x}（{}），期望 model[{t}]={:#x}",
                        h.raw(),
                        got.raw(),
                        whose,
                        want.raw()
                    );
                }
            }
        }
    }
}

fn phase_label(mode: usize) -> &'static str {
    match mode {
        0 => "·minor后",
        1 => "·major后",
        2 => "·full后",
        3 => "·compact后",
        4 => "·增量后",
        _ => "·collect后",
    }
}
