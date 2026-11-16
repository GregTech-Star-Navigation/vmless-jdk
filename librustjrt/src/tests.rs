//! 单元测试：多类、根集、写屏障、晋升阈值、LOS、TLAB、增长、压缩、统计、兼容层。

use crate::*;

fn small_cfg() -> HeapConfig {
    HeapConfig {
        initial: 4 << 20,
        min_capacity: 4 << 20,
        max: 128 << 20,
        ..Default::default()
    }
}

fn node_heap(tenuring: u32) -> (ManagedHeap, u32) {
    let mut heap = ManagedHeap::new(HeapConfig { tenuring_threshold: tenuring, ..small_cfg() });
    let node = heap.register_object("Node", &[SlotKind::Prim, SlotKind::Ref]);
    (heap, node)
}

// ===================== 多类对象 =====================

#[test]
fn gc_follows_each_objects_own_layout() {
    let mut heap = ManagedHeap::new(small_cfg());
    let node = heap.register_object("Node", &[SlotKind::Prim, SlotKind::Ref]);
    let leaf = heap.register_object("Leaf", &[SlotKind::Prim]); // 只有 Prim，没有引用槽
    let scope = heap.root_scope();

    let a = scope.root(heap.alloc(node));
    let b = scope.root(heap.alloc(node));
    let c = scope.root(heap.alloc(leaf));
    heap.set_i32(a, 0, 111);
    heap.set_i32(b, 0, 222);
    heap.set_i32(c, 0, 333);
    heap.set_ref(a, 1, b);
    heap.set_ref(b, 1, c); // 引用落在 Node 的 slot 1

    heap.minor();

    let a2 = scope.handle(0);
    let b2 = scope.handle(1);
    let c2 = scope.handle(2);
    assert_eq!(heap.get_i32(a2, 0), 111);
    assert_eq!(heap.get_i32(b2, 0), 222);
    assert_eq!(heap.get_i32(c2, 0), 333);
    assert_eq!(heap.get_ref(a2, 1).raw(), b2.raw());
    assert_eq!(heap.get_ref(b2, 1).raw(), c2.raw());
    assert_eq!(heap.stats().collected_objects, 0);
}

#[test]
fn prim_slot_is_not_traced() {
    // 只有 Prim 槽的类：槽里存的东西看着像偏移也不能被当成引用
    let mut heap = ManagedHeap::new(small_cfg());
    let prim = heap.register_object("Prim", &[SlotKind::Prim]);
    let holder = heap.register_object("Holder", &[SlotKind::Prim]);
    let scope = heap.root_scope();
    let h = scope.root(heap.alloc(holder));
    let victim = heap.alloc(prim); // 不 root
    heap.write_slot(h, 0, victim.raw());

    heap.minor();

    assert_eq!(heap.stats().collected_objects, 1, "Prim 槽里的值被当成引用了");
    assert_eq!(heap.read_slot(scope.handle(0), 0), victim.raw());
}

#[test]
fn array_reference_elements_are_traced() {
    let mut heap = ManagedHeap::new(small_cfg());
    let node = heap.register_object("Node", &[SlotKind::Prim]);
    let arr = heap.register_array("[LNode;", 4, true);
    let scope = heap.root_scope();
    let a = scope.root(heap.alloc_array(arr, 4));
    for i in 0..4u32 {
        let k = heap.alloc(node);
        heap.set_i32(k, 0, (i * 11) as i32);
        heap.set_ref(a, i, k);
    }
    heap.minor();
    let a2 = scope.handle(0);
    for i in 0..4u32 {
        let k = heap.get_ref(a2, i);
        assert!(!k.is_null(), "数组元素 {i} 被回收了");
        assert_eq!(heap.get_i32(k, 0), (i * 11) as i32);
    }
}

// ===================== 根集 =====================

#[test]
fn root_scope_is_raii() {
    let mut heap = ManagedHeap::new(small_cfg());
    let node = heap.register_object("Node", &[SlotKind::Prim]);
    assert_eq!(heap.roots_len(), 0);
    {
        let s1 = heap.root_scope();
        s1.root(heap.alloc(node));
        assert_eq!(heap.roots_len(), 1);
        {
            let s2 = heap.root_scope();
            s2.root(heap.alloc(node));
            s2.root(heap.alloc(node));
            assert_eq!(heap.roots_len(), 3);
        }
        assert_eq!(heap.roots_len(), 1, "内层作用域 drop 后应弹回外层水位");
    }
    assert_eq!(heap.roots_len(), 0);
}

