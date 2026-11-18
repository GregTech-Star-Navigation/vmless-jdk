//! 多线程 VM 的共享状态：堆（锁外分配 + 原子字段）、监视器表、类表快照、冷数据。
//!
//! ## 为什么不是「一把大锁」
//!
//! 旧结构是整个 `Vm` 是 `&mut self`：线程之间靠 GIL 串行，`AtomicPtr<Vm>` 是唯一的
//! unsafe 点。代价是 **Java 线程根本不并行**（实测 4 线程比串行还慢 2.2 倍，因为
//! 每 512 条字节码要交还一次 GIL，票据锁的 condvar 乒乓比干活还贵）。
//!
//! 现在拆成三层：
//!
//! ```text
//! 不可变共享   Arc<ClassStore>          —— 加新类时 COW 换快照（`Vm::store`）
//! 共享可变     Shared{ Heap, MonitorTable, ... }  —— 细粒度同步（本文件）
//! 线程私有     Vm{ depth, tick, … } + 解释器帧栈     —— 无锁
//! ```
//!
//! 关键点：**没有一条共享路径需要拿全局锁**。
//! - 字段访问 = `ObjHdr.cells[i]` 上的 `AtomicU64` + `AtomicU8`（tag），无锁、无锁条；
//! - 对象分配 = 每个线程一个 TLAB（原子 bump 一次拿一块，块内纯 bump）；
//! - 监视器 = 对象头里的 thin lock（CAS），只在**真竞争**时才膨胀成等待队列；
//! - 类表 = `Arc<ClassStore>` 快照，读不拿锁。
//!
//! 冷数据（静态字段、类对象表、indy 状态、注解代理…）用 `Mutex` 分组，理由写在
//! [`Shared`] 的注释里：它们不在热路径上，逐个做无锁结构的收益抵不过出错风险。
//!
//! ## 引用计数与句柄
//!
//! 句柄编码沿用旧的三段式（`> 0` 对象、`< 0` 数组、`>= STR_BASE` 字符串、`0` = null），
//! 因为它在 exec/natives/reflect 里出现了两百多处。堆内部改成「页表 + 每对象一块」：
//! 句柄仍是**下标**，但下标查到的是 `ObjHdr` 指针，`ObjHdr` 里的槽位是原子单元，
//! 因此从任何线程读任何对象都不需要锁，也不会因为 `Vec` 扩容而失效 ——
//! 这正是旧 `objects: Vec<Vec<Slot>>` 做不到的（扩容会让别的线程读到半搬的引用）。

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::load::ClassPath;
use crate::ClassFile;

use super::indy;
use super::load::ClassStore;
use super::native_reg::NativeRegistry;
use super::reflect::AnnInst;
use super::slot::Slot;
use super::Vm;
use super::thread::ThreadRec;
use super::monitors::MonitorTable;

/// 字符串句柄基址（与 `heap::STR_BASE` 同值；见 `heap.rs`）
pub(crate) const STR_BASE: i32 = 1 << 24;

// ---------------------------------------------------------------- 槽位单元

/// 堆里的一个槽位：值与标签分开存，两者都是原子的。
///
/// 为什么不把 `Slot`（16 字节）直接放进去：那需要一个 16 字节的原子交换，
/// 而 `AtomicU128` 不是稳定 Rust。分开存是安全的，因为一个槽的**标签在分配后就固定**
/// （字段类型由类布局决定、数组元素类型由描述符决定），只有值会变；
/// 读侧先 `Acquire` 读标签、再 `Acquire` 读值，写侧反过来 —— 只要标签不变，
/// 任何交错都读到「某个完整的写入」。
#[repr(C)]
pub(crate) struct Cell {
    tag: AtomicU8,
    val: AtomicU64,
}

const T_I: u8 = 0;
const T_L: u8 = 1;
const T_F: u8 = 2; // double
const T_FL: u8 = 3; // float

impl Cell {
    fn new(v: Slot) -> Cell {
        let (tag, val) = encode(v);
        Cell { tag: AtomicU8::new(tag), val: AtomicU64::new(val) }
    }

    #[inline]
    pub(crate) fn get(&self) -> Slot {
        let tag = self.tag.load(Ordering::Acquire);
        let v = self.val.load(Ordering::Acquire);
        decode(tag, v)
    }

    #[inline]
    pub(crate) fn set(&self, v: Slot) {
        let (tag, val) = encode(v);
        // 标签先写、值后写：读者先读标签再读值时，看到新标签就一定能看到新值
        self.tag.store(tag, Ordering::Release);
        self.val.store(val, Ordering::Release);
    }
}

fn encode(v: Slot) -> (u8, u64) {
    match v {
        Slot::I(x) => (T_I, x as u32 as u64),
        Slot::L(x) => (T_L, x as u64),
        Slot::F(x) => (T_F, x.to_bits()),
        Slot::Fl(x) => (T_FL, x.to_bits() as u64),
    }
}

fn decode(tag: u8, v: u64) -> Slot {
    match tag {
        T_L => Slot::L(v as i64),
        T_F => Slot::F(f64::from_bits(v)),
        T_FL => Slot::Fl(f32::from_bits(v as u32)),
        // T_I 与未知标签都按 int 处理（未知标签只可能来自内存被写坏）
        _ => Slot::I(v as u32 as i32),
    }
}

// ---------------------------------------------------------------- 对象块

/// 一个对象/数组里**哪些槽是引用**。
///
/// GC 必须精确知道这件事：多标一个（把 int 当句柄）只会多留一点垃圾，少标一个就是
/// 把活对象回收掉 —— 后者是段错误。所以「不知道」时一律退化成 `All`（逐个用页表验证）。
pub(crate) enum RefBits {
    /// 没有引用槽（字段全是基本类型 / 基本类型数组）
    None,
    /// 每个槽都可能是引用（引用数组；或类布局未知时的保守答案）
    All,
    /// 位图（bit i = 第 i 个槽是引用）；≤64 槽用内联，超了才装箱
    Small(u64),
    Big(Box<[u64]>),
}

