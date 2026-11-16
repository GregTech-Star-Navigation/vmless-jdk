# librustjrt — Rust 版 Java 运行时内存层

给 vmless 的 JIT / 解释器用的托管堆：多类对象、显式根集、写屏障 + 卡表、分代回收
（复制式年轻代 + 标记清扫老年代）、TLAB、大对象空间（LOS）、增长收缩、统计。

**状态**：生产级（`cargo test --release` 51 项 + 2 个文档测试）。除分代回收外，已有
压缩式老年代（`Heap::compact` + 稳定句柄）、增量标记（`step(budget)` + `GcPhase`）、
多线程共享堆（`SharedHeap`）、弱引用与 finalization 队列。旧的 0.1 单文件实现冻结为
兼容层（`compat.rs` / `gen.rs`），只为老基准与外部调用方保留。

## 快速开始

```bash
cargo test --release                    # 51 个单元测试 + 2 个文档测试
cargo build --release --examples        # 11 个基准示例（8 个老的 + bench_full + bench_prod）
cargo run --release --example bench_full
cargo run --release --example bench_prod   # 压缩 / 增量 / 并发 / 弱引用 分项数字
```

> Windows 上如果链接报 `LNK1104`（构建产物被带外进程占用），换一个
> `CARGO_TARGET_DIR` 即可。

## 最小用法

```rust
use librustjrt::{Handle, HeapConfig, ManagedHeap, SlotKind};

let mut heap = ManagedHeap::new(HeapConfig::default());
let node = heap.register_object("Node", &[SlotKind::Prim, SlotKind::Ref]);

let scope = heap.root_scope();            // RAII：drop 时自动注销这批根
let a = scope.root(heap.alloc(node));
let b = scope.root(heap.alloc(node));
heap.set_ref(a, 1, b);                    // 带写屏障
heap.minor();                             // 年轻代回收；b 通过 a 活下来

let a = scope.handle(0);                  // GC 后偏移可能变了：从根表重读
let b = heap.get_ref(a, 1);
assert_eq!(heap.get_i32(b, 0), 0);
```

## 模块

| 文件 | 职责 |
| --- | --- |
| `object.rs` | 对象头（8 字节：`class_id` + `flags`，字段数在 flags 高 16 位）+ 4 字节槽；数组长度占独立槽，元素从 16 字节开始 |
| `layout.rs` | 类布局表：`class_id` → 字段位图 / 大小 / 引用位 / 数组元素 |
| `alloc.rs` | `Region`（可 `realloc` 的连续堆）、TLAB bump、按精确大小的空闲表 |
| `heap.rs` | 空间划分、分配入口、写屏障接线、增长收缩、回收策略、`extern "C"` 入口 |
| `young.rs` | 复制式 minor GC（eden + survivor → to-space，超龄晋升） |
| `old.rs` | 老年代标记-清扫 + 滑动压缩（`Heap::compact`，返回重映射表） |
| `handles.rs` | 稳定句柄表：`StableHandle` → 槽位 → 偏移，压缩后批量重映射 |
| `incremental.rs` | 增量标记状态机 `GcPhase::{Idle,Marking,Sweeping}` + `step(budget)` |
| `weak.rs` | 弱引用表（`WeakRef`）+ finalization 队列 |
| `concurrent.rs` | `SharedHeap`：每线程 TLAB + 原子取块 + 写锁 GC |
| `cards.rs` | 卡表 + 对象起点图（脏卡 → 对象） |
| `barrier.rs` | 写屏障：incremental-update（默认）/ SATB |
| `roots.rs` | `RootScope`（RAII）、`Handle`、影子栈 |
| `stats.rs` | 每次 GC 的耗时 / 回收量 / 晋升量 / 各代占用 + 环形历史 |
| `compat.rs`, `gen.rs` | **冻结的 0.1 API**（见文末） |

## 内存布局

```text
0      64                                young_end                            cap
├保留──┼──── eden ────┬─semi0─┬─semi1─┼──── 老年代 + LOS（bump 向上）────┤
                     ↑ 两块 survivor 交替做 from/to
```

- 所有引用是**相对堆基址的 u32 偏移**（`0` = null）。堆增长用 `realloc` 搬整块：
  基址变了但偏移不变，所以 JIT 存在寄存器里的 handle 不会失效。
- 老年代与 LOS 共用尾部 arena，靠三张表分开记账：`live`（普通老对象）、
  `los_live`（大对象）、两张按精确大小的空闲表。LOS 的唯一特殊点是**复制式
  GC 不碰它**。
- 卡表与对象起点图只覆盖尾部（`[young_end, cap)`）—— 只有老年代才会指向年轻代。

## 关键设计决定（以及对旧实现的修正）

1. **按对象查类**：对象头里存 `class_id`，GC 追引用时按该类的位图取引用槽。
   旧实现全局共用一个 `ClassMeta`，两个不同形状的类会互相误导。
2. **显式根集**：`RootScope` + `heap.with_roots(|roots| …)` 一张偏移表，GC 就地读、
   就地改写。旧 API 让调用方在栈上拼 `Vec<*mut T>`，编译代码的根根本进不去。