#[test]
fn with_roots_updates_shadow_stack() {
    let mut heap = ManagedHeap::new(small_cfg());
    let node = heap.register_object("Node", &[SlotKind::Prim]);
    let h = heap.alloc(node);
    heap.with_roots(|roots| roots.push(h.raw()));
    heap.minor();
    let moved = heap.with_roots(|roots| roots[0]);
    assert_ne!(moved, 0);
    assert_ne!(moved, h.raw(), "对象被搬动后影子栈里的偏移应被改写");
    assert_eq!(heap.class_of(Handle(moved)), node);
    heap.with_roots(|roots| roots.clear());
    assert_eq!(heap.roots_len(), 0);
}

// ===================== 写屏障 + 卡表（关键） =====================

#[test]
fn write_barrier_keeps_old_to_young_reference() {
    let (mut heap, node) = node_heap(2);
    let scope = heap.root_scope();
    let old = scope.root(heap.alloc(node));
    heap.set_i32(old, 0, 7);
    heap.minor(); // 年龄 1，进 survivor
    heap.minor(); // 年龄 2，晋升
    let old = scope.handle(0);
    assert_eq!(heap.old_live_count(), 1, "阈值为 2 时两轮 minor 应晋升");

    let young = heap.alloc(node);
    heap.set_i32(young, 0, 42);
    heap.set_ref(old, 1, young); // 老 → 年轻：写屏障应当标脏 old 所在的卡
    assert_eq!(heap.stats().card_dirty, 1, "老 → 年轻 的写入必须标脏卡");

    heap.minor();

    let child = heap.get_ref(old, 1);
    assert!(!child.is_null(), "老 → 年轻 的引用丢了：minor 的根集没包含脏卡里的对象");
    assert_eq!(heap.get_i32(child, 0), 42);
    assert!(heap.spaces().is_young(child.raw()));
    assert_eq!(
        heap.stats().collected_objects,
        0,
        "有屏障时这一轮不该回收任何对象（否则读到的 42 只是 eden 里的旧字节）"
    );
    assert_eq!(heap.stats().card_dirty, 1, "仍然指向年轻代 → 卡要继续脏");
}

#[test]
fn without_barrier_young_is_wrongly_collected() {
    // 反向实验：绕过屏障（不标脏）后，年轻对象必然被回收 —— 证明上一条测的确实是屏障
    let (mut heap, node) = node_heap(2);
    let scope = heap.root_scope();
    let _old = scope.root(heap.alloc(node));
    heap.minor();
    heap.minor();
    let old = scope.handle(0);
    assert_eq!(heap.old_live_count(), 1);

    let young = heap.alloc(node);
    heap.set_i32(young, 0, 42);
    heap.set_ref_raw(old, 1, young); // 有意绕过写屏障
    heap.minor();

    assert_eq!(
        heap.stats().collected_objects,
        1,
        "绕过屏障后年轻对象应当被错误回收（反向实验）"
    );
}

// ===================== 晋升 =====================

#[test]
fn survivors_promote_at_tenuring_threshold() {
    let mut heap = ManagedHeap::new(HeapConfig { tenuring_threshold: 3, ..small_cfg() });
    let node = heap.register_object("Node", &[SlotKind::Prim]);
    let scope = heap.root_scope();
    scope.root(heap.alloc(node));
    for round in 1..=3 {
        heap.minor();
        if round < 3 {
            assert_eq!(heap.old_live_count(), 0, "第 {round} 轮还不该晋升");
        }
    }
    assert_eq!(heap.old_live_count(), 1, "第 3 轮应当晋升");
    assert_eq!(heap.stats().promoted_objects, 1);
}

// ===================== LOS =====================

#[test]
fn large_objects_go_to_los_and_are_not_copied() {
    let mut heap = ManagedHeap::new(HeapConfig { los_threshold: 256, ..small_cfg() });
    let node = heap.register_object("Node", &[SlotKind::Prim, SlotKind::Ref]);
    let arr = heap.register_array("[LNode;", 4, true);
    let scope = heap.root_scope();
    let big = scope.root(heap.alloc_array(arr, 100)); // 16 + 400 = 416 ≥ 256 → LOS
    assert_eq!(heap.los_live_count(), 1, "大对象应当进 LOS");
    let before = big.raw();

    let child = heap.alloc(node);
    heap.set_i32(child, 0, 5);
    heap.set_ref(big, 3, child);

    heap.minor();

    assert_eq!(scope.handle(0).raw(), before, "LOS 对象不该被复制搬走");
    assert_eq!(heap.los_live_count(), 1);
    let moved = heap.get_ref(big, 3);
    assert!(!moved.is_null(), "LOS 对象的引用槽没被修正");
    assert_eq!(heap.get_i32(moved, 0), 5);

    drop(scope);
    heap.major();
    assert_eq!(heap.los_live_count(), 0, "LOS 对象无人引用后应被 major 回收");
}

