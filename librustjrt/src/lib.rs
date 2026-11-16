//! Rust 版 Java 运行时的内存层。
//!
//! # 新架构（`ManagedHeap`）
//!
//! ```text
//! object.rs      对象头（8 字节：class_id + flags，字段数在 flags 高 16 位）+ 4 字节槽
//! layout.rs      类布局表：class_id → 字段位图 / 大小 / 引用位
//! alloc.rs       Region（可 realloc 的连续堆）+ TLAB bump + 按大小空闲表
//! heap.rs        空间划分、分配入口、写屏障接线、增长收缩、策略、extern "C" 入口
//! young.rs       年轻代复制式回收（eden + survivor → to-space，超龄晋升）
//! old.rs         老年代标记-清扫 + 滑动压缩（返回重映射表）
//! cards.rs       卡表 + 对象起点图（脏卡 → 对象）
//! barrier.rs     写屏障（incremental-update / satb）
//! roots.rs       RootScope（RAII）+ handle + 影子栈
//! stats.rs       每次 GC 的耗时 / 回收量 / 晋升量 / 各代占用
//! handles.rs     稳定句柄表：StableHandle → 槽位 → 偏移（压缩安全）
//! incremental.rs 增量标记状态机 + step(budget)
//! weak.rs        弱引用表 + finalization 队列
//! concurrent.rs  SharedHeap：多线程共享堆（每线程 TLAB + 原子取块）
//! ```
//!
//! ```no_run
//! use librustjrt::{ManagedHeap, HeapConfig, SlotKind, Handle};
//!
//! let mut heap = ManagedHeap::new(HeapConfig::default());
//! let node = heap.register_object("Node", &[SlotKind::Prim, SlotKind::Ref]);
//! let scope = heap.root_scope();
//! let a = scope.root(heap.alloc(node));
//! let b = scope.root(heap.alloc(node));
//! heap.set_ref(a, 1, b);           // 带写屏障
//! heap.minor();                    // b 通过 a 活下来
//! let b2 = heap.get_ref(scope.handle(0), 1);
//! assert!(!b2.is_null());
//! ```
//!
//! # 兼容层（`compat` / `gen`）
//!
//! 0.1 的旧 API 全部保留（`JavaObject`、`ClassMeta`、`Heap`、`CompactHeap`、
//! `CompactMeta`、`CompactObject`、`BumpArena`、`IncrementalState`、`GcPhase`、
//! `gen::GenerationalHeap`）。它们按老语义冻结实现，只给旧基准用 —— 新代码请用
//! `ManagedHeap`：多类、根集、写屏障、分代、LOS、TLAB、统计都在那边。

pub mod alloc;
pub mod barrier;
pub mod cards;
pub mod concurrent;
pub mod handles;
pub mod heap;
pub mod incremental;
pub mod layout;
pub mod object;
pub mod old;
pub mod roots;
pub mod stats;
pub mod weak;
pub mod young;

mod compat;
pub mod gen;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_prod;

// ---- 新架构 ----
pub use barrier::BarrierKind;
pub use cards::{CardTable, StartMap, CARD_BYTES, CARD_SHIFT};
pub use concurrent::{SharedHeap, ThreadLocalTlab};
pub use handles::{HandleTable, StableHandle};
pub use heap::{Heap as ManagedHeap, HeapConfig, Spaces};
pub use layout::{ClassLayout, LayoutTable, SlotKind};
pub use object::{Object, HEADER as OBJECT_HEADER, NULL};
pub use old::{MajorReport, OldSpace};
pub use roots::{Handle, RootScope};
pub use stats::{GcKind, GcRecord, Stats};
pub use weak::{FinalizerQueue, WeakRef, WeakTable};
pub use young::MinorReport;

// 注意：`GcPhase` 在根上是**兼容层的旧枚举**（`compat::GcPhase`，冻结）。
// 增量标记的新状态机是 `librustjrt::incremental::GcPhase`，两者语义不同，
// 所以新枚举只在本模块路径下导出，不抢根上的名字。
pub use incremental::{GcPhase as MarkPhase, Marker, StepReport};

// ---- 兼容层（旧 API，冻结）----
pub use compat::{
    BumpArena, ClassMeta, CompactHeap, CompactMeta, CompactObject, FieldKind, GcPhase, Heap,
    IncrementalState, JavaObject, MarkSweep, COMPACT_HEADER, HEADER,
};