/// `RefBits::None` 的进程级共享实例（基本类型数组、无引用字段的对象）
fn refbits_none() -> Arc<RefBits> {
    static NONE: std::sync::OnceLock<Arc<RefBits>> = std::sync::OnceLock::new();
    Arc::clone(NONE.get_or_init(|| Arc::new(RefBits::None)))
}

/// `RefBits::All` 的进程级共享实例（引用数组、布局未知时的保守答案）
fn refbits_all() -> Arc<RefBits> {
    static ALL: std::sync::OnceLock<Arc<RefBits>> = std::sync::OnceLock::new();
    Arc::clone(ALL.get_or_init(|| Arc::new(RefBits::All)))
}

impl RefBits {
    #[inline]
    pub(crate) fn is_ref(&self, i: usize) -> bool {        match self {
            RefBits::None => false,
            RefBits::All => true,
            RefBits::Small(w) => (i as u32) < 64 && (w & (1u64 << i)) != 0,
            RefBits::Big(w) => w.get(i >> 6).map(|x| (x >> (i & 63)) & 1 != 0).unwrap_or(false),
        }
    }

    /// `(槽数, 是不是引用)` 列表 → 位图
    pub(crate) fn from_iter(n: usize, is_ref: impl Fn(usize) -> bool) -> RefBits {
        let mut any = false;
        let mut bits = vec![0u64; (n + 63) / 64];
        for i in 0..n {
            if is_ref(i) {
                bits[i >> 6] |= 1 << (i & 63);
                any = true;
            }
        }
        if !any {
            return RefBits::None;
        }
        if n <= 64 {
            RefBits::Small(bits.first().copied().unwrap_or(0))
        } else {
            RefBits::Big(bits.into_boxed_slice())
        }
    }
}

/// 一个对象/数组在堆里的家：类名、长度、thin lock 字、槽位数组。
///
/// 地址稳定（`Box`），多线程并发访问只碰 `cells` / `lock` / `epoch` 三个原子域。
pub(crate) struct ObjHdr {
    /// 运行时类：对象是内部类名，数组是描述符（`[I` / `[Ljava/lang/String;`）
    pub(crate) class: Arc<str>,
    /// 数组长度（对象恒 0）
    pub(crate) len: u32,
    /// thin lock 字：`0` = 未锁；`tid << 8 | 重入计数` = 持有者
    pub(crate) lock: AtomicU32,
    /// GC 标记世代（`== Heap.gc_epoch` 就是本轮存活）。用世代号而不是清除位，
    /// 是因为清扫要遍历所有条目，没法先来一遍 O(n) 的清零。
    pub(crate) epoch: AtomicU32,
    /// 引用槽位图（分配时按类布局算好，扫描时零查表）
    pub(crate) refs: Arc<RefBits>,
    cells: Box<[Cell]>,
}

impl ObjHdr {
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.cells.len()
    }

    #[inline]
    pub(crate) fn get(&self, i: usize) -> Option<Slot> {
        self.cells.get(i).map(|c| c.get())
    }

    #[inline]
    pub(crate) fn set(&self, i: usize, v: Slot) {
        if let Some(c) = self.cells.get(i) {
            c.set(v);
        }
    }

    /// 有没有引用槽（没有的话标记时连值都不用碰 —— 少一次访存就是少一次 miss）
    #[inline]
    pub(crate) fn has_refs(&self) -> bool {
        !matches!(&*self.refs, RefBits::None)
    }

    /// 槽位数组的首地址（GC 预取用：访问它才算把对象的"身体"拉进缓存）
    #[inline]
    pub(crate) fn cells_ptr(&self) -> *const Cell {
        self.cells.as_ptr()
    }

    /// 快照（冷路径用：反射/indy 要整块读）
    pub(crate) fn snapshot(&self) -> Vec<Slot> {
        self.cells.iter().map(|c| c.get()).collect()
    }
}

// ---------------------------------------------------------------- 页表

/// 页表里一页的条目数（2 的幂）
const PAGE_BITS: usize = 9;
const PAGE: usize = 1 << PAGE_BITS; // 512
/// 页表根大小：`1 << 16` 页 × 512 = 3355 万对象。根占 512KB，按需提交。
const ROOT_PAGES: usize = 1 << 16;

struct TablePage<T> {
    entries: [AtomicPtr<T>; PAGE],
}

/// 只追加的页表：`下标 → *const T`。
///
/// 为什么不直接用 `Vec<Box<T>>`：`Vec` 扩容时会把元素搬走，别的线程手里的
/// `&T` 就悬空了。页表永不搬动已发布的条目，所以 `&T` 的寿命等于进程寿命 ——
/// 这就是「任何线程都能安全地拿到任意对象的引用」的全部秘密。
pub(crate) struct PtrTable<T> {
    pages: Box<[AtomicPtr<TablePage<T>>]>,
    count: AtomicU32,
}

impl<T> PtrTable<T> {
    fn new() -> PtrTable<T> {
        let pages = (0..ROOT_PAGES).map(|_| AtomicPtr::new(std::ptr::null_mut())).collect::<Vec<_>>();
        PtrTable { pages: pages.into_boxed_slice(), count: AtomicU32::new(0) }
    }

    #[inline]
    fn page(&self, p: usize) -> Option<&TablePage<T>> {
        if p >= self.pages.len() {
            return None;
        }
        let ptr = self.pages[p].load(Ordering::Acquire);
        if ptr.is_null() {
            None
        } else {
            Some(unsafe { &*ptr })
        }
    }