// ===================== TLAB =====================

#[test]
fn tlab_bumps_without_exclusive_borrow() {
    let mut heap = ManagedHeap::new(small_cfg());
    assert!(heap.tlab_alloc(24).is_none(), "TLAB 未划出时应当返回 None");
    assert!(heap.refill_tlab());

    let mut n = 0usize;
    let mut prev = 0u32;
    while let Some(h) = heap.tlab_alloc(24) {
        assert!(h.raw() > prev, "TLAB 分配必须单调");
        prev = h.raw();
        n += 1;
    }
    assert!(n >= 100, "一块 TLAB 至少能放 100 个 24 字节对象，实际 {n}");
    assert!(heap.tlab_used() >= n as u32 * 24);

    // 再切一块能接着分配
    assert!(heap.refill_tlab());
    let h2 = heap.tlab_alloc(24).expect("refill 之后应当能分配");
    assert!(h2.raw() > prev);
}

#[test]
fn eden_offsets_are_reused_after_minor() {
    let (mut heap, node) = node_heap(3);
    let scope = heap.root_scope();
    let first = scope.root(heap.alloc(node)).raw();
    for _ in 0..10_000 {
        let _ = heap.alloc(node);
    }
    assert!(heap.eden_used() > 0);
    heap.minor();
    assert_eq!(heap.eden_used(), 0, "minor 之后 eden 归零");
    let after = scope.root(heap.alloc(node)).raw();
    assert_eq!(after, first, "eden 应当从同一偏移重新开始");
}

// ===================== 增长 / 收缩 =====================

#[test]
fn heap_grows_under_pressure_and_keeps_offsets_valid() {
    let mut heap = ManagedHeap::new(HeapConfig {
        initial: 4 << 20,
        min_capacity: 4 << 20,
        max: 128 << 20,
        ..Default::default()
    });
    let node = heap.register_object("Node", &[SlotKind::Prim, SlotKind::Ref]);
    let cap0 = heap.capacity();
    let scope = heap.root_scope();
    for i in 0..1_000_000u32 {
        let h = heap.alloc(node);
        heap.set_i32(h, 0, i as i32);
        if i % 8 == 0 {
            scope.root(h);
        }
    }
    assert!(
        heap.capacity() > cap0,
        "压力下堆应当增长：{cap0} → {}",
        heap.capacity()
    );
    // realloc 搬块之后偏移必须仍然有效
    for k in 0..64 {
        let h = scope.handle(k);
        assert_eq!(heap.get_i32(h, 0), (k * 8) as i32, "第 {k} 个根的内容坏了");
    }
    assert_eq!(scope.len(), 125_000);
}

#[test]
fn shrink_to_fit_keeps_live_objects() {
    let mut heap = ManagedHeap::new(HeapConfig {
        initial: 16 << 20,
        min_capacity: 4 << 20,
        max: 128 << 20,
        ..Default::default()
    });
    let node = heap.register_object("Node", &[SlotKind::Prim, SlotKind::Ref]);
    let scope = heap.root_scope();
    for i in 0..10_000u32 {
        let h = scope.root(heap.alloc(node));
        heap.set_i32(h, 0, i as i32);
    }
    heap.grow_capacity(64 << 20);
    let grown = heap.capacity();
    heap.minor();
    heap.shrink_to_fit();
    assert!(heap.capacity() <= grown);
    for k in 0..64 {
        let h = scope.handle(k);
        assert_eq!(heap.get_i32(h, 0), k as i32);
    }
}

// ===================== major / 压缩 =====================

#[test]
fn major_frees_unreachable_old_objects() {
    let (mut heap, node) = node_heap(1);
    let scope = heap.root_scope();
    for _ in 0..10 {
        scope.root(heap.alloc(node));
    }
    heap.minor();
    assert_eq!(heap.old_live_count(), 10, "10 个对象应当全部晋升");
    // 去掉后 5 个根（它们变垃圾）
    for _ in 0..5 {
        heap.pop_root();
    }
    assert_eq!(heap.roots_len(), 5);
    let rep = heap.major();
    assert_eq!(rep.freed, 5);
    assert_eq!(heap.old_live_count(), 5);
    assert_eq!(scope.len(), 5);
}

