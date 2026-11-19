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

    /// 批量把 `n` 个未初始化的槽位写成「全零」（= `Slot::I(0)`）。
    ///
    /// 这是一次**连续 memset**：标签与值都是 0，正好是 `T_I` + `I(0)` 的编码，
    /// 所以不必逐槽调 `encode`。对象创建时清零与写头合并成一次内存往返，
    /// 这是 `new X()` 比改动前少一次分配、少一次全块写的直接来源。
    #[inline]
    unsafe fn fill_zero(p: *mut Cell, n: usize) {
        std::ptr::write_bytes(p as *mut u8, 0, n * CELL_SIZE);
    }

    /// 批量把 `n` 个未初始化的槽位写成同一个值（`new int[n]` 的零/初值、对象默认值）
    /// 往**未初始化**的槽位写一个值（填充函数用；等价于 `Cell::new` 但不需要先有引用）
    #[inline]
    unsafe fn write(p: *mut Cell, v: Slot) {
        let (tag, val) = encode(v);
        std::ptr::addr_of_mut!((*p).tag).write(AtomicU8::new(tag));
        std::ptr::addr_of_mut!((*p).val).write(AtomicU64::new(val));
    }

    #[inline]
    unsafe fn fill(p: *mut Cell, v: Slot, n: usize) {
        let (tag, val) = encode(v);
        // 与 `fill_zero` 同构，但值不是零：逐槽写两个域（没有一次性 16 字节原子写可用）
        for i in 0..n {
            let c = &mut *p.add(i);
            c.tag = AtomicU8::new(tag);
            c.val = AtomicU64::new(val);
        }
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

/// `RefBits::All` / `None` 的进程级共享实例（引用数组、无引用字段的对象、宽对象）。
///
/// 泄漏成 `&'static`：槽位图只跟类布局有关，类的种类有界（java.base 也就几千个），
/// 而对象头里只能放一个 8 字节域 —— 是内联位图还是指针由 `nslots > 64` 决定，不需要额外标志位。
fn refbits_all_static() -> &'static RefBits {
    static A: std::sync::OnceLock<&'static RefBits> = std::sync::OnceLock::new();
    A.get_or_init(|| Box::leak(Box::new(RefBits::All)))
}

fn refbits_none_static() -> &'static RefBits {
    static N: std::sync::OnceLock<&'static RefBits> = std::sync::OnceLock::new();
    N.get_or_init(|| Box::leak(Box::new(RefBits::None)))
}