    #[inline]
    pub(crate) fn get(&self, i: u32) -> Option<&T> {
        let p = (i as usize) >> PAGE_BITS;
        let page = self.page(p)?;
        let e = page.entries[i as usize & (PAGE - 1)].load(Ordering::Acquire);
        if e.is_null() {
            None
        } else {
            Some(unsafe { &*e })
        }
    }

    #[inline]
    pub(crate) fn count(&self) -> u32 {
        self.count.load(Ordering::Acquire)
    }

    /// 追加一个对象，返回它的下标。多线程安全（页用 CAS 建、条目用 Release 发布）。
    fn push(&self, v: T) -> Option<u32> {        let idx = self.count.fetch_add(1, Ordering::AcqRel);
        let p = (idx as usize) >> PAGE_BITS;
        if p >= self.pages.len() {
            // 页表用尽：对象数上限，明确失败（调用方报 OutOfMemoryError），不静默丢
            self.count.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        let mut page = self.pages[p].load(Ordering::Acquire);
        if page.is_null() {
            let fresh = Box::into_raw(Box::new(TablePage::<T> {
                entries: std::array::from_fn(|_| AtomicPtr::new(std::ptr::null_mut())),
            }));
            match self.pages[p].compare_exchange(
                std::ptr::null_mut(),
                fresh,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => page = fresh,
                // 别人先建好了：丢掉自己这块（里面还没有任何条目）
                Err(winner) => {
                    drop(unsafe { Box::from_raw(fresh) });
                    page = winner;
                }
            }
        }
        let obj = Box::into_raw(Box::new(v));
        unsafe { (*page).entries[idx as usize & (PAGE - 1)].store(obj, Ordering::Release) };
        Some(idx)
    }

    /// 清扫：对 `0..count` 的每个非空条目调 `keep(下标, &对象)`，返回 `false` 就
    /// **释放**它（`Box::from_raw` + 条目置空）。返回释放掉的对象数。
    ///
    /// 索引**不复用**：句柄就是索引，复用会让「某个还活在别处的陈旧句柄」指向另一台
    /// 对象 —— 那是静默数据损坏，比多占一点页表内存糟得多。
    fn sweep(&self, mut keep: impl FnMut(u32, &T) -> bool) -> u32 {
        let n = self.count.load(Ordering::Acquire);
        let mut freed = 0u32;
        for i in 0..n {
            let p = (i as usize) >> PAGE_BITS;
            let Some(page) = self.page(p) else { continue };
            let slot = &page.entries[i as usize & (PAGE - 1)];
            let e = slot.load(Ordering::Acquire);
            if e.is_null() {
                continue;
            }
            if keep(i, unsafe { &*e }) {
                continue;
            }
            if slot
                .compare_exchange(e, std::ptr::null_mut(), Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                drop(unsafe { Box::from_raw(e) });
                freed += 1;
            }
        }
        freed
    }
}

// 页表里**已发布**的条目不搬动（对象地址因此稳定），所以跨线程共享它是安全的；
// `T: Send + Sync` 保证条目本身可以跨线程读。
unsafe impl<T: Send + Sync> Send for PtrTable<T> {}
unsafe impl<T: Send + Sync> Sync for PtrTable<T> {}

// ---------------------------------------------------------------- TLAB

/// 每线程一个的 TLAB：先原子地划走一大块槽位，块内纯 bump。
///
/// 这不是为了「分配快一点」—— `Cell` 已经是原子单元，直接逐对象分配也是安全的。
/// 它省的是**页表条目的原子写**：块内连续对象的页是同一页，`push_range` 只在跨页时
/// 才做 CAS。实测对象密集的并行分配（验收第 8 条）靠它把分配路径压到接近单线程。
const TLAB_CELLS: usize = 256;

thread_local! {
    static TLAB: std::cell::Cell<Tlab> = const { std::cell::Cell::new(Tlab::EMPTY) };
}

#[derive(Clone, Copy)]
struct Tlab {
    owner: usize,
    start: u32,
    next: u32,
    end: u32,
}

impl Tlab {
    const EMPTY: Tlab = Tlab { owner: 0, start: 0, next: 0, end: 0 };
}

// ---------------------------------------------------------------- 堆

/// 对象 / 数组 / 字符串三套堆。
pub(crate) struct Heap {
    objs: PtrTable<ObjHdr>,
    arrs: PtrTable<ObjHdr>,
    /// 字符串池：句柄 = `STR_BASE + 下标`（字符串**不回收**：它们是不可变常量/驻留串）
    pub(crate) strings: RwLock<Vec<Arc<str>>>,
    /// 字符串常量 → 句柄（同一常量池项必须得到同一句柄，否则引用相等会错）
    pub(crate) str_handles: Mutex<HashMap<(usize, u16), i32>>,
    /// 字符串句柄的**字段视图**：`getfield value/coder/...` 落在字符串堆句柄上时用
    pub(crate) str_fields: Mutex<HashMap<i32, Vec<Slot>>>,
    /// `StringBuilder` 的缓冲：对象句柄 → 已拼好的文本
    pub(crate) sb_bufs: Mutex<HashMap<i32, String>>,
    /// 包装类的小整数缓存：`Integer#5` → 对象句柄
    pub(crate) wrapper_cache: Mutex<HashMap<String, i32>>,

