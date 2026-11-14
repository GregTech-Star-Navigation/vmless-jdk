//! 端到端：.java → 解析 → 符号表 → 降级为 Rust 运行时类元数据 → 堆分配 → 标记清扫。
//!
//! 这条链路打通意味着"源码里的引用关系"能直接驱动 Rust 侧的内存回收。

use classfile::{lower, parser, sema};
use librustjrt::Heap;

const SRC: &str = r#"
class Node {
    int value;
    Node next;
}

class Holder {
    Node head;
    String name;
}
"#;

fn main() {
    // 1) 源码 → 符号表
    let cu = parser::parse_compilation_unit(SRC).expect("parse");
    let st = sema::SymbolTable::build(&cu);

    // 2) 符号表 → Rust 运行时类元数据（引用位图）
    let node_ti = st.get("Node").expect("Node");
    let holder_ti = st.get("Holder").expect("Holder");
    let node_meta = lower::class_meta(node_ti);
    let holder_meta = lower::class_meta(holder_ti);

    println!("Node   : 字段位图 {:?}，对象大小 {} 字节", node_meta.fields, node_meta.size());
    println!("Holder : 字段位图 {:?}，对象大小 {} 字节", holder_meta.fields, holder_meta.size());

    // 3) 在 Rust 堆上按这些元数据分配
    let mut heap = Heap::new(1 << 20);
    let holder = heap.alloc(&holder_meta);
    let n1 = heap.alloc(&node_meta);
    let n2 = heap.alloc(&node_meta);
    let n3 = heap.alloc(&node_meta);
    heap.set_ref(holder, 0, n1); // Holder.head = n1
    heap.set_ref(n1, 1, n2);     // n1.next = n2
    heap.set_ref(n2, 1, n3);     // n2.next = n3
    println!("分配 4 个对象，活对象数 {}", heap.live_count());

    // 4) 从 roots 出发回收：全部可达
    heap.collect(&[holder], &[&holder_meta, &node_meta]);
    println!("第一次清扫：回收 {} 个，存活 {}", heap.collected, heap.live_count());

    // 5) 断开 head，整条 Node 链变成垃圾
    heap.set_ref(holder, 0, std::ptr::null_mut());
    heap.collect(&[holder], &[&holder_meta, &node_meta]);
    println!("断开 head 后清扫：累计回收 {} 个，存活 {}", heap.collected, heap.live_count());

    assert_eq!(heap.live_count(), 1, "只剩 Holder 一个活对象");
    assert_eq!(heap.collected, 3, "三个 Node 全部回收");

    // 6) 回收的槽位可以复用
    let before = heap.arena_used();
    let n4 = heap.alloc(&node_meta);
    let after = heap.arena_used();
    println!("复用检查：再分配 1 个 Node，arena 用量 {} → {}（相同表示复用空闲槽）", before, after);
    assert_eq!(before, after);
    heap.set_ref(holder, 0, n4);

    println!("端到端通过：源码里的引用关系直接驱动了 Rust 侧的回收。");
}
