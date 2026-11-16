//! 并发分配：多线程共享一个堆，锁外分配，GC 仍然全停顿。
//!
//! # 为什么 `Heap` 本身不能直接 `Sync`
//!
//! `Heap` 的分配路径全是 `Cell` bump（`eden_top`、`semi_top`、`tlab`）——
//! 单线程一把梭换来的是 `ALLOC_NS ≈ 12`。把每个 `Cell` 都换成 `AtomicU32`
//! 会让**单线程的每一条路径**都背上原子操作，而那是最常走的路。
//!
//! 所以这里分成两层：
//!
//! ```text
//!   [线程 1] ─┐                       ┌─ 每线程 TLAB 块（普通 bump，没有原子操作）
//!   [线程 2] ─┼─► SharedHeap ────────►┤
//!   [线程 3] ─┘        │              └─ 块用完 → AtomicU32 上 CAS 拿新块
//!                      └─ GC / 元操作：写锁（RwLock）+ &mut Heap
//! ```
//!
//! - **块分配**走 `AtomicU32::fetch_update`，每 `CHUNK` 字节一次，不是每次分配一次。
//! - **块内分配**是纯 bump，代价与单线程一样。
//! - `RwLock` 的读锁只挡「GC 正在搬家」这一件事：`Region` 是 `realloc` 出来的，
//!   增长会让基址变化，正在往外写的线程就会写进已释放的内存。读锁把这条竞态关掉，
//!   代价是一次 `Acquire` 级别的位置位。**这是唯一为并发付的固定成本**。
//! - 代际（`epoch`）在每次 GC 后 +1，线程本地块一看代际变了就作废：
//!   否则线程会继续往已经被清空 / 挪走的 eden 里写。
//!
//! # 用法
//!
//! ```no_run
//! use librustjrt::{HeapConfig, SlotKind, SharedHeap};
//! use std::sync::Arc;
//!
//! let shared = Arc::new(SharedHeap::new(HeapConfig::default()));
//! let node = shared.with_heap_mut(|h| h.register_object("Node", &[SlotKind::Prim]));
//! let mut handles = Vec::new();
//! for _ in 0..4 {
//!     let sh = shared.clone();
//!     handles.push(std::thread::spawn(move || {
//!         (0..1000).map(|_| sh.alloc(node)).collect::<Vec<_>>()
//!     }));
//! }
//! let all: Vec<u32> = handles.into_iter().flat_map(|t| t.join().unwrap()).collect();
//! shared.with_heap_mut(|h| h.major());
//! # let _ = all;
//! ```

use std::cell::Cell;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::heap::{Heap, HeapConfig};
use crate::object::{self, FLAG_ARRAY, HEADER};
use crate::old::MajorReport;
use crate::roots::Handle;
use crate::stats::Stats;
use crate::young::MinorReport;

/// 每次从原子 bump 上拿的块大小（字节）
pub const CHUNK: u32 = 64 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct ThreadLocalTlab {
    owner: usize,
    epoch: u32,
    start: u32,
    top: u32,
    end: u32,
    /// 本块内分配的对象数 / 总分配次数（每取新块时汇总到原子计数器）
    objects: u32,
    allocs: u32,
}

impl Default for ThreadLocalTlab {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadLocalTlab {
    pub const fn new() -> ThreadLocalTlab {
        ThreadLocalTlab { owner: 0, epoch: 0, start: 0, top: 0, end: 0, objects: 0, allocs: 0 }
    }

    #[inline]
    fn usable(&self, owner: usize, epoch: u32) -> bool {
        self.owner == owner && self.epoch == epoch && self.top < self.end
    }

    #[inline]
    fn alloc(&mut self, size: u32) -> Option<u32> {
        if self.top.checked_add(size)? <= self.end {
            let off = self.top;
            self.top += size;
            Some(off)
        } else {
            None
        }
    }

    #[inline]
    pub fn remaining(&self) -> u32 {
        self.end.saturating_sub(self.top)
    }