    // ---- GC ----
    /// 当前标记世代：`ObjHdr.epoch == 这个数` 就是「本轮活着」
    pub(crate) gc_epoch: AtomicU32,
    /// 上一次回收之后的分配次数（触发阈值用它）
    pub(crate) allocs_since_gc: AtomicU64,
    /// 上一次回收后有多少个对象/数组活着（阈值与统计用）
    pub(crate) live_after_gc: AtomicU64,
    /// 上一轮标记**实际扫过**多少个对象。决定下一轮要不要起并行标记线程。
    ///
    /// 为什么不用 `live()`：它把「本轮已分配但还没扫掉的垃圾」也算进去 —— 短命对象
    /// 负载下它在 26 万上下，而真正被标记的可能只有一千个（实测 G1：live=262159、
    /// 标记 1163）。按 `live()` 判就会为了标 1163 个对象去起 7 个线程，光创建线程
    /// 就占掉停顿的六成。改用上一轮的真实标记数，短命负载下停顿从 0.76ms 降到 0.35ms，
    /// 大活集（G2，150 万）仍然走并行。
    pub(crate) last_marked: AtomicU64,
    /// 累计已释放的对象数（`live()` 用它把页表高水位折成活对象数）
    pub(crate) reclaimed: AtomicU64,
    /// 类表快照（算字段引用位图用；`store_publish` 与构造时挂上）
    store: RwLock<Option<Arc<ClassStore>>>,
    /// 类名 → 字段引用位图（每个类只算一次）
    layouts: RwLock<HashMap<String, Arc<RefBits>>>,
}

impl Heap {
    pub(crate) fn new() -> Heap {
        Heap {
            objs: PtrTable::new(),
            arrs: PtrTable::new(),
            strings: RwLock::new(Vec::new()),
            str_handles: Mutex::new(HashMap::new()),
            str_fields: Mutex::new(HashMap::new()),
            sb_bufs: Mutex::new(HashMap::new()),
            wrapper_cache: Mutex::new(HashMap::new()),
            gc_epoch: AtomicU32::new(1),
            allocs_since_gc: AtomicU64::new(0),
            live_after_gc: AtomicU64::new(0),
            last_marked: AtomicU64::new(0),
            reclaimed: AtomicU64::new(0),
            store: RwLock::new(None),
            layouts: RwLock::new(HashMap::new()),
        }
    }

    /// 挂上一份类表快照（`Shared::new` / `store_publish` 调；GC 算位图要用）
    pub(crate) fn attach_store(&self, s: Arc<ClassStore>) {
        *self.store.write().unwrap_or_else(|e| e.into_inner()) = Some(s);
    }


    // ---------- 分配 ----------

    /// 对象的字段引用位图（按类布局算，按类名缓存）。
    ///
    /// 算不出来（类不在类表里 / 槽数和布局对不上）就退化成 `All`：GC 会把每个槽都
    /// 拿页表验证一遍 —— 多留一点垃圾，但绝不会漏掉一个真引用。
    fn obj_refbits(&self, class: &str, n: usize) -> Arc<RefBits> {
        if let Some(b) = self.layouts.read().unwrap_or_else(|e| e.into_inner()).get(class) {
            return Arc::clone(b);
        }
        let g = self.store.read().unwrap_or_else(|e| e.into_inner()).clone();
        let bits = match g {
            Some(store) => class_layout(&store, class, n),
            None => RefBits::All,
        };
        let a = Arc::new(bits);
        self.layouts
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(class.to_string(), Arc::clone(&a));
        a
    }

    /// 分配一个对象，返回句柄（> 0）。类名决定字段数由调用方给。
    pub(crate) fn new_object(&self, slots: Vec<Slot>, class: &str) -> Option<i32> {
        let n = slots.len().max(1);
        let refs = self.obj_refbits(class, n);
        let mut cells = Vec::with_capacity(n);
        for i in 0..n {
            cells.push(Cell::new(slots.get(i).copied().unwrap_or(Slot::I(0))));
        }
        let hdr = ObjHdr {
            class: intern_class(class),
            len: 0,
            lock: AtomicU32::new(0),
            epoch: AtomicU32::new(self.gc_epoch.load(Ordering::Acquire)),
            refs,
            cells: cells.into_boxed_slice(),
        };
        self.allocs_since_gc.fetch_add(1, Ordering::Relaxed);
        // 句柄 = 下标 + 1，而 `handle >= STR_BASE` 会被 `ref_kind` 判成**字符串**。
        // 所以对象下标必须 < STR_BASE - 1，否则第 16777216 个对象起会拿到一个
        // 「看起来是字符串」的句柄 —— 那是静默的语义错乱，比 OOM 糟得多。
        // 页表本身能放到 3355 万个，这道闸是句柄编码先撞上的，所以按句柄上限拦。
        let i = self.objs.push(hdr)?;
        if i as i32 + 1 >= STR_BASE {
            return None;
        }
        Some(i as i32 + 1)
    }

    /// 分配一个数组，返回句柄（< 0）
    pub(crate) fn new_array(&self, elems: Vec<Slot>, desc: &str) -> Option<i32> {
        let n = elems.len();
        // 数组元素类型由描述符决定：`[L…;` / `[[…` 是引用数组，其余是基本类型。
        // 两种情况都用**进程级共享**的那两个 Arc：此前每次 `new int[3]` 都要为
        // `RefBits::None` 单独 Arc 分配一次（~40 字节 + 一次 malloc + 之后一次原子减），
        // 而它恰恰是两个常量之一 —— 短命数组多的负载里这是纯浪费。
        let refs: Arc<RefBits> = if n > 0 && is_ref_array_desc(desc) {
            refbits_all()
        } else {
            refbits_none()
        };
        let cells: Vec<Cell> = elems.into_iter().map(Cell::new).collect();
        let hdr = ObjHdr {
            class: intern_class(desc),
            len: n as u32,
            lock: AtomicU32::new(0),
            epoch: AtomicU32::new(self.gc_epoch.load(Ordering::Acquire)),
            refs,
            cells: cells.into_boxed_slice(),
        };
        self.allocs_since_gc.fetch_add(1, Ordering::Relaxed);
        self.arrs.push(hdr).map(|i| -(i as i32) - 1)
    }