#[test]
fn compaction_moves_objects_and_fixes_references() {
    let (mut heap, node) = node_heap(1);
    {
        let s = heap.root_scope();
        for i in 0..10u32 {
            let h = s.root(heap.alloc(node));
            heap.set_i32(h, 0, i as i32);
        }
        heap.minor(); // 10 个全晋升
    }
    // 只留偶数位，制造空洞
    let scope = heap.root_scope();
    for i in 0..5 {
        let off = heap.old_objects()[i * 2];
        scope.root(Handle(off));
    }
    heap.major();
    assert_eq!(heap.old_live_count(), 5);
    let before = heap.old_objects().to_vec();

    let rep = heap.compact();
    assert!(rep.compacted);
    assert_eq!(heap.old_live_count(), 5);
    let after = heap.old_objects().to_vec();
    assert_ne!(before, after, "压缩应当改变存活对象的偏移");

    // 内容仍在（payload = 原来的偶数下标）
    let mut seen: Vec<i32> = after.iter().map(|&o| heap.get_i32(Handle(o), 0)).collect();
    seen.sort_unstable();
    assert_eq!(seen, vec![0, 2, 4, 6, 8]);
    // 根表被重映射，指向压缩后的对象
    for k in 0..5 {
        let h = scope.handle(k);
        assert!(heap.get_i32(h, 0) <= 8);
    }
}

// ===================== SATB =====================

#[test]
fn satb_barrier_retains_overwritten_reference() {
    let mut heap = ManagedHeap::new(HeapConfig {
        barrier: BarrierKind::Satb,
        tenuring_threshold: 2,
        ..small_cfg()
    });
    let node = heap.register_object("Node", &[SlotKind::Prim, SlotKind::Ref]);
    let scope = heap.root_scope();
    scope.root(heap.alloc(node));
    heap.minor();
    heap.minor();
    let old = scope.handle(0);
    assert_eq!(heap.old_live_count(), 1);

    let young = heap.alloc(node);
    heap.set_i32(young, 0, 99);
    heap.set_ref(old, 1, young); // 老 → 年轻
    heap.set_ref(old, 1, Handle::NULL); // 覆盖掉：SATB 应记下旧值
    assert!(heap.stats().satb_len > 0, "SATB 应当记下被覆盖的年轻引用");

    heap.minor();
    assert_eq!(heap.stats().collected_objects, 0, "SATB 缓冲里的对象本轮不该被回收");
}

// ===================== 统计 =====================

#[test]
fn stats_report_gc_activity() {
    let mut heap = ManagedHeap::new(small_cfg());
    let node = heap.register_object("Node", &[SlotKind::Prim]);
    let scope = heap.root_scope();
    scope.root(heap.alloc(node));
    for _ in 0..100 {
        let _ = heap.alloc(node);
    }
    let rep = heap.minor();
    let st = heap.stats();
    assert_eq!(st.minor_count, 1);
    assert_eq!(rep.collected, 100);
    assert_eq!(st.collected_objects, 100);
    assert_eq!(st.promoted_objects, 0);
    assert!(st.last_pause_ns > 0);
    assert_eq!(st.eden_used, 0);
    assert_eq!(st.young_used, rep.survived_bytes as u32);
    assert_eq!(st.capacity, heap.capacity());
    assert_eq!(heap.history().count(), 1);
}

#[test]
fn collect_policy_runs_major_when_old_is_full() {
    let mut heap = ManagedHeap::new(HeapConfig {
        tenuring_threshold: 1,
        major_ratio: 0.01, // 老年代几乎一有东西就算「吃紧」
        ..small_cfg()
    });
    let node = heap.register_object("Node", &[SlotKind::Prim]);
    let scope = heap.root_scope();
    for _ in 0..2000 {
        scope.root(heap.alloc(node));
    }
    heap.minor();
    assert!(heap.old_live_count() > 0);
    assert!(heap.old_ratio() >= 0.01, "老年代占比 {} 应当超过阈值", heap.old_ratio());
    heap.collect(); // 老年代占比超阈值 → 应当跑 major
    assert!(heap.stats().major_count >= 1);
}