impl RefBits {
    #[inline]
    pub(crate) fn is_ref(&self, i: usize) -> bool {
        match self {
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

    /// 位图 → 对象头里的那一个 8 字节域。
    ///
    /// **编码约定（唯一）**：`0` = 一个引用槽都没有；`n <= 64` 时是内联位图；
    /// `n > 64` 时是泄漏成静态实例的 `*const RefBits`。所以 `is_ref`/`has_refs`
    /// 两条路径都先判 0 —— 没有引用槽的对象占实测负载的绝大多数，这个判断同时是
    /// 热路径的短路。
    fn to_word(self, n: usize) -> u64 {
        if matches!(self, RefBits::None) {
            return 0;
        }
        if n <= 64 {
            match self {
                RefBits::None => 0,
                RefBits::Small(w) => w,
                // `All` 用全 1（槽位数 ≤ 64，多出来的位没人看）
                RefBits::All | RefBits::Big(_) => u64::MAX,
            }
        } else {
            Box::leak(Box::new(self)) as *const RefBits as usize as u64
        }
    }
}

/// 一个槽位的字节数（`{tag: AtomicU8, val: AtomicU64}`，对齐到 8 → 16 字节）
pub(crate) const CELL_SIZE: usize = 16;
/// 对象头字节数；槽位数组紧跟在头后面，**同一次分配**
pub(crate) const OBJ_HDR_SIZE: usize = 24;

/// 一个对象/数组在堆里的家：24 字节头 + 紧随其后的变长槽位数组（一次分配）。
///
/// 旧布局是 56 字节头（`Arc<str>` 16 + len/lock/epoch 12 + `Arc<RefBits>` 8 + `Box<[Cell]>` 16）
/// **加**一次单独的槽位数组分配（再 16 字节 malloc 块头）。三项改动合起来：
///
/// | 项 | 旧 | 新 |
/// | --- | --- | --- |
/// | 类名 `Arc<str>` | 16 B | `u32` class_id（见 `Heap::class_names`） |
/// | 引用位图 `Arc<RefBits>` | 8 B | ≤64 槽内联 `u64`，>64 槽静态指针 |
/// | 槽位数组 | 8 B 指针 + 16 B 块头 + 独立分配 | 内联，零额外开销 |
/// | 头总大小 | 56 B | **24 B** |
///
/// 除了 RSS，24 字节头 + 内联槽位让「标记」从「页表 → 头 → 槽位数组」三次访存
/// 变成「页表 → 头+槽位」，这正是停下来标 80 万对象那一格的主要成本。
///
/// 地址稳定（一次分配后永不搬动），并发访问只碰 `lock` / `epoch` 两个原子域。
#[repr(C)]
pub(crate) struct ObjHdr {
    /// thin lock 字：`0` = 未锁；`tid << 8 | 重入计数` = 持有者
    pub(crate) lock: AtomicU32,
    /// GC 标记世代（`== Heap.gc_epoch` 就是本轮存活）。用世代号而不是清除位，
    /// 是因为清扫要遍历所有条目，没法先来一遍 O(n) 的清零。
    pub(crate) epoch: AtomicU32,
    /// 槽位数（对象 = 字段数，数组 = 元素数）
    pub(crate) nslots: u32,
    /// 运行时类 id（`Heap::class_names` 里的下标；对象是内部类名，数组是描述符）
    pub(crate) class_id: u32,
    /// 引用槽位图：≤64 槽内联位图；>64 槽存 `*const RefBits`（泄漏的静态实例）
    refs: u64,
}

/// 对象头必须是 24 字节 —— 这是 RSS 与标记停顿的关键指标，改坏了编译期就报
const _: () = assert!(std::mem::size_of::<ObjHdr>() == OBJ_HDR_SIZE);
/// 槽位单元必须是 16 字节（内联数组的步长靠它）
const _: () = assert!(std::mem::size_of::<Cell>() == CELL_SIZE);

impl ObjHdr {
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.nslots as usize
    }

    /// 槽位数组（紧跟对象头，长度 `nslots`）
    #[inline]
    fn cells(&self) -> &[Cell] {
        unsafe {
            let p = (self as *const ObjHdr as *const u8).add(OBJ_HDR_SIZE) as *const Cell;
            std::slice::from_raw_parts(p, self.nslots as usize)
        }
    }

    #[inline]
    pub(crate) fn get(&self, i: usize) -> Option<Slot> {
        self.cells().get(i).map(|c| c.get())
    }

    #[inline]
    pub(crate) fn set(&self, i: usize, v: Slot) {
        if let Some(c) = self.cells().get(i) {
            c.set(v);
        }
    }

    /// 第 i 槽是不是引用
    ///
    /// `nslots > 64` 时 `refs` 是一个**指针**（指向堆上的 `RefBits`）。
    /// **`refs == 0` 必须当成「没有引用槽」而不是去解引用** ——
    /// 空指针 deref 在 release 下就是 SIGSEGV，而且**没有任何 Rust panic**，
    /// 症状是「只要分配超过 64 槽的对象（长字符串、大 `byte[]`、大数组）就崩」，
    /// 极易被误判成别处的内存破坏。实测：`"x".repeat(200)` 必崩。
    #[inline]
    pub(crate) fn is_ref(&self, i: usize) -> bool {
        if self.nslots <= 64 {
            i < 64 && (self.refs >> i) & 1 != 0
        } else if self.refs == 0 {
            false
        } else {
            // `nslots`/`refs` 只写一次、之后不变，所以这里读到的指针永远同一个
            (unsafe { &*(self.refs as usize as *const RefBits) }).is_ref(i)
        }
    }

    /// 有没有引用槽（没有的话标记时连值都不用碰 —— 少一次访存就是少一次 miss）
    #[inline]
    pub(crate) fn has_refs(&self) -> bool {
        if self.nslots <= 64 {
            self.refs != 0
        } else if self.refs == 0 {
            false
        } else {
            !matches!(unsafe { &*(self.refs as usize as *const RefBits) }, RefBits::None)
        }
    }

    /// 槽位数组的首地址（GC 预取用：访问它才算把对象的"身体"拉进缓存）
    #[inline]
    pub(crate) fn cells_ptr(&self) -> *const Cell {
        self.cells().as_ptr()
    }

    /// 扫一遍引用槽，**只把年轻代的目标**推进 `out`。年轻代回收的根扫描用它。
    ///
    /// 这是分代回收最值钱的一条优化：老对象（比如一个装着全部存活对象的大数组）
    /// 被写脏之后，它的每个槽都得看一遍，但槽里绝大多数指向的是**老**对象 ——
    /// 那些目标不需要标记、更不需要追。只比较一次下标就扔掉，省掉一次页表查找
    /// （实测 GcBench 的 1M 槽 `keep` 数组：这一条把停顿从 47ms 压到毫秒级）。
    #[inline]
    pub(crate) fn scan_refs_young(&self, out: &mut Vec<i32>, lo_obj: u32, lo_arr: u32) {
        let cells = self.cells();
        let n = cells.len();
        let mut push = |v: i32| {
            if v > 0 {
                if (v as u32 - 1) >= lo_obj {
                    out.push(v);
                }
            } else if v < 0 && (-(v as i64) - 1) as u64 >= lo_arr as u64 {
                out.push(v);
            }
        };
        if self.nslots <= 64 {
            let mut bits = self.refs;
            while bits != 0 {
                let i = bits.trailing_zeros() as usize;
                if i >= n {
                    break;
                }
                if let Slot::I(v) = cells[i].get() {
                    if v != 0 {
                        push(v);
                    }
                }
                bits &= bits - 1;
            }
        } else if self.refs != 0 {
            for i in 0..n {
                if self.is_ref(i) {
                    if let Slot::I(v) = cells[i].get() {
                        if v != 0 {
                            push(v);
                        }
                    }
                }
            }
        }
    }

    /// 扫一遍引用槽：把非零引用推进 `out`。**标记热循环专用**。
    ///
    /// 两个优化叠在一起：槽位切片只取一次（逐槽调 `get` 会为每槽重算一次指针），
    /// 以及按位图驱动 —— 没有引用槽的对象（实测负载里是大多数）直接跳出，
    /// 一次数组都不用碰。
    #[inline]
    pub(crate) fn scan_refs(&self, out: &mut Vec<i32>) {
        let cells = self.cells();
        let n = cells.len();
        if self.nslots <= 64 {
            let mut bits = self.refs;
            while bits != 0 {
                let i = bits.trailing_zeros() as usize;
                if i >= n {
                    break;
                }
                if let Slot::I(v) = cells[i].get() {
                    if v != 0 {
                        out.push(v);
                    }
                }
                bits &= bits - 1;
            }
        } else {
            for i in 0..n {
                if self.is_ref(i) {
                    if let Slot::I(v) = cells[i].get() {
                        if v != 0 {
                            out.push(v);
                        }
                    }
                }
            }
        }
    }

    /// 快照（冷路径用：反射/indy 要整块读）
    pub(crate) fn snapshot(&self) -> Vec<Slot> {
        self.cells().iter().map(|c| c.get()).collect()
    }

    /// 这个对象在堆里占多少字节（统计用）
    pub(crate) fn bytes(&self) -> u64 {
        (OBJ_HDR_SIZE + CELL_SIZE * self.nslots as usize) as u64
    }
}

/// 一个对象块的分配布局（头 + 槽位）
#[inline]
fn obj_layout(n: usize) -> std::alloc::Layout {
    std::alloc::Layout::from_size_align(OBJ_HDR_SIZE + CELL_SIZE * n, 8)
        .expect("对象布局非法（槽位数溢出）")
}

/// 新对象槽位的填充方式
pub(crate) enum Fill<'a> {
    /// 全零（`Slot::I(0)`，等价于 memset 0 —— 走批量写，不做逐槽 encode）
    Zero,
    /// 全部同一个值（数组的初值 / 对象的默认值）
    Uniform(Slot),
    /// 逐槽给值（不够的位置补 `I(0)`）
    Slice(&'a [Slot]),
}

/// 分配一个对象块所需的类信息
#[derive(Clone, Copy)]
struct ClassMeta {
    id: u32,
    refs: u64,
}

/// 分配对象块并**一次写完**头与槽位（清零与写头合并，省掉一次内存往返）
fn alloc_obj(n: usize, m: ClassMeta, epoch: u32, fill: Fill) -> *mut ObjHdr {
    let layout = obj_layout(n);
    let p = unsafe { std::alloc::alloc(layout) } as *mut ObjHdr;
    if p.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    unsafe {
        std::ptr::addr_of_mut!((*p).lock).write(AtomicU32::new(0));
        std::ptr::addr_of_mut!((*p).epoch).write(AtomicU32::new(epoch));
        std::ptr::addr_of_mut!((*p).nslots).write(n as u32);
        std::ptr::addr_of_mut!((*p).class_id).write(m.id);
        std::ptr::addr_of_mut!((*p).refs).write(m.refs);
        let cells = (p as *mut u8).add(OBJ_HDR_SIZE) as *mut Cell;
        match fill {
            Fill::Zero => Cell::fill_zero(cells, n),
            Fill::Uniform(v) => Cell::fill(cells, v, n),
            Fill::Slice(src) => {
                let k = src.len().min(n);
                for i in 0..k {
                    Cell::write(cells.add(i), src[i]);
                }
                if k < n {
                    Cell::fill(cells.add(k), Slot::I(0), n - k);
                }
            }
        }
    }
    p
}

/// 释放对象块（`nslots` 只写一次，直接读回来算布局）
#[inline]
unsafe fn free_obj(p: *mut ObjHdr) {
    let n = (*p).nslots as usize;
    std::alloc::dealloc(p as *mut u8, obj_layout(n));
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
    /// 条目是**裸分配块**（对象头 + 内联槽位），释放必须由堆提供正确布局
    free: fn(*mut T),
}

impl<T> PtrTable<T> {
    fn new(free: fn(*mut T)) -> PtrTable<T> {
        let pages = (0..ROOT_PAGES).map(|_| AtomicPtr::new(std::ptr::null_mut())).collect::<Vec<_>>();
        PtrTable { pages: pages.into_boxed_slice(), count: AtomicU32::new(0), free }
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
    fn push(&self, v: *mut T) -> Option<u32> {        let idx = self.count.fetch_add(1, Ordering::AcqRel);
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
        unsafe { (*page).entries[idx as usize & (PAGE - 1)].store(v, Ordering::Release) };
        Some(idx)
    }

    /// 清扫：对 `0..count` 的每个非空条目调 `keep(下标, &对象)`，返回 `false` 就
    /// **释放**它（`Box::from_raw` + 条目置空）。返回释放掉的对象数。
    ///
    /// 索引**不复用**：句柄就是索引，复用会让「某个还活在别处的陈旧句柄」指向另一台
    /// 对象 —— 那是静默数据损坏，比多占一点页表内存糟得多。
    fn sweep(&self, keep: impl FnMut(u32, &T) -> bool) -> u32 {
        self.sweep_from(0, keep)
    }

    /// 只扫 `[lo, count)`（年轻代回收：老条目一个都不碰，代价 ∝ 新生代）
    fn sweep_from(&self, lo: u32, mut keep: impl FnMut(u32, &T) -> bool) -> u32 {
        let n = self.count.load(Ordering::Acquire);
        let mut freed = 0u32;
        for i in lo.min(n)..n {
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
                (self.free)(e);
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

// ---------------------------------------------------------------- 堆

/// 对象 / 数组 / 字符串三套堆。
pub(crate) struct Heap {
    objs: PtrTable<ObjHdr>,
    arrs: PtrTable<ObjHdr>,
    /// class_id → 类名/描述符。对象头里只放 4 字节 id（旧布局放 16 字节 `Arc<str>`）。
    class_names: RwLock<Vec<Arc<str>>>,
    /// 类名 → class_id（分配热路径唯一的一次查表）
    class_ids: RwLock<HashMap<Box<str>, u32>>,

    // ---- 分代 ----
    /// 年轻代下界：下标 `>= 它` 的对象是「上一次年轻代回收之后分配的」
    young_lo_obj: AtomicU32,
    young_lo_arr: AtomicU32,
    /// 写屏障开关（年轻代回收启用；关掉 = 退回全量标记-清扫）
    barrier_on: AtomicBool,
    /// 老对象里被写过引用的是哪些（跨代引用的来源）
    dirty_obj: DirtyMap,
    dirty_arr: DirtyMap,
    /// 距上次年轻代回收分配了多少字节（决定要不要回收 eden）
    pub(crate) young_bytes: AtomicU64,
    /// 距上次**全量**回收分配了多少字节（全量回收的判据）
    major_bytes: AtomicU64,
    /// 堆里活对象大致占多少字节（净值：分配加、释放减）
    bytes_live: AtomicU64,
    /// 上一次全量回收之后的存活字节（下次全量的阈值材料）
    live_bytes_after: AtomicU64,
    /// 做过全量回收了吗（**第一次必须是全量**：冷启动时「年轻代下界之前都活着」
    /// 这个前提还不成立 —— 那批对象从来没有被验证过可达性）
    did_major: AtomicBool,
    /// 年轻代触发阈值（字节，按存活率自适应）
    eden_bytes: AtomicU64,
    /// 年轻代回收次数（自适应与统计用）
    pub(crate) minor_count: AtomicU64,
    /// 上一个年轻代回收的存活率（万分比）
    survivor_bp: AtomicU32,
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
    /// 上一轮**全量**标记标了多少个。全量回收的并行判据用它 —— 用 `last_marked`
    /// 会被紧随其后的年轻代回收写回一个小数字，于是每次全量都退化成单线程
    /// （实测 GcBench 8M/1M：单线程追 100 万活对象 47ms，8 线程 7ms）。
    pub(crate) last_major_marked: AtomicU64,
    /// 上一轮**全量**标记标了多少个。全量回收的并行判据用它 —— 用 `last_marked`
    /// 会被紧随其后的年轻代回收写回一个小数字，于是每次全量都退化成单线程
    /// （实测 GcBench：单线程追 100 万活对象 47ms，8 线程 7ms）。
    /// 累计已释放的对象数（`live()` 用它把页表高水位折成活对象数）
    pub(crate) reclaimed: AtomicU64,
    /// 类表快照（算字段引用位图用；`store_publish` 与构造时挂上）
    store: RwLock<Option<Arc<ClassStore>>>,
    /// 类名 → 字段引用位图（每个类只算一次）
    layouts: RwLock<HashMap<String, ClassMeta>>,
}

/// 老对象「被写过引用」的精确位图：1 bit 一个对象/数组。
///
/// 为什么不用卡表：卡表按地址分块，一块里可能塞着 512 个对象，年轻代回收时得把
/// 整块对象都扫一遍。位图一 bit 一个对象，**精确到对象**，代价只是一次无锁
/// `fetch_or`；位图本身按需分页（一页 4096 个 u64 = 26 万个对象），没写过的页
/// 根本不存在，所以它对 RSS 的影响是「写过多少算多少」而不是「堆有多大」。
const DMAP_WORDS: usize = 1 << 12;
/// 一页覆盖的对象数：4096 × 64 = 262144
const DMAP_SHIFT: u32 = 12 + 6;
const DMAP_WMASK: usize = DMAP_WORDS - 1;
/// 页表根：16384 页 × 26.2 万 = 43 亿个对象，根本身 128KB
const DMAP_PAGES: usize = 1 << 14;

struct DPage {
    w: [AtomicU64; DMAP_WORDS],
}

pub(crate) struct DirtyMap {
    pages: Box<[AtomicPtr<DPage>]>,
    /// 已建到第几页（扫描只看这里）
    npages: AtomicU32,
}

impl DirtyMap {
    fn new() -> DirtyMap {
        DirtyMap {
            pages: (0..DMAP_PAGES).map(|_| AtomicPtr::new(std::ptr::null_mut())).collect(),
            npages: AtomicU32::new(0),
        }
    }

    #[inline]
    fn page_or_make(&self, p: usize) -> Option<&DPage> {
        if p >= self.pages.len() {
            return None;
        }
        let cur = self.pages[p].load(Ordering::Acquire);
        if !cur.is_null() {
            return Some(unsafe { &*cur });
        }
        let fresh = Box::into_raw(Box::new(DPage {
            w: std::array::from_fn(|_| AtomicU64::new(0)),
        }));
        match self.pages[p].compare_exchange(
            std::ptr::null_mut(),
            fresh,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {}
            Err(winner) => {
                drop(unsafe { Box::from_raw(fresh) });
                return unsafe { Some(&*winner) };
            }
        }
        // 发布页数（扫描端按它循环）
        let need = p as u32 + 1;
        let mut cur = self.npages.load(Ordering::Acquire);
        while cur < need {
            match self.npages.compare_exchange_weak(
                cur,
                need,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(c) => cur = c,
            }
        }
        unsafe { Some(&*fresh) }
    }

    /// 记一笔：老对象 `idx` 里写过引用
    #[inline]
    pub(crate) fn set(&self, idx: u32) {
        let p = (idx >> DMAP_SHIFT) as usize;
        let Some(page) = self.page_or_make(p) else { return };
        let w = ((idx as usize) >> 6) & DMAP_WMASK;
        page.w[w].fetch_or(1u64 << (idx & 63), Ordering::Relaxed);
    }

    /// 遍历并**原子清空**（调用方必须已经把世界停住，否则会丢掉并发写的账）
    pub(crate) fn drain(&self, mut f: impl FnMut(u32)) {
        let n = self.npages.load(Ordering::Acquire) as usize;
        for p in 0..n.min(self.pages.len()) {
            let ptr = self.pages[p].load(Ordering::Acquire);
            if ptr.is_null() {
                continue;
            }
            let page = unsafe { &*ptr };
            for w in 0..DMAP_WORDS {
                let v = page.w[w].swap(0, Ordering::Relaxed);
                if v == 0 {
                    continue;
                }
                let base = ((p as u32) << DMAP_SHIFT) + ((w as u32) << 6);
                let mut bits = v;
                while bits != 0 {
                    let b = bits.trailing_zeros();
                    f(base + b);
                    bits &= bits - 1;
                }
            }
        }
    }
}


impl Heap {
    pub(crate) fn new() -> Heap {
        Heap {
            objs: PtrTable::new(|p| unsafe { free_obj(p) }),
            arrs: PtrTable::new(|p| unsafe { free_obj(p) }),
            class_names: RwLock::new(Vec::new()),
            class_ids: RwLock::new(HashMap::new()),
            young_lo_obj: AtomicU32::new(0),
            young_lo_arr: AtomicU32::new(0),
            barrier_on: AtomicBool::new(true),
            dirty_obj: DirtyMap::new(),
            dirty_arr: DirtyMap::new(),
            young_bytes: AtomicU64::new(0),
            major_bytes: AtomicU64::new(0),
            bytes_live: AtomicU64::new(0),
            live_bytes_after: AtomicU64::new(0),
            did_major: AtomicBool::new(false),
            eden_bytes: AtomicU64::new(EDEN_BYTES_MIN),
            minor_count: AtomicU64::new(0),
            survivor_bp: AtomicU32::new(0),
            strings: RwLock::new(Vec::new()),
            str_handles: Mutex::new(HashMap::new()),
            str_fields: Mutex::new(HashMap::new()),
            sb_bufs: Mutex::new(HashMap::new()),
            wrapper_cache: Mutex::new(HashMap::new()),
            gc_epoch: AtomicU32::new(1),
            live_after_gc: AtomicU64::new(0),
            last_marked: AtomicU64::new(0),
            last_major_marked: AtomicU64::new(0),
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

    /// 类名 + 槽数 → 分配对象头所需的一切：class_id 与引用位图字。
    ///
    /// 按类名缓存（每个类只算一次布局）；算不出来（类不在类表里 / 槽数和布局对不上）
    /// 就退化成 `All`：GC 会把每个槽都拿页表验证一遍 —— 多留一点垃圾，但绝不会漏掉
    /// 一个真引用。
    #[inline]
    fn class_meta(&self, class: &str, n: usize) -> ClassMeta {
        if let Some(m) = self.layouts.read().unwrap_or_else(|e| e.into_inner()).get(class) {
            return *m;
        }
        let id = self.intern_class_id(class);
        let g = self.store.read().unwrap_or_else(|e| e.into_inner()).clone();
        let bits = match g {
            Some(store) => class_layout(&store, class, n),
            None => RefBits::All,
        };
        let m = ClassMeta { id, refs: bits.to_word(n) };
        self.layouts
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(class.to_string(), m);
        m
    }

    /// 类名驻留：同一个类名只拿一个 id（对象头里只放 4 字节）
    fn intern_class_id(&self, name: &str) -> u32 {
        {
            let g = self.class_ids.read().unwrap_or_else(|e| e.into_inner());
            if let Some(&id) = g.get(name) {
                return id;
            }
        }
        let mut g = self.class_ids.write().unwrap_or_else(|e| e.into_inner());
        if let Some(&id) = g.get(name) {
            return id;
        }
        let mut names = self.class_names.write().unwrap_or_else(|e| e.into_inner());
        let id = names.len() as u32;
        names.push(Arc::from(name));
        g.insert(Box::from(name), id);
        id
    }

    /// class_id → 类名
    #[inline]
    fn class_name(&self, id: u32) -> Option<Arc<str>> {
        self.class_names
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(id as usize)
            .cloned()
    }

    /// 分配一个对象，返回句柄（> 0）。类名决定字段数由调用方给。
    pub(crate) fn new_object(&self, slots: Vec<Slot>, class: &str) -> Option<i32> {
        let n = slots.len().max(1);
        let m = self.class_meta(class, n);
        let size = OBJ_HDR_SIZE + CELL_SIZE * n;
        self.charge(size);
        let p = alloc_obj(n, m, self.gc_epoch.load(Ordering::Acquire), Fill::Slice(&slots));
        // 句柄 = 下标 + 1，而 `handle >= STR_BASE` 会被 `ref_kind` 判成**字符串**。
        // 所以对象下标必须 < STR_BASE - 1，否则第 16777216 个对象起会拿到一个
        // 「看起来是字符串」的句柄 —— 那是静默的语义错乱，比 OOM 糟得多。
        // 页表本身能放到 3355 万个，这道闸是句柄编码先撞上的，所以按句柄上限拦。
        let i = match self.objs.push(p) {
            Some(i) => i,
            None => {
                unsafe { free_obj(p) };
                return None;
            }
        };
        if i as i32 + 1 >= STR_BASE {
            return None;
        }
                Some(i as i32 + 1)
    }

    /// 分配一个数组，返回句柄（< 0）
    pub(crate) fn new_array(&self, elems: Vec<Slot>, desc: &str) -> Option<i32> {
        self.alloc_array_fill(elems.len(), desc, Fill::Slice(&elems))
    }

    /// 分配一个元素全为默认值的数组（长度 `n`）
    pub(crate) fn alloc_array(&self, n: usize, desc: &str, zero: Slot) -> Option<i32> {
        self.alloc_array_fill(n, desc, Fill::Uniform(zero))
    }

    /// 数组分配的唯一实现（`vec![zero; n]` 那条老路会先分配一个 n 槽的临时 Vec ——
    /// `new int[10000]` 时那是 160KB 的纯浪费）
    fn alloc_array_fill(&self, n: usize, desc: &str, fill: Fill) -> Option<i32> {
        // `Slot::I(0)` 的编码正好全 0 → 用 memset 一次写完，省掉 n 次逐槽 encode
        // （`new int[n]` / `new Object[n]` / 三色标记里的辅助数组都走这条）
        let fill = match fill {
            Fill::Uniform(Slot::I(0)) => Fill::Zero,
            other => other,
        };
        // 数组元素类型由描述符决定：`[L…;` / `[[…` 是引用数组，其余是基本类型。
        // 位图只有两个常量，直接由描述符算出字，不做任何查表。
        let refs = if n > 0 && is_ref_array_desc(desc) {
            if n <= 64 {
                u64::MAX
            } else {
                refbits_all_static() as *const RefBits as usize as u64
            }
        } else {
            0
        };
        let id = self.intern_class_id(desc);
        let size = OBJ_HDR_SIZE + CELL_SIZE * n;
        self.charge(size);
        let p = alloc_obj(n, ClassMeta { id, refs }, self.gc_epoch.load(Ordering::Acquire), fill);
        let i = match self.arrs.push(p) {
            Some(i) => i,
            None => {
                unsafe { free_obj(p) };
                return None;
            }
        };
                Some(-(i as i32) - 1)
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
            if i < o.len() {
                o.set(i, v);
                // 这一格按布局是引用就必须记脏账：全仓有几十处是 `heap.set` 直接写
                // 引用槽（反射 / JNI / indy / natives），逐个改成 set_ref 做不到，
                // 所以把屏障放在**唯一的存储入口**上，按槽位位图判断。
                if self.barrier_on.load(Ordering::Relaxed) && o.is_ref(i) {
                    self.mark_dirty(h, o);
                }
            }
        }
    }

    /// 对象字段的写屏障：引用字段写进来时给**老对象**记一笔脏账。
    ///
    /// 年轻代回收只看根集是不够的：老年代对象里完全可能存着指向年轻代的引用，
    /// 而根集里没有这条边。所以「老对象被写过引用」必须在 store 时就记下来，
    /// 回收时把那些老对象也当根扫一遍（见 `gc.rs` 的 `collect`）。
    ///
    /// 代价：一次 `AtomicBool` 读 + 一次 `AtomicU32` 读；只有源对象是老对象时才
    /// 真去 `fetch_or` 一个位。目标是不是年轻代**故意不判断** —— 多记一笔只是
    /// 多扫一个对象，漏记就是把活对象回收掉。
    #[inline]
    pub(crate) fn set_ref(&self, h: i32, i: usize, v: Slot) {
        if let Some(o) = self.obj(h).or_else(|| self.arr(h)) {
            if i < o.len() {
                o.set(i, v);
                // 这里是**确定**的引用写入：不看位图，一律记脏账（位图算错时宁可多扫）
                if self.barrier_on.load(Ordering::Relaxed) {
                    self.mark_dirty(h, o);
                }
            }
        }
    }

    /// 老对象被写过引用 → 记一笔（源是老对象才记；目标是不是年轻代故意不判断）
    #[inline]
    fn mark_dirty(&self, h: i32, _o: &ObjHdr) {
        if h > 0 {
            let idx = (h - 1) as u32;
            if idx < self.young_lo_obj.load(Ordering::Relaxed) {
                self.dirty_obj.set(idx);
            }
        } else {
            let idx = (-h - 1) as u32;
            if idx < self.young_lo_arr.load(Ordering::Relaxed) {
                self.dirty_arr.set(idx);
            }
        }
    }

    pub(crate) fn snapshot(&self, h: i32) -> Option<Vec<Slot>> {
        self.obj(h).or_else(|| self.arr(h)).map(|o| o.snapshot())
    }

    pub(crate) fn class_of(&self, h: i32) -> Option<Arc<str>> {
        let id = self.obj(h).or_else(|| self.arr(h))?.class_id;
        self.class_name(id)
    }

    /// 数组长度（不是数组返回 None）
    pub(crate) fn array_len(&self, h: i32) -> Option<usize> {
        self.arr(h).map(|o| o.nslots as usize)
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
        self.sweep_from(0, 0, epoch)
    }

    /// 只清扫两个表里下标 `>= lo` 的区段（年轻代回收用：老年代的条目一个都不碰）
    pub(crate) fn sweep_from(&self, lo_obj: u32, lo_arr: u32, epoch: u32) -> (u32, u32, u64) {
        let mut bytes = 0u64;
        let o = self.objs.sweep_from(lo_obj, |_, obj| {
            if obj.epoch.load(Ordering::Acquire) == epoch {
                return true;
            }
            bytes += obj.bytes();
            false
        });
        let a = self.arrs.sweep_from(lo_arr, |_, obj| {
            if obj.epoch.load(Ordering::Acquire) == epoch {
                return true;
            }
            bytes += obj.bytes();
            false
        });
        self.reclaimed.fetch_add(o as u64 + a as u64, Ordering::AcqRel);
        self.bytes_live.fetch_sub(bytes, Ordering::AcqRel);
        (o, a, bytes)
    }

    /// 现在堆里活着的对象数（对象 + 数组）
    pub(crate) fn live(&self) -> u64 {
        let (o, a) = self.counts();
        let dead = self.reclaimed.load(Ordering::Acquire);
        (o as u64 + a as u64).saturating_sub(dead)
    }

    /// 再分配多少次就该回收一轮（「存活 × 2」与下限取大）—— 全量回收的判据
    pub(crate) fn gc_threshold(&self) -> u64 {
        (self.live_after_gc.load(Ordering::Acquire) * 2).max(MIN_GC_THRESHOLD)
    }

    // ---------- 分代 ----------

    /// 把一次分配的字节数记账（决定 eden 什么时候满）
    ///
    /// 大对象只按 `los_bytes` 计 —— 与 G1 把大对象直接放进独立 region（不占 eden）
    /// 同一个道理：否则一个 10MB 数组一分配就把 eden 撑爆，回收频率完全由它决定。
    #[inline]
    fn charge(&self, bytes: usize) {
        let b = bytes as u64;
        let c = if b > self.los_bytes() { self.los_bytes() } else { b };
        self.young_bytes.fetch_add(c, Ordering::Relaxed);
        self.major_bytes.fetch_add(c, Ordering::Relaxed);
        self.bytes_live.fetch_add(b, Ordering::Relaxed);
    }

    /// 大对象阈值：eden 的 1/8（G1 的 `G1HeapRegionSize` 思路，但不用固定 64KB ——
    /// eden 是自适应的，阈值跟着它走才不会在 eden 收缩之后把一个普通数组误判成大对象）
    #[inline]
    pub(crate) fn los_bytes(&self) -> u64 {
        (self.eden_bytes.load(Ordering::Relaxed) / 8).max(64 << 10)
    }

    #[inline]
    pub(crate) fn young_lo_obj(&self) -> u32 {
        self.young_lo_obj.load(Ordering::Acquire)
    }

    #[inline]
    pub(crate) fn young_lo_arr(&self) -> u32 {
        self.young_lo_arr.load(Ordering::Acquire)
    }

    /// 这个对象索引是不是年轻的
    #[inline]
    pub(crate) fn young_obj(&self, idx: u32) -> bool {
        idx >= self.young_lo_obj.load(Ordering::Relaxed)
    }

    #[inline]
    pub(crate) fn young_arr(&self, idx: u32) -> bool {
        idx >= self.young_lo_arr.load(Ordering::Relaxed)
    }

    /// 年轻代里已经分配了多少字节 / eden 上限
    #[inline]
    pub(crate) fn young_bytes(&self) -> u64 {
        self.young_bytes.load(Ordering::Relaxed)
    }

    #[inline]
    pub(crate) fn eden_bytes(&self) -> u64 {
        self.eden_bytes.load(Ordering::Relaxed)
    }

    #[inline]
    pub(crate) fn survivor_bp(&self) -> u32 {
        self.survivor_bp.load(Ordering::Relaxed)
    }

    /// 一轮年轻代回收之后：edens 归零、存活率反馈、下界推到当前末尾（= 全部晋升）
    ///
    /// 存活率反馈是 G1 那套 `-XX:AdaptiveSizePolicy` 的简化版：存活率高说明
    /// 大多数对象会活很久，收得太勤只是白烧 CPU —— 把 eden 放大；存活率极低
    /// 说明这波就是短命对象，把 eden 收小，让峰值垃圾量跟着降。
    pub(crate) fn young_after(&self, young_before: u64, survivors: u64) {
        self.young_bytes.store(0, Ordering::Relaxed);
        let (o, a) = self.counts();
        self.young_lo_obj.store(o, Ordering::Release);
        self.young_lo_arr.store(a, Ordering::Release);
        self.minor_count.fetch_add(1, Ordering::AcqRel);
        let bp = if young_before == 0 { 0 } else { (survivors * 1_000_000 / young_before) as u32 };
        self.survivor_bp.store(bp, Ordering::Relaxed);
        let cur = self.eden_bytes.load(Ordering::Relaxed);
        let next = if bp >= 500_000 {
            (cur * 2).min(EDEN_BYTES_MAX)
        } else if bp <= 20_000 {
            (cur / 2).max(EDEN_BYTES_MIN)
        } else {
            cur
        };
        self.eden_bytes.store(next, Ordering::Relaxed);
    }

    /// 全量回收之后：所有对象都是「老」的，脏账清空（它们已经不在年轻代里了）
    pub(crate) fn major_after(&self) {
        let (o, a) = self.counts();
        self.major_bytes.store(0, Ordering::Relaxed);
        self.young_lo_obj.store(o, Ordering::Release);
        self.young_lo_arr.store(a, Ordering::Release);
        self.young_bytes.store(0, Ordering::Relaxed);
        self.dirty_drain(|_| {});
    }

    /// 把老对象的脏账全部取出来（回调收到**句柄**，GC 直接当根扫）
    pub(crate) fn dirty_drain(&self, mut f: impl FnMut(i32)) {
        self.dirty_obj.drain(|idx| f(idx as i32 + 1));
        self.dirty_arr.drain(|idx| f(-(idx as i32) - 1));
    }

    /// 这一轮该做哪种回收（`None` = 还不用收）
    ///
    /// 判据只有两条，都是字节口径（旧代码用「对象个数」，同样的 `new int[3]` 与
    /// `new long[64]` 触发次数一样 —— 那是错的）：
    /// 1. eden 满了 → 年轻代回收（只看新生代，停顿 ∝ 新生代而不是整个活集）；
    /// 2. 页表长得比活对象多太多 → 全量回收（老年代里攒了垃圾）。
    #[inline]
    pub(crate) fn gc_kind(&self) -> GcKind {
        if !self.did_major.load(Ordering::Relaxed) {
            return if self.young_bytes.load(Ordering::Relaxed) >= EDEN_BYTES_MIN {
                GcKind::Major
            } else {
                GcKind::None
            };
        }
        if self.young_bytes.load(Ordering::Relaxed) >= self.eden_bytes.load(Ordering::Relaxed) {
            return GcKind::Minor;
        }
        // 全量回收按「自上次全量以来分配了多少字节」判，**不能按页表高水位判**：
        // 索引不复用，高水位只增不减 —— 按它判会在第一次全量之后每轮都成立，
        // 退化成「每个检查点都做一次全量回收」。
        let since = self.major_bytes.load(Ordering::Relaxed);
        let live = self.live_bytes_after.load(Ordering::Relaxed);
        if since >= (live + live / 2).max(MIN_MAJOR_BYTES) {
            return GcKind::Major;
        }
        GcKind::None
    }

    /// 全量回收结束：阈值材料换成「现在的存活字节」，自上次全量的计数清零
    pub(crate) fn major_settle(&self) {
        self.live_bytes_after.store(self.bytes_live.load(Ordering::Relaxed), Ordering::Relaxed);
        self.major_bytes.store(0, Ordering::Relaxed);
        self.did_major.store(true, Ordering::Release);
    }
}

/// 回收的种类
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum GcKind {
    /// 不收
    None,
    /// 只收年轻代（停顿 ∝ 新生代）
    Minor,
    /// 全量标记-清扫（停顿 ∝ 活集）
    Major,
}

/// eden 起步 8MB —— 比 G1 的默认 region 数（约 5% 堆）小得多，因为这里不做复制，
/// 年轻代回收只是「标记一遍再清扫一遍」，回收频率高一点比 eden 大了攒停顿划算。
pub(crate) const EDEN_BYTES_MIN: u64 = 8 << 20;
/// eden 上限：再大就只是把峰值垃圾量堆起来
pub(crate) const EDEN_BYTES_MAX: u64 = 96 << 20;
/// 全量回收的最短间隔（字节）：再小就退化成「一直在做全量回收」
pub(crate) const MIN_MAJOR_BYTES: u64 = 32 << 20;

/// 触发阈值下限：不管堆里有多少活对象，至少分配这么多次才考虑回收
pub(crate) const MIN_GC_THRESHOLD: u64 = 262_144;


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

// ---------------------------------------------------------------- 静态字段表

/// 静态字段表：**两条访问路径**。
///
/// - **名字键** `(声明类, 字段名)`：反射 / Unsafe / native / 系统属性走这条，冷，形状与
///   原来的 `HashMap<(String, String), Slot>` 一样（`get` / `insert` / `values`），
///   所以外部调用点一行都不用改。
/// - **槽号**：解释器 `getstatic` / `putstatic` 走这条。槽号就是 `vals` 的下标，
///   读到值只要一次下标 + 一次拷贝 —— **零分配、零哈希**。槽号按 `(类, 名)` 分配、
///   **永不复用**（复用会让调用点缓存指向别的字段）。
///
/// 为什么把值搬出哈希表：解释器每条 `getstatic` 都要 `clone` 两个 `String` 再哈希一遍
/// （exec.rs 的快路），实测 305 ns；`putstatic` 还要再走一遍继承链解析（每次约 800 ns
/// 的 `chain()` 分配），1122 ns。键换成整数下标后这两笔固定开销直接消失。
///
/// 语义不变：名字键与槽号**指向同一份值**（`insert` 先拿槽号再写），
/// 所以 native 按名字写进去的值，解释器按槽号读得到（`System.setOut` 就靠这个）。
pub(crate) struct StaticStore {
    /// `(类, 字段名)` → 槽号
    by_name: HashMap<(String, String), u32>,
    /// 槽号 → 值。下标 0 是空槽（用 `0` 表示「调用点还没解析」），真实槽号从 1 开始。
    vals: Vec<Slot>,
}

impl StaticStore {
    fn new() -> StaticStore {
        StaticStore {
            by_name: HashMap::new(),
            vals: vec![Slot::I(0)],
        }
    }

    /// 冷路径：按名字读
    pub(crate) fn get(&self, k: &(String, String)) -> Option<&Slot> {
        self.by_name.get(k).map(|&i| &self.vals[i as usize])
    }

    /// 冷路径：按名字写（没有就建槽）。返回旧值，形状与 `HashMap::insert` 一致
    pub(crate) fn insert(&mut self, k: (String, String), v: Slot) -> Option<Slot> {
        let i = self.slot_or_new(k.0.as_str(), k.1.as_str());
        Some(std::mem::replace(&mut self.vals[i as usize], v))
    }

    /// `(类, 字段名)` 的槽号，没有就分配一个（**永不复用**）
    pub(crate) fn slot_of(&mut self, class: &str, name: &str) -> u32 {
        self.slot_or_new(class, name)
    }

    fn slot_or_new(&mut self, class: &str, name: &str) -> u32 {
        // 这次临时元组只在首次解析一个字段时出现一次（调用点会缓存槽号），不在热路径上
        if let Some(&i) = self.by_name.get(&(class.to_string(), name.to_string())) {
            return i;
        }
        let i = self.vals.len() as u32;
        self.by_name.insert((class.to_string(), name.to_string()), i);
        self.vals.push(Slot::I(0));
        i
    }

    /// 热路径：按槽号读
    #[inline]
    pub(crate) fn value(&self, id: u32) -> Slot {
        self.vals[id as usize]
    }

    /// 热路径：按槽号写
    #[inline]
    pub(crate) fn set(&mut self, id: u32, v: Slot) {
        self.vals[id as usize] = v;
    }

    /// GC 扫根：表里的每个 `Slot::I(h)` 都是候选句柄
    pub(crate) fn values(&self) -> std::slice::Iter<'_, Slot> {
        self.vals.iter()
    }
}

impl Shared {
    /// 取（必要时建）某个类的「`<clinit>` 已完成」原子标志。
    ///
    /// 泄漏成 `'static`：类不会卸载，一个类一个字节，调用点缓存直接存引用、不做引用计数
    /// （热路径上不能有 `Arc` 的原子增减）。
    pub(crate) fn init_flag(&self, class: &str) -> &'static AtomicU8 {
        if let Some(&p) = lk(&self.init_flags).get(class) {
            return p;
        }
        let mut g = lk(&self.init_flags);
        if let Some(&p) = g.get(class) {
            return p;
        }
        let p: &'static AtomicU8 = Box::leak(Box::new(AtomicU8::new(0)));
        g.insert(class.to_string(), p);
        p
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
    pub(crate) statics: Mutex<StaticStore>,
    /// 类名 → 「`<clinit>` 已完成」的共享原子标志（`ensure_init` 完成时置 2）。
    ///
    /// 给**调用点缓存**用：`getstatic`/`putstatic` 第一次执行时拿到这里那个 `&'static`
    /// 原子量的地址，之后每次只剩一次 `load`，不再拿 `initialized` 的全局锁。
    /// 权威仍然是 `initialized` + `initializing` 那套状态机（含阻塞等待）——
    /// 这个原子量只是它的**观测快照**，不改任何一次「恰好执行一次」的判定。
    pub(crate) init_flags: Mutex<HashMap<String, &'static AtomicU8>>,
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
            statics: Mutex::new(StaticStore::new()),
            init_flags: Mutex::new(HashMap::new()),
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