    /// 分配一个元素全为默认值的数组（长度 `n`）
    pub(crate) fn alloc_array(&self, n: usize, desc: &str, zero: Slot) -> Option<i32> {
        self.new_array(vec![zero; n], desc)
    }

    // ---------- 对象 ----------

    #[inline]
    fn obj(&self, h: i32) -> Option<&ObjHdr> {
        if h <= 0 {
            return None;
        }
        self.objs.get(h as u32 - 1)
    }

    #[inline]
    fn arr(&self, h: i32) -> Option<&ObjHdr> {
        if h >= 0 {
            return None;
        }
        self.arrs.get((-h) as u32 - 1)
    }

    /// 任意句柄（对象 / 数组 / 字符串）→ 槽位个数
    pub(crate) fn len(&self, h: i32) -> usize {
        if h >= STR_BASE {
            return self.str_field_snapshot(h).map(|v| v.len()).unwrap_or(0);
        }
        self.obj(h).or_else(|| self.arr(h)).map(|o| o.len()).unwrap_or(0)
    }

    #[inline]
    pub(crate) fn get(&self, h: i32, i: usize) -> Option<Slot> {
        self.obj(h).or_else(|| self.arr(h))?.get(i)
    }

    #[inline]
    pub(crate) fn set(&self, h: i32, i: usize, v: Slot) {
        if let Some(o) = self.obj(h).or_else(|| self.arr(h)) {
            o.set(i, v);
        }
    }

    /// 对象字段的写屏障：引用字段写进来时记卡（GC 用；当前 GC 未接，见 `gc.rs`）
    #[inline]
    pub(crate) fn set_ref(&self, h: i32, i: usize, v: Slot) {
        if let Some(o) = self.obj(h).or_else(|| self.arr(h)) {
            if i < o.len() {
                o.set(i, v);
                super::gc::dirty_write(h as u32);
            }
        }
    }

    pub(crate) fn snapshot(&self, h: i32) -> Option<Vec<Slot>> {
        self.obj(h).or_else(|| self.arr(h)).map(|o| o.snapshot())
    }

    pub(crate) fn class_of(&self, h: i32) -> Option<Arc<str>> {
        self.obj(h).or_else(|| self.arr(h)).map(|o| o.class.clone())
    }

    /// 数组长度（不是数组返回 None）
    pub(crate) fn array_len(&self, h: i32) -> Option<usize> {
        self.arr(h).map(|o| o.len as usize)
    }

    /// 对象的 thin lock 字（字符串句柄没有对象头 → None，走膨胀路径）
    pub(crate) fn lock_word(&self, h: i32) -> Option<&AtomicU32> {
        self.obj(h).or_else(|| self.arr(h)).map(|o| &o.lock)
    }

    // ---------- 字符串 ----------

    pub(crate) fn new_string(&self, text: impl Into<String>) -> i32 {
        let s: Arc<str> = Arc::from(text.into());
        let mut g = self.strings.write().unwrap_or_else(|e| e.into_inner());
        g.push(s);
        STR_BASE + (g.len() as i32) - 1
    }

    pub(crate) fn string_text(&self, h: i32) -> String {
        if h < STR_BASE {
            return String::new();
        }
        let i = (h - STR_BASE) as usize;
        let g = self.strings.read().unwrap_or_else(|e| e.into_inner());
        g.get(i).map(|s| s.to_string()).unwrap_or_default()
    }

    pub(crate) fn is_string(&self, h: i32) -> bool {
        h >= STR_BASE
    }

    pub(crate) fn string_handle(&self, class: usize, cp: u16, text: impl FnOnce() -> Option<String>) -> i32 {
        if let Some(h) = self.str_handles.lock().unwrap_or_else(|e| e.into_inner()).get(&(class, cp)) {
            return *h;
        }
        let t = text().unwrap_or_default();
        let h = self.new_string(t);
        self.str_handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((class, cp), h);
        h
    }

    pub(crate) fn str_handles_clear(&self) {
        self.str_handles.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// 字符串句柄的字段视图（`String.length()` 之类要读 value/coder）
    pub(crate) fn str_field_snapshot(&self, h: i32) -> Option<Vec<Slot>> {
        self.str_fields.lock().unwrap_or_else(|e| e.into_inner()).get(&h).cloned()
    }

    pub(crate) fn str_field_get(&self, h: i32, i: usize) -> Option<Slot> {
        self.str_field_snapshot(h)?.get(i).copied()
    }

    pub(crate) fn str_field_set(&self, h: i32, i: usize, v: Slot) {
        if let Some(f) = self.str_fields.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&h) {
            if i < f.len() {
                f[i] = v;
            }
        }
    }

    /// 装上字符串句柄的字段视图（value/coder 由调用方算好）
    pub(crate) fn str_fields_install(&self, h: i32, slots: Vec<Slot>) {
        self.str_fields.lock().unwrap_or_else(|e| e.into_inner()).insert(h, slots);
    }

    /// 保证字符串句柄有字段视图（长度 = `n`），返回它的快照
    pub(crate) fn str_fields_ensure(&self, h: i32, n: usize) -> Vec<Slot> {
        let mut g = self.str_fields.lock().unwrap_or_else(|e| e.into_inner());
        let e = g.entry(h).or_insert_with(|| vec![Slot::I(0); n.max(1)]);
        if e.len() < n {
            e.resize(n.max(1), Slot::I(0));
        }
        e.clone()
    }

    pub(crate) fn sb_bufs_append(&self, h: i32, text: &str) {
        let mut g = self.sb_bufs.lock().unwrap_or_else(|e| e.into_inner());
        g.entry(h).or_default().push_str(text);
    }