    #[inline]
    pub fn used(&self) -> u32 {
        self.top.saturating_sub(self.start)
    }
}

thread_local! {
    /// 本线程在各堆上的当前块。一个线程同时用多个堆是不常见的，
    /// 所以只留一份、靠 `owner` 判归属（换堆就作废重拿，代价可忽略）。
    static TL: Cell<ThreadLocalTlab> = const { Cell::new(ThreadLocalTlab::new()) };
}

struct Inner {
    /// 只有拿到写锁才能 `&mut *heap`
    heap: std::cell::UnsafeCell<Heap>,
    /// 读锁 = 正在分配；写锁 = GC / 元操作 / 搬家
    guard: RwLock<()>,
    /// eden bump 的原子版本（`Heap::eden_top` 的镜像）
    eden_top: AtomicU32,
    eden_start: u32,
    eden_end: u32,
    /// 堆基址镜像（`Region::grow` 会让它变）
    base: AtomicPtr<u8>,
    /// GC 代际：变了就作废线程本地块
    epoch: AtomicU32,
    /// 并发分配的计数（`Heap::eden_objects` 的镜像）
    eden_objects: AtomicUsize,
    allocs: AtomicU64,
    /// 正在 GC 的标志（诊断）
    in_gc: AtomicU32,
    /// 元操作串行化（with_heap_mut / gc）
    meta: Mutex<()>,
}

/// 可被多线程共享的堆。
///
/// 分配是锁外的（每线程 TLAB + 原子取块），GC 是全停顿的（写锁）。
#[derive(Clone)]
pub struct SharedHeap {
    inner: Arc<Inner>,
}

// 安全说明：`Inner::heap` 只在持写锁时被 `&mut` 访问；读锁期间只通过
// `eden_top` + `base` 两个原子做无锁 bump，不碰 `Heap` 本体。
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

impl Default for SharedHeap {
    fn default() -> Self {
        SharedHeap::new(HeapConfig::default())
    }
}

impl SharedHeap {
    pub fn new(cfg: HeapConfig) -> SharedHeap {
        let heap = Heap::new(cfg);
        let eden_top = heap.eden_top_raw();
        let eden_start = heap.eden_start();
        let eden_end = heap.eden_end();
        let base = heap.base();
        SharedHeap {
            inner: Arc::new(Inner {
                heap: std::cell::UnsafeCell::new(heap),
                guard: RwLock::new(()),
                eden_top: AtomicU32::new(eden_top),
                eden_start,
                eden_end,
                base: AtomicPtr::new(base),
                epoch: AtomicU32::new(0),
                eden_objects: AtomicUsize::new(0),
                allocs: AtomicU64::new(0),
                in_gc: AtomicU32::new(0),
                meta: Mutex::new(()),
            }),
        }
    }

    #[inline]
    fn id(&self) -> usize {
        Arc::as_ptr(&self.inner) as usize
    }

    /// 分配一个按类布局的对象（多线程安全）。
    ///
    /// 快路径只拿**一次**读锁：类布局查询和 bump 在同一次持锁里做完。
    /// （早期版本查询一次、bump 一次，等于两次 `RwLock::read`，实测占了一半成本。）
    pub fn alloc(&self, class_id: u32) -> u32 {
        self.alloc_with(class_id, 0, 0)
    }

    /// 分配数组
    pub fn alloc_array(&self, class_id: u32, len: u32) -> u32 {
        self.alloc_with(class_id, len, 1)
    }

    /// 宽分配（`field_count` 决定大小，让对象自描述尺寸）
    pub fn alloc_sized(&self, class_id: u32, field_count: u32) -> u32 {
        self.alloc_with(class_id, 0, field_count)
    }