3. **写屏障 + 卡表**：`set_ref` 时若「老 → 年轻」就把老对象所在卡标脏；
   `minor` 的根集 = 显式根 + SATB + **所有脏卡里的对象**（靠对象起点图把卡解析成对象）。
   旧 `gen.rs` 只扫 roots，任何 `old → young` 都会把活对象回收掉。
4. **LOS**：≥ `los_threshold`（默认 64 KB）的分配直接进老年代独立链表，不参与复制。
5. **TLAB**：`Heap::tlab_alloc(&self, size)` 用 `Cell` bump，无需 `&mut self`；
   耗尽后 `refill_tlab()` 从 eden 再切一块。
6. **增长 / 收缩**：尾部满了先真回收，再 `realloc` 翻倍到 `cfg.max`；空闲率超过
   `min_free_ratio` 时 `shrink_to_fit()` 把 `old.top()` 之上的空间还给系统。
7. **统计**：`heap.stats()` 快照 + `heap.history()` 环形记录。

## 资源与代价

- 对象头 8 字节（旧实现 16）。普通对象 `8 + 4×字段数` 对齐到 8：2 个字段 = 16 字节，
  和老基线同尺寸，所以基准可以直接对比。
- 数组：`16 + 元素数×元素大小`。长度存在头后面的独立槽里，元素因此从 16 字节开始。
- `Heap` 本身仍不是 `Sync`（单线程路径的 `ALLOC_NS≈12` 就靠这一点）。多线程用
  `SharedHeap`：每线程 TLAB 块 + `AtomicU32` 取块，GC 走写锁。实测分配成本
  `1T=32ns / 4T=88ns`（读锁 + 原子 bump 的代价，换的是 `realloc` 搬家期间的基址安全）。

## 已知边界（不是 bug，是明确不做的）

- `Heap::compact()` 会移动对象，**改变偏移**：偏移直用的 `Handle` 之后只能从根表重读。
  要跨压缩持有引用就用 `StableHandle`（`Heap::handle_new` / `resolve`）—— 句柄表在
  压缩后被重映射过，句柄本身不动。`minor()` 搬动的年轻对象同样会跟着改。
- 压缩前会先做一轮「全部晋升」的 minor：压缩只改写老对象内部的引用槽，年轻对象里
  指向老年代的引用槽不会被改写（这是踩过的坑，`stress_200_rounds_mixed_gc_modes` 逮到的）。
- 增量标记的 marking / sweeping / 卡表重建都按预算切；`minor()` 会先把在跑的
  增量回收推完（复制会搬对象，灰队列里的偏移会野）。
- 年轻代的物理划分在构造时固定；「按存活率调整年轻代大小」调的是 eden 的**软上限**
  （GC 触发点），不搬内存 —— 偏移稳定优先于这一项。
- `alloc_sized(class_id, field_count)` 的对象按 `class_id` 的位图追引用；给未注册类
  （`class_id = 0` → `<opaque>`）用时字段里的引用**不会被追踪**（宁可漏活不可炸）。
- LOS 元素的引用数组要求 `elem_size == 4`（引用本身就是 4 字节偏移）。

## 兼容层（冻结）

`librustjrt::` 下仍然导出 0.1 的全部名字，签名不变：

`JavaObject`（含 `size_for`）、`HEADER`、`BumpArena`、`MarkSweep`、`FieldKind`、
`ClassMeta`、`Heap`（`new(cap)` / `alloc(&ClassMeta)` / `set_ref` / `get_ref` /
`collect(roots, metas)` / `live_count` / `arena_used`）、`CompactObject`、
`COMPACT_HEADER`、`CompactMeta`、`CompactHeap`（含 `begin_cycle` / `step` /
`offset_of` / `used_bytes`）、`IncrementalState`、`GcPhase`，以及
`gen::GenerationalHeap` / `gen::MinorStats`。

它们的语义按 0.1 冻结（8 字节引用槽、单一 `ClassMeta` 布局），只共享最底层的
`alloc::Region`；新代码请用 `ManagedHeap`。

## 基准

`examples/bench_full.rs`（8M 次分配、1/8 存活、一轮 minor 全部晋升）：

```text
ALLOC_NS=11.2   MINOR_MS=34.8   PROMOTED=1000000   OLD_LIVE=1000000   MAX_STEP_MS=0.84
```

同机同刻的老实现（`examples/bench_gen.rs` + `bench_compact.rs`）：`MINOR_MS≈36.3`、
`MAX_STEP_MS≈1.43`、分配 `13.5 ns/对象`。

`examples/bench_prod.rs`（新增能力的分项数字，1M 老对象）：

```text
A 压缩    FRAG 0.500 → 0.000   SPAN 15.3MB → 7.6MB   COMPACT_MS=54.1   MOVED=499999
B 增量    INC_STEPS=733   INC_TOTAL_MS=30.1   INC_MAX_STEP_MS=0.683   （对照 STW_MAJOR_MS=26.9）
C 并发    CONC_1T_ALLOC_NS=32.4   CONC_2T=87.6   CONC_4T=88.2
D 弱引用  WEAK 200000 个 → GC 后存活 100000，WEAK_GC_MS=27.9；FINALIZED_QUEUED=5000 只入队一次
```
