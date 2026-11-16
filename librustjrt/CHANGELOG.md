# CHANGELOG — librustjrt

## 0.2.0 — 2026-11-17：生产级内存层

从「完整但保守」到「生产级」：压缩、增量化、多线程、弱引用/finalization 四项能力，
加上一条能持续跑的 200 轮随机图压力测试（它逮到了一个真 bug，见下）。

### 新增能力

- **压缩式老年代**（`old::compact` + `heap::Heap::compact`）
  滑动压缩（mark-compact），按地址升序把存活对象排紧，搬完把尾部整个还回去。
  触发策略是碎片率 `1 - 存活/已提交` 超过 `HeapConfig::compact_ratio`（默认 0.4），
  由 `Heap::full()` 自动判断，也可以手动 `compact()` / `compact_if_fragmented()`。
  实测 100 万老对象、一对一空洞：碎片率 0.500 → 0.000，跨度 15.3MB → 7.6MB，54ms。

- **稳定句柄表**（`handles::HandleTable` / `StableHandle`）
  外部持有的引用不再依赖「偏移即 handle」：句柄指向表里的槽位，压缩后整表按重映射
  一次性改好，句柄本身不动。句柄带代号（generation），槽位复用后旧句柄解析为空，
  不会悄悄指向不相干的新对象。偏移直用的旧 API 原样保留，充当零开销兼容层。

- **增量标记**（`incremental::Marker` + `Heap::step(budget)`）
  状态机 `incremental::GcPhase::{Idle, Marking, Sweeping}`，标记 / 清扫 / 卡表重建
  三段都按对象数预算切片。1M 老对象：489→733 步，总耗时 33.5ms（对照全停顿 27.0ms），
  **最大单步 0.730ms**（0.73ms vs 27ms，停顿降了 37 倍）。
  交错期的两条屏障都上：增量更新（新值着色）+ SATB（删除屏障），两条各配一条反向实验。

- **多线程共享堆**（`concurrent::SharedHeap` + `ThreadLocalTlab`）
  每线程 TLAB 块 + `AtomicU32` 取块（每 64KB 一次 CAS），块内仍是纯 bump。
  GC 与元操作走写锁。计数按线程缓冲、取块时汇总，避免两条热点 cache line。
  实测分配成本：1T 32.5ns / 2T 64.9ns / 4T 83.1ns。

- **弱引用与 finalization**（`weak::WeakTable` / `WeakRef` / `FinalizerQueue`）
  弱引用在 minor / 压缩里跟着搬家，目标死了解析为空；反向索引只在有弱引用时才建。
  `FLAG_FINALIZE` 对象不可达时先进队列、复活一轮再回收，`FLAG_FINALIZED` 保证只入队一次。

### 修掉的 bug

- **压缩漏改写年轻对象里的老年代引用**（`Heap::compact`）
  压缩只改写「老对象内部的引用槽」，一个 eden 里的对象指向老年代对象时，压缩之后它
  手里就是旧地址，而那块地方可能已经住进别的对象。修法：压缩前先做一轮
  `tenuring_threshold = 1` 的 minor，把可达对象全部晋升（真实 JVM 的 full GC 也这么做）。
  这个 bug 是 `stress_200_rounds_mixed_gc_modes` 在第 14 轮逮到的。

- **清扫后句柄不失效**（`HandleTable::prune`）
  不压缩的 `major()` 只把死对象挂进空闲表，句柄里的偏移还留着 —— 解析出来是个
  看着合法的数字，其实指向别人的数据。现在清扫后会按对象起点图把失效句柄清空。

### 测试

`cargo test --release`：51 个单元测试 + 2 个文档测试全绿（0.1 是 27 + 1）。
其中 23 个是这次新增的（`tests_prod.rs`），5 条自带反向实验：把屏障 / 着色拆掉，
断言必须翻转 —— 保证「测试跑通了」不是「测试没在测它」。

### 兼容性

- `compat.rs` / `gen.rs` 里 0.1 的全部类型与签名**一个字都没动**。
- `HeapConfig` 新增字段 `compact_ratio` / `auto_compact` / `incremental_budget`：
  用 `..Default::default()` 构造的调用方不受影响，写全字段字面量的会编译失败。
- `GcPhase` 在 crate 根上仍是**兼容层的旧枚举**；增量标记的新状态机是
  `librustjrt::incremental::GcPhase`（根上也导出了别名 `MarkPhase`）。