    pub(crate) fn sb_bufs_take(&self, h: i32) -> String {
        self.sb_bufs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&h)
            .unwrap_or_default()
    }

    pub(crate) fn wrapper_get(&self, key: &str) -> Option<i32> {
        self.wrapper_cache.lock().unwrap_or_else(|e| e.into_inner()).get(key).copied()
    }

    pub(crate) fn wrapper_put(&self, key: String, h: i32) {
        self.wrapper_cache.lock().unwrap_or_else(|e| e.into_inner()).insert(key, h);
    }

    /// 已分配对象数 / 数组数（诊断与 GC 用）
    pub(crate) fn counts(&self) -> (u32, u32) {
        (self.objs.count(), self.arrs.count())
    }

    // ---------- GC ----------

    /// 任意句柄 → 对象头（字符串句柄没有对象头，返回 None）
    #[inline]
    pub(crate) fn hdr(&self, h: i32) -> Option<&ObjHdr> {
        self.obj(h).or_else(|| self.arr(h))
    }

    /// 标记：返回 true = 这一轮**第一次**标到它（调用方要把它的槽也扫一遍）
    #[inline]
    pub(crate) fn mark(&self, h: i32, epoch: u32) -> bool {
        let Some(o) = self.hdr(h) else { return false };
        if o.epoch.load(Ordering::Acquire) == epoch {
            return false;
        }
        o.epoch.store(epoch, Ordering::Release);
        true
    }

    /// 已经拿到对象头指针时的标记（并行标记的批处理路径用）
    #[inline]
    pub(crate) fn mark_ptr(&self, o: *const ObjHdr, epoch: u32) -> bool {
        let o = unsafe { &*o };
        if o.epoch.load(Ordering::Acquire) == epoch {
            return false;
        }
        o.epoch.store(epoch, Ordering::Release);
        true
    }

    /// 清扫一轮：释放所有 `epoch != 本轮` 的对象/数组。返回 `(对象数, 数组数, 字节数)`
    pub(crate) fn sweep(&self, epoch: u32) -> (u32, u32, u64) {
        let mut bytes = 0u64;
        let o = self.objs.sweep(|_, obj| {
            if obj.epoch.load(Ordering::Acquire) == epoch {
                return true;
            }
            bytes += obj.bytes();
            false
        });
        let a = self.arrs.sweep(|_, obj| {
            if obj.epoch.load(Ordering::Acquire) == epoch {
                return true;
            }
            bytes += obj.bytes();
            false
        });
        self.reclaimed.fetch_add(o as u64 + a as u64, Ordering::AcqRel);
        (o, a, bytes)
    }

    /// 现在堆里活着的对象数（对象 + 数组）
    pub(crate) fn live(&self) -> u64 {
        let (o, a) = self.counts();
        let dead = self.reclaimed.load(Ordering::Acquire);
        (o as u64 + a as u64).saturating_sub(dead)
    }

    /// 再分配多少次就该回收一轮（「存活 × 2」与下限取大）
    pub(crate) fn gc_threshold(&self) -> u64 {
        (self.live_after_gc.load(Ordering::Acquire) * 2).max(MIN_GC_THRESHOLD)
    }
}

/// 触发阈值下限：不管堆里有多少活对象，至少分配这么多次才考虑回收
pub(crate) const MIN_GC_THRESHOLD: u64 = 262_144;

impl ObjHdr {
    /// 这个对象在堆里大约占多少字节（统计用：48 字节头 + 每槽 16 字节）
    pub(crate) fn bytes(&self) -> u64 {
        48 + 16 * self.cells.len() as u64
    }
}

/// 描述符是不是引用类型
#[inline]
pub(crate) fn is_ref_desc(d: &str) -> bool {
    d.starts_with('L') || d.starts_with('[')
}

/// 数组描述符的元素是不是引用（`[Ljava/…;` / `[[…`）
#[inline]
pub(crate) fn is_ref_array_desc(desc: &str) -> bool {
    let b = desc.as_bytes();
    b.len() >= 2 && b[0] == b'[' && (b[1] == b'L' || b[1] == b'[')
}

/// 类布局 → 字段引用位图。
///
/// 槽位顺序必须和 `load.rs::build_metas` 完全一致（它决定 `field_index`，也就是
/// 对象里第 i 个槽是谁）：继承链**从根往本类**走，每个类按声明顺序、跳过静态字段。
/// 任何一步对不上就返回 `All`（保守）—— 位图算错是 GC 的头号事故来源。
fn class_layout(store: &ClassStore, class: &str, n: usize) -> RefBits {
    let count = store.field_count(class);
    if count == 0 {
        return RefBits::All;
    }
    let chain = store.chain(class); // [本类, 父类, …]
    let mut refs: Vec<bool> = Vec::with_capacity(count);
    for name in chain.iter().rev() {
        let Some(&ci) = store.by_name.get(name) else { continue };
        let cf = &store.classes[ci];
        for f in &cf.fields {
            if f.access_flags & 0x0008 != 0 {
                continue; // 静态字段不在对象里
            }
            let d = String::from_utf8_lossy(cf.utf8(f.descriptor_index).unwrap_or_default()).into_owned();
            refs.push(is_ref_desc(&d));
        }
    }
    if refs.len() != count {
        return RefBits::All;
    }
    // 下标 ≥ count 的是调用方补的填充槽（`new_object` 会 `.max(1)`）：当引用看
    RefBits::from_iter(n, |i| refs.get(i).copied().unwrap_or(true))
}

