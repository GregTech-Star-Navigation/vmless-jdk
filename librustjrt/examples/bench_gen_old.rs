//! 分代两段式：minor（年轻代复制）+ major（老年代标记清扫）
use librustjrt::gen::GenerationalHeap;
use librustjrt::{CompactMeta, FieldKind};

fn main() {
    let meta = CompactMeta { name: "Node", fields: vec![FieldKind::Prim, FieldKind::Ref] };
    let mut h = GenerationalHeap::new(768 << 20);
    // 第一轮：分配 4M，1/8 存活
    let mut root: u32 = 0;
    for i in 0..4_000_000u32 {
        let o = h.alloc(&meta);
        h.set_slot(o, 0, i);
        if i & 7 == 0 { h.set_slot(o, 1, root); root = o; }
    }
    let mut roots = [root];
    let t = std::time::Instant::now();
    let s1 = h.minor(&mut roots, &meta);
    println!("minor#1 {:.1} ms  复制 {}  老年代 {} 个", t.elapsed().as_secs_f64() * 1e3, s1.copied, h.old_live_count());

    // 第二轮：再分配 4M，另一条链
    let mut root2: u32 = 0;
    for i in 0..4_000_000u32 {
        let o = h.alloc(&meta);
        h.set_slot(o, 0, i);
        if i & 7 == 0 { h.set_slot(o, 1, root2); root2 = o; }
    }
    let mut both = [roots[0], root2];
    let t = std::time::Instant::now();
    let s2 = h.minor(&mut both, &meta);
    println!("minor#2 {:.1} ms  复制 {}  老年代 {} 个", t.elapsed().as_secs_f64() * 1e3, s2.copied, h.old_live_count());

    // 只保留第二轮的链为根，老年代里第一轮的 50 万变成垃圾
    let t = std::time::Instant::now();
    let freed = h.major(&both, &meta);
    println!("major   {:.1} ms  回收 {}  老年代存活 {} 个", t.elapsed().as_secs_f64() * 1e3, freed, h.old_live_count());
}