    /// `kind`: 0 = 按类布局，1 = 数组（`len` 有效），>=2 = 显式字段数（`extra = field_count`）
    fn alloc_with(&self, class_id: u32, len: u32, kind: u32) -> u32 {
        // ---- 快路径：一次读锁，布局 + bump + 初始化 ----
        let fast = {
            let _r = self.inner.guard.read().unwrap();
            let h = unsafe { &*self.inner.heap.get() };
            let (length, size, flags) = match kind {
                1 => {
                    let cls = h.layout().class(class_id);
                    (len, cls.total_size(len), FLAG_ARRAY)
                }
                0 => {
                    let cls = h.layout().class(class_id);
                    let fl = object::with_field_count(
                        if cls.is_array { FLAG_ARRAY } else { 0 },
                        cls.field_count,
                    );
                    (0, cls.total_size(0), fl)
                }
                n => (
                    0,
                    object::Object::size_for_fields(n as usize) as u32,
                    object::with_field_count(0, n),
                ),
            };
            debug_assert!((size as usize) >= HEADER);
            if size > CHUNK / 4 {
                None // 大对象：走 LOS（要写锁，所以先出去）
            } else {
                self.bump_fast(h.base(), class_id, length, size, flags).map(|off| (off, size, flags, length))
            }
        };

        match fast {
            Some((off, _, _, _)) => off,
            None => {
                // 慢路径：先确认是不是「大对象」还是「eden 用尽」
                let big = {
                    let _r = self.inner.guard.read().unwrap();
                    let h = unsafe { &*self.inner.heap.get() };
                    let size = match kind {
                        1 => h.layout().class(class_id).total_size(len),
                        0 => h.layout().class(class_id).total_size(0),
                        n => object::Object::size_for_fields(n as usize) as u32,
                    };
                    size > CHUNK / 4
                };
                if big {
                    return self.los_alloc(class_id, len, kind);
                }
                // eden 用尽：放开读锁，进写锁回收一轮，然后重试
                self.minor();
                self.alloc_with(class_id, len, kind)
            }
        }
    }

    /// 在**已持读锁**的前提下做一次 TLAB bump + 初始化。
    fn bump_fast(
        &self,
        base: *mut u8,
        class_id: u32,
        length: u32,
        size: u32,
        flags: u32,
    ) -> Option<u32> {
        let id = self.id();
        let epoch = self.inner.epoch.load(Ordering::Acquire);
        let off = TL.with(|c| {
            let mut t = c.get();
            if !t.usable(id, epoch) {
                return None;
            }
            let off = t.alloc(size)?;
            c.set(t);
            Some(off)
        });
        let off = match off {
            Some(o) => o,
            None => {
                // 块用完（或作废）：原子拿一块新的，一次 CAS 换 CHUNK 字节
                let want = size.max(CHUNK);
                let start = self.bump_eden(want)?;
                TL.with(|c| {
                    let mut t = c.get();
                    // 换块前把上块攒的计数汇总掉
                    if t.objects != 0 || t.allocs != 0 {
                        self.inner.eden_objects.fetch_add(t.objects as usize, Ordering::Relaxed);
                        self.inner.allocs.fetch_add(t.allocs as u64, Ordering::Relaxed);
                    }
                    t.owner = id;
                    t.epoch = epoch;
                    t.start = start;
                    t.top = start;
                    t.end = start + want;
                    t.objects = 0;
                    t.allocs = 0;
                    let off = t.alloc(size);
                    c.set(t);
                    off
                })?
            }
        };
        unsafe { object::init(base, off, class_id, flags, length, size) };
        // 计数留在**线程本地**：两次共享原子自增在 4 线程下就是两条热点 cache line，
        // 会把分配吞吐拖成一半。取新块时才汇总（每 64KB 一次）。
        TL.with(|c| {
            let mut t = c.get();
            t.objects = t.objects.wrapping_add(1);
            t.allocs = t.allocs.wrapping_add(1);
            c.set(t);
        });
        Some(off)
    }

    /// 原子地划走 `want` 字节。返回起点；eden 不够返回 `None`。
    fn bump_eden(&self, want: u32) -> Option<u32> {
        let mut cur = self.inner.eden_top.load(Ordering::Relaxed);
        loop {
            let end = self.inner.eden_end;
            let take = want.min(end.saturating_sub(cur));
            if take < want {
                return None;
            }
            match self.inner.eden_top.compare_exchange_weak(
                cur,
                cur + want,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(cur),
                Err(actual) => cur = actual,
            }
        }
    }

    fn los_alloc(&self, class_id: u32, len: u32, kind: u32) -> u32 {
        let _g = self.inner.guard.write().unwrap();
        let epoch = self.inner.epoch.load(Ordering::Acquire);
        let h = unsafe { &mut *self.inner.heap.get() };
        h.sync_eden_top(self.inner.eden_top.load(Ordering::Acquire));
        let (length, size, flags) = match kind {
            1 => (len, h.layout().class(class_id).total_size(len), FLAG_ARRAY),
            0 => {
                let cls = h.layout().class(class_id);
                let fl = object::with_field_count(
                    if cls.is_array { FLAG_ARRAY } else { 0 },
                    cls.field_count,
                );
                (0, cls.total_size(0), fl)
            }
            n => (
                0,
                object::Object::size_for_fields(n as usize) as u32,
                object::with_field_count(0, n),
            ),
        };
        let off = h.alloc_los_raw(class_id, length, size, flags | object::FLAG_LOS);
        self.inner.epoch.store(epoch.wrapping_add(1), Ordering::Release);
        self.inner.base.store(h.base(), Ordering::Release);
        self.inner.eden_top.store(h.eden_top_raw(), Ordering::Release);
        self.inner.eden_objects.store(0, Ordering::Release);
        off
    }