/// 类名驻留：同一个类名在堆里只存一份 `Arc<str>`
fn intern_class(name: &str) -> Arc<str> {
    // 直接 Arc::from 已经足够便宜（一次分配），驻留表反而要一把锁。
    // 类名是对象头的热读字段，读侧只做引用计数自增，绝不拿锁。
    Arc::from(name)
}

// ---------------------------------------------------------------- indy 状态

/// `invokedynamic` / `java.lang.invoke` 的全部运行时状态。
///
/// 一个锁管一组：这些表只在 `invokedynamic` 首次解析、反射访问方法句柄、
/// `StringConcatFactory` 建调用点时被碰，**都不在字节码热路径上**
/// （热路径只读已解析的调用点句柄，见 `exec.rs` 的 `0xba`）。拆成无锁结构没有收益。
#[derive(Default)]
pub(crate) struct IndyState {
    pub(crate) mhs: HashMap<i32, indy::MhInfo>,
    pub(crate) mtypes: HashMap<i32, String>,
    pub(crate) lookups: HashMap<i32, String>,
    pub(crate) callsites: HashMap<i32, indy::CallSiteInfo>,
    pub(crate) lambdas: HashMap<i32, indy::LambdaInfo>,
    pub(crate) varhandles: HashMap<i32, indy::VarHandleInfo>,
    pub(crate) bsm_tables: HashMap<String, indy::BsmTable>,
    pub(crate) indy_sites: HashMap<(usize, u16), i32>,
    pub(crate) condy_values: HashMap<(usize, u16), indy::TypedSlot>,
    pub(crate) indy_resolved: usize,
    pub(crate) indy_calls: usize,
    pub(crate) lambda_seq: usize,
}

/// indy 状态里所有的候选句柄（GC 根扫描用）。
///
/// 这些表里存的是**方法句柄对象 / 调用点 / 捕获值**的句柄 —— 它们没有任何 Java 侧
/// 的引用指着（就是 VM 自己造的元数据），所以必须显式当根，否则会被回收掉。
impl IndyState {
    pub(crate) fn each_handle(&self, mut f: impl FnMut(i32)) {
        for (k, v) in self.mhs.iter() {
            f(*k);
            match &v.kind {
                indy::MhKind::Bound { target, bindings, .. } => {
                    f(*target);
                    for b in bindings {
                        if let Slot::I(h) = *b {
                            f(h);
                        }
                    }
                }
                indy::MhKind::Constant(v) => {
                    if let Slot::I(h) = *v {
                        f(h);
                    }
                }
                indy::MhKind::Adapter { target } => f(*target),
                indy::MhKind::Concat { consts, .. } => {
                    for (_, v) in consts.iter() {
                        if let Slot::I(h) = *v {
                            f(h);
                        }
                    }
                }
                indy::MhKind::LambdaCtor(t) => f(t.target),
                indy::MhKind::Direct { .. } | indy::MhKind::Identity => {}
            }
        }
        for k in self.mtypes.keys() {
            f(*k);
        }
        for k in self.lookups.keys() {
            f(*k);
        }
        for (k, v) in self.callsites.iter() {
            f(*k);
            f(v.target);
        }
        for (k, v) in self.lambdas.iter() {
            f(*k);
            f(v.target);
            for c in &v.captured {
                if let Slot::I(h) = *c {
                    f(h);
                }
            }
        }
        for k in self.varhandles.keys() {
            f(*k);
        }
        for v in self.indy_sites.values() {
            f(*v);
        }
        for (_, v) in self.condy_values.iter() {
            if let Slot::I(h) = v.1 {
                f(h);
            }
        }
    }
}

// ---------------------------------------------------------------- 共享根

/// 所有跨线程可变状态的根。每个 Java 线程的 `Vm` 持有同一个 `Arc<Shared>`。
pub(crate) struct Shared {
    // ---- 堆（热） ----
    pub(crate) heap: Heap,
    /// 监视器：对象头 thin lock + 竞争时膨胀出的等待集
    pub(crate) monitors: MonitorTable,

    // ---- 类元数据（读多写极少） ----
    /// 权威快照；加类时 COW 换一份，各线程用自己的 `Vm::store` 快照
    pub(crate) store: RwLock<Arc<ClassStore>>,
    /// 类表版本号：线程发现自己的快照旧了就在**下一次解析失败时**刷新
    pub(crate) store_ver: AtomicU64,

    // ---- 冷数据：一把锁一组 ----
    pub(crate) statics: Mutex<HashMap<(String, String), Slot>>,
    pub(crate) output: Mutex<Vec<String>>,
    pub(crate) initialized: Mutex<HashSet<String>>,
    pub(crate) initializing: Mutex<HashMap<String, u64>>,
    pub(crate) init_waiters: Mutex<HashMap<String, Vec<u64>>>,
    pub(crate) class_objs: Mutex<HashMap<i32, String>>,
    pub(crate) class_obj_cache: Mutex<HashMap<String, i32>>,
    pub(crate) exc_msgs: Mutex<HashMap<i32, i32>>,
    /// `Unsafe.staticFieldBase` 的哨兵对象 → `(类, 字段名)`。
    /// 静态字段在我们这里没有「基址 + 字节偏移」的布局（存在 `statics` 表里），
    /// 但 `Unsafe.putInt(base, off, v)` 的 ABI 要求先拿 base 再拿 off，
    /// 所以给每个静态字段发一个哨兵对象，靠它把手上的 `base` 反查回具体的字段。
    pub(crate) static_bases: Mutex<HashMap<i32, (String, String)>>,
    pub(crate) ann_objs: Mutex<HashMap<i32, AnnInst>>,
    pub(crate) indy: Mutex<IndyState>,
    /// 运行期类加载用的 classpath（`Class.forName`）
    pub(crate) cp: Mutex<Option<ClassPath>>,
    /// Java 线程表
    pub(crate) threads: Mutex<HashMap<u64, ThreadRec>>,
    /// 缺 native 的累计清单
    pub(crate) missing_natives: Mutex<BTreeSet<String>>,