// ===================== 压力（随机图 + 反复回收） =====================

#[test]
fn stress_random_graph_survives_repeated_gc() {
    // 固定种子的 LCG，不引外部依赖
    let mut seed: u64 = 0x1234_5678;
    let mut rnd = move |m: u64| -> u64 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) % m
    };

    let mut heap = ManagedHeap::new(HeapConfig {
        initial: 8 << 20,
        min_capacity: 8 << 20,
        max: 256 << 20,
        tenuring_threshold: 2,
        ..Default::default()
    });
    let node = heap.register_object("Node", &[SlotKind::Prim, SlotKind::Ref]);
    let junk = heap.register_object("Junk", &[SlotKind::Prim]);
    let n = 2000usize;

    let scope = heap.root_scope();
    for i in 0..n {
        let h = scope.root(heap.alloc(node));
        heap.set_i32(h, 0, i as i32);
    }
    // 每个对象指向一个下标更小的对象：形成一张有环有链的图
    for i in 1..n {
        let target = rnd(i as u64) as usize;
        let hi = scope.handle(i);
        heap.set_ref(hi, 1, scope.handle(target));
    }

    for round in 0..30u32 {
        // 每轮先制造一批垃圾
        for k in 0..500u32 {
            let g = heap.alloc(junk);
            heap.set_i32(g, 0, k as i32);
        }
        match round % 4 {
            0 => {
                heap.minor();
            }
            1 => {
                heap.major();
            }
            2 => heap.collect(),
            _ => {
                heap.compact();
            }
        }
        // 所有活对象：内容必须还是自己的下标，引用必须仍指向活对象
        for i in 0..n {
            let h = scope.handle(i);
            assert_eq!(
                heap.get_i32(h, 0),
                i as i32,
                "第 {round} 轮：对象 {i} 的内容被写坏了"
            );
            let r = heap.get_ref(h, 1);
            if !r.is_null() {
                let p = heap.get_i32(r, 0);
                assert!(
                    (0..n as i32).contains(&p),
                    "第 {round} 轮：对象 {i} 的引用指向了垃圾（读到 {p}）"
                );
            }
        }
    }
    assert_eq!(scope.len(), n);
}

// ===================== 兼容层 =====================

#[test]
fn legacy_api_still_works() {
    let m = ClassMeta {
        name: "Node".into(),
        fields: vec![FieldKind::Ref, FieldKind::Prim],
    };
    let mut h = Heap::new(1 << 20);
    let root = h.alloc(&m);
    let a = h.alloc(&m);
    let b = h.alloc(&m);
    h.set_ref(root, 0, a);
    h.set_ref(a, 0, b);
    h.collect(&[root], &[&m]);
    assert_eq!(h.live_count(), 3);
    h.set_ref(root, 0, std::ptr::null_mut());
    h.collect(&[root], &[&m]);
    assert_eq!(h.live_count(), 1);

    // BumpArena 仍然一块大内存、单调 bump
    let arena = BumpArena::new(1 << 16);
    let o = arena.alloc_object(4).expect("arena");
    assert_eq!(arena.used(), 48);
    unsafe { assert_eq!((*o).field_len, 4) };

    // CompactHeap + 增量 GC
    // （先垫一个对象：老实现把偏移 0 当 null，第一个对象不能当根 —— 这是旧 API 的既有限制，
    //   这里只验证兼容层还能跑，不改变它的语义。）
    let cm = CompactMeta {
        name: "Node",
        fields: vec![FieldKind::Prim, FieldKind::Ref],
    };
    let mut ch = CompactHeap::new(1 << 20);
    let _pad = ch.alloc(&cm);
    let r = ch.alloc(&cm);
    let c = ch.alloc(&cm);
    ch.set_slot(r, 1, ch.offset_of(c));
    let mut st = IncrementalState::default();
    ch.begin_cycle(&mut st, &[ch.offset_of(r)]);
    while ch.step(&mut st, &cm, 32) {}
    assert_eq!(ch.live_count(), 2);

    // 老的分代实现
    let mut gh = crate::gen::GenerationalHeap::new(1 << 20);
    let g1 = gh.alloc(&cm);
    let g2 = gh.alloc(&cm);
    gh.set_slot(g1, 1, g2);
    let mut roots = [g1];
    let st = gh.minor(&mut roots, &cm);
    assert_eq!(st.copied, 2);
    assert_eq!(gh.old_live_count(), 2);
}