    /// 以写锁跑一轮 GC：把所有线程本地块作废，再让 `Heap` 干活。
    ///
    /// 全停顿的边界就在这把写锁上 —— 从拿到锁到放开，其它线程的分配会卡在读锁上。
    fn run_gc<R>(&self, f: impl FnOnce(&mut Heap) -> R) -> R {
        let _m = self.inner.meta.lock().unwrap();
        let _g = self.inner.guard.write().unwrap();
        self.inner.in_gc.fetch_add(1, Ordering::Relaxed);
        let objects = self.inner.eden_objects.swap(0, Ordering::AcqRel);
        let r = {
            let h = unsafe { &mut *self.inner.heap.get() };
            h.sync_eden_top(self.inner.eden_top.load(Ordering::Acquire));
            h.add_eden_objects(objects);
            let r = f(h);
            // GC 之后：eden 换了位置 / 被清空 → 代际 +1，所有线程本地块作废
            self.inner.epoch.fetch_add(1, Ordering::AcqRel);
            self.inner.base.store(h.base(), Ordering::Release);
            self.inner.eden_top.store(h.eden_top_raw(), Ordering::Release);
            r
        };
        self.inner.in_gc.fetch_sub(1, Ordering::Relaxed);
        r
    }

    /// 一轮 minor（复制年轻代）
    pub fn minor(&self) -> MinorReport {
        self.run_gc(|h| h.minor())
    }

    /// 一轮 major（老年代标记-清扫）
    pub fn major(&self) -> MajorReport {
        self.run_gc(|h| h.major())
    }

    /// 一轮 full（minor + major + 按碎片率决定是否压缩）
    pub fn full(&self) -> MajorReport {
        self.run_gc(|h| h.full())
    }

    /// 在写锁里拿到 `&mut Heap`：根集操作、写屏障、遍历都用它。
    ///
    /// 闭包里**不要**做长耗时的事（那等于一次全停顿），也别把 `&mut Heap`
    /// 泄漏到闭包外。闭包结束时线程本地块一律作废（闭包可能直接调了 `alloc`）。
    pub fn with_heap_mut<R>(&self, f: impl FnOnce(&mut Heap) -> R) -> R {
        self.run_gc(f)
    }

    /// 只读访问（持写锁，保证没有人在写；读操作很短）。
    ///
    /// 注意：`Heap::tlab_alloc` 是 `&self` 的，但它的 bump 指针是单线程那份 ——
    /// 在并发堆里调它会和别的线程抢同一个指针。要分配请用 `SharedHeap::alloc`。
    pub fn with_heap<R>(&self, f: impl FnOnce(&Heap) -> R) -> R {
        let _g = self.inner.guard.write().unwrap();
        let h = unsafe { &*self.inner.heap.get() };
        f(h)
    }

    pub fn stats(&self) -> Stats {
        self.with_heap(|h| h.stats())
    }

    /// 已汇总的并发分配次数（诊断）。
    ///
    /// 计数按线程缓冲、取新块时汇总，所以线程退出时最后一块里的分配还没汇总 ——
    /// 这是个下界，不是精确值（精确值要每次分配都打一条热点 cache line，不值得）。
    #[inline]
    pub fn alloc_count(&self) -> u64 {
        self.inner.allocs.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn epoch(&self) -> u32 {
        self.inner.epoch.load(Ordering::Acquire)
    }

    /// eden 的语义区间（并发分配返回的偏移一定落在这里）
    #[inline]
    pub fn eden_span(&self) -> (u32, u32) {
        (self.inner.eden_start, self.inner.eden_end)
    }

    /// 把裸偏移包成 `Handle`
    #[inline]
    pub fn handle(off: u32) -> Handle {
        Handle(off)
    }
}