    // ---- native 注册表（构造后只读） ----
    pub(crate) natives: NativeRegistry,

    // ---- 计数与配置 ----
    pub(crate) nthreads: AtomicU32,
    pub(crate) thread_seq: AtomicU32,
    pub(crate) stalled: AtomicU32,
    pub(crate) jdk_loaded: bool,
    pub(crate) prefer_builtin: bool,
    pub(crate) indy_stats: bool,
    /// 全局停靠点（GC 停世界用）
    pub(crate) safepoint: super::gc::Safepoint,
    /// `VMLESS_GC_STATS=1`：每轮回收打一行统计
    pub(crate) gc_stats: bool,
    /// `VMLESS_GC_STRESS=1`：每次 GC 检查点都回收（把漏根立刻打成崩溃）
    pub(crate) gc_stress: bool,
    /// `VMLESS_GC_OFF=1`：完全关掉回收（量「改前」基线用；其余行为不变）
    pub(crate) gc_off: bool,
    /// 清扫线程在跑（这段时间不开始新的一轮，否则标记世代与清扫目标世代会打架）
    pub(crate) sweeping: AtomicBool,
    /// 同一时刻只允许一个线程发起回收
    pub(crate) gc_lock: Mutex<()>,
    /// 上一次「停世界」失败的退避截止时间（毫秒，相对进程启动）
    pub(crate) gc_backoff: AtomicU64,
    /// 算过的引用位图数 / 其中退化成保守的个数（覆盖率诊断）
    pub(crate) refmap_total: AtomicU64,
    pub(crate) refmap_cons: AtomicU64,
    /// 每帧引用位图缓存：`(ClassFile 地址, 方法下标) → 位图`
    pub(crate) refmaps: RwLock<HashMap<(usize, usize), (Arc<ClassFile>, Arc<super::gc::MethodRefs>)>>,
}

impl Shared {
    pub(crate) fn new(store: Arc<ClassStore>) -> Shared {
        let jdk_loaded = store.has_class("java/lang/Object");
        let indy_stats = super::prop("vmless.indy.stats")
            .map(|v| v != "0" && v != "false")
            .unwrap_or_else(|| {
                std::env::var("VMLESS_INDY_STATS").map(|v| v == "1").unwrap_or(false)
            });
        let heap = Heap::new();
        heap.attach_store(Arc::clone(&store));
        Shared {
            heap,
            monitors: MonitorTable::new(),
            store: RwLock::new(store),
            store_ver: AtomicU64::new(0),
            statics: Mutex::new(HashMap::new()),
            output: Mutex::new(Vec::new()),
            initialized: Mutex::new(HashSet::new()),
            initializing: Mutex::new(HashMap::new()),
            init_waiters: Mutex::new(HashMap::new()),
            class_objs: Mutex::new(HashMap::new()),
            class_obj_cache: Mutex::new(HashMap::new()),
            exc_msgs: Mutex::new(HashMap::new()),
            static_bases: Mutex::new(HashMap::new()),
            ann_objs: Mutex::new(HashMap::new()),
            indy: Mutex::new(IndyState::default()),
            cp: Mutex::new(None),
            threads: Mutex::new(HashMap::new()),
            missing_natives: Mutex::new(BTreeSet::new()),
            natives: NativeRegistry::new(),
            nthreads: AtomicU32::new(0),
            thread_seq: AtomicU32::new(0),
            stalled: AtomicU32::new(0),
            jdk_loaded,
            prefer_builtin: std::env::var("VMLESS_PREFER_BUILTIN").map(|v| v == "1").unwrap_or(false),
            indy_stats,
            safepoint: super::gc::Safepoint::new(),
            gc_stats: std::env::var("VMLESS_GC_STATS").map(|v| v != "0" && v != "false").unwrap_or(false),
            gc_stress: std::env::var("VMLESS_GC_STRESS").map(|v| v != "0" && v != "false").unwrap_or(false),
            gc_off: std::env::var("VMLESS_GC_OFF").map(|v| v != "0" && v != "false").unwrap_or(false),
            sweeping: AtomicBool::new(false),
            gc_lock: Mutex::new(()),
            gc_backoff: AtomicU64::new(0),
            refmap_total: AtomicU64::new(0),
            refmap_cons: AtomicU64::new(0),
            refmaps: RwLock::new(HashMap::new()),
        }
    }

    /// 取一份最新的类表快照
    #[inline]
    pub(crate) fn store_snapshot(&self) -> Arc<ClassStore> {
        self.store.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 用一份新快照替换权威类表，并推进版本号
    pub(crate) fn store_publish(&self, s: Arc<ClassStore>) -> u64 {
        self.heap.attach_store(Arc::clone(&s));
        *self.store.write().unwrap_or_else(|e| e.into_inner()) = s;
        self.store_ver.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub(crate) fn output_push(&self, line: String) {
        self.output.lock().unwrap_or_else(|e| e.into_inner()).push(line);
    }

    pub(crate) fn output_take(&self) -> Vec<String> {
        std::mem::take(&mut *self.output.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub(crate) fn output_drain(&self) -> Vec<String> {
        self.output_take()
    }
}

/// 取一把共享 `Mutex` 的锁（poison 了也继续 —— VM 里宁可读到半截状态也不要因为
/// 别的线程 panic 而整进程卡死）
#[inline]
pub(crate) fn lk<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Vm {
    /// indy / 反射状态表的锁
    #[inline]
    pub(crate) fn indy(&self) -> std::sync::MutexGuard<'_, IndyState> {
        lk(&self.sh.indy)
    }
}
