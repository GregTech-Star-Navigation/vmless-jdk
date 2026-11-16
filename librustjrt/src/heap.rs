//! 堆：空间划分（eden / survivor×2 / 老年代 + LOS）、TLAB、写屏障接线、增长收缩、回收策略。
//!
//! # 内存布局（全部用相对基址的 u32 偏移）
//!
//! ```text
//! 0        64                                            eden_end        tail_start        cap
//! ├─保留──┼──────────── eden ────────────┬──semi0──┬──semi1��─┼──── 老年代 + LOS ────┤
//!                                         ↑ 两块 survivor 交替做 from/to
//! ```
//!
//! - 年轻代 = `[young_start, tail_start)`：eden + 两块 survivor 语义空间（复制式回收）。
//! - 老年代 + LOS 共用尾部 arena，从 `tail_start` 向高地址 bump，靠三条表分开记账。
//! - 增长 = `realloc` 整块内存：基址变了但偏移不变，所以 handle 不会失效。

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use crate::alloc::{Region, Tlab};
use crate::barrier::{self, BarrierKind};
use crate::cards::{CardTable, StartMap};
use crate::layout::{LayoutTable, SlotKind};
use crate::object::{self, Object, FLAG_ARRAY, FLAG_LOS, FLAG_MARK, HEADER, SLOT_BYTES};
use crate::old::{self, MajorReport, OldSpace};
use crate::roots::{Handle, RootScope};
use crate::stats::{GcKind, GcRecord, Stats};
use crate::young::{self, MinorReport};

/// `[0, RESERVED)` 保留，偏移 0 永远是 null
const RESERVED: u32 = 64;
const TLAB_MIN: u32 = 32 * 1024;
const TLAB_MAX: u32 = 1 << 20;
const TAIL_MIN: usize = 1 << 20;

/// 堆配置。改名/加字段不会破坏旧 API（旧 `Heap::new(cap)` 在 compat 层）。
#[derive(Clone, Debug)]
pub struct HeapConfig {
    /// 初始容量（字节）
    pub initial: usize,
    /// 上限（超过就直接失败，不再增长）
    pub max: usize,
    /// 收缩不会低于这个值
    pub min_capacity: usize,
    /// eden 占容量的比例
    pub eden_ratio: f64,
    /// 单块 survivor 占容量的比例（两块合计 2×）
    pub semi_ratio: f64,
    /// 大于等于这个字节数的分配直接进 LOS
    pub los_threshold: u32,
    /// 存活年龄达到这个值就晋升老年代
    pub tenuring_threshold: u32,
    /// 老年代占用率超过它就触发 major
    pub major_ratio: f64,
    /// 空闲率超过它就收缩
    pub min_free_ratio: f64,
    pub barrier: BarrierKind,
    /// 保留多少条 GC 历史
    pub history: usize,
}

impl Default for HeapConfig {
    fn default() -> Self {
        HeapConfig {
            initial: 64 << 20,
            max: 4 << 30,
            min_capacity: 8 << 20,
            eden_ratio: 0.48,
            semi_ratio: 0.08,
            los_threshold: 64 * 1024,
            tenuring_threshold: 3,
            major_ratio: 0.7,
            min_free_ratio: 0.6,
            barrier: BarrierKind::IncrementalUpdate,
            history: 64,
        }
    }
}

/// 空间划界的快照（屏障判定用，Copy 便宜）
#[derive(Clone, Copy, Debug)]
pub struct Spaces {
    pub young_start: u32,
    /// 年轻代结束 = 老年代起点
    pub young_end: u32,
    pub tail_start: u32,
    pub cap: u32,
}

impl Spaces {
    #[inline]
    pub fn is_young(&self, off: u32) -> bool {
        off >= self.young_start && off < self.young_end
    }
    #[inline]
    pub fn is_old(&self, off: u32) -> bool {
        off >= self.young_end && off < self.cap
    }
}

/// GC 阶段的共享上下文。各个回收算法（`young::minor` / `old::major` / `old::compact`）
/// 只认这个结构，不认 `Heap` —— 避免借用纠缠，也让算法本身好测。
pub(crate) struct GcEnv<'a> {
    pub base: *mut u8,
    pub cap: u32,
    pub layout: &'a LayoutTable,
    pub young_start: u32,
    pub young_end: u32,
    pub semi: [(u32, u32); 2],
    pub semi_idx: u8,
    pub to_top: &'a Cell<u32>,
    pub old: &'a mut OldSpace,
    pub cards: &'a mut CardTable,
    pub starts: &'a mut StartMap,
    pub roots: &'a mut Vec<u32>,
    pub satb: &'a mut Vec<u32>,
    pub tenuring_threshold: u32,
    pub work: Vec<u32>,
    pub scanned: Vec<u32>,
    /// 本轮开始时年轻代有几个对象（算回收量用）
    pub young_objects: usize,
    pub young_bytes: u32,
    pub copied: usize,
    pub promoted: usize,
    pub promoted_bytes: usize,
    pub survived: usize,
    pub survived_bytes: usize,
    pub max_step_ns: u128,
    pub marked: usize,
    pub failures: usize,
}

impl<'a> GcEnv<'a> {
    #[inline]
    pub fn is_young(&self, off: u32) -> bool {
        off >= self.young_start && off < self.young_end
    }

    /// 是否在「会被复制的那一侧」：eden 或当前 from-space。
    /// to-space 里的对象**不能再复制一次**（否则 Cheney 会无限复制）。
    #[inline]
    pub fn is_evacuable(&self, off: u32) -> bool {
        if off < self.young_start || off >= self.young_end {
            return false;
        }
        let (from_lo, from_hi) = self.semi[self.semi_idx as usize];
        off < self.semi[0].0 || (off >= from_lo && off < from_hi)
    }

    #[inline]
    pub fn young_bytes(&self) -> u32 {
        self.young_bytes
    }

    /// 往 to-space bump（复制式回收的落点）
    #[inline]
    pub fn alloc_to(&mut self, size: u32) -> Option<u32> {
        let top = self.to_top.get();
        let end = self.semi[(1 - self.semi_idx) as usize].1;
        if top.checked_add(size)? <= end {
            self.to_top.set(top + size);
            Some(top)
        } else {
            None
        }
    }

    /// 往老年代尾部 bump（晋升 / LOS）
    #[inline]
    pub fn alloc_tail(&mut self, size: u32) -> Option<u32> {
        self.old.alloc(self.cap, size)
    }
}

/// 完整的托管堆。
pub struct Heap {
    region: Region,
    cfg: HeapConfig,
    layout: LayoutTable,

    // 年轻代
    young_start: u32,
    eden_start: u32,
    eden_end: u32,
    /// eden 软上限：分配越过它就触发 minor（自适应）
    eden_soft_end: Cell<u32>,
    eden_soft_min: u32,
    semi: [(u32, u32); 2],
    semi_idx: u8,
    semi_top: Cell<u32>,
    semi_used: u32,
    semi_objects: usize,
    eden_top: Cell<u32>,
    eden_objects: Cell<usize>,

    // 老年代 + LOS
    young_end: u32,
    old: OldSpace,

    // 写屏障与根
    cards: CardTable,
    starts: StartMap,
    roots: Rc<RefCell<Vec<u32>>>,
    satb: Vec<u32>,
    tlab: Tlab,

    // 统计
    stats: Stats,
    history: VecDeque<GcRecord>,
    last_survival: f64,
}

impl Heap {
    pub fn new(mut cfg: HeapConfig) -> Heap {
        cfg.initial = cfg.initial.clamp(cfg.min_capacity, cfg.max).max(RESERVED as usize * 1024);
        let cap = cfg.initial;
        let region = Region::new(cap);

        // 分区：eden / semi0 / semi1 在低地址固定，尾部给老年代 + LOS
        let usable = cap - RESERVED as usize;
        let mut eden_size = Object::align((usable as f64 * cfg.eden_ratio) as usize).max(4096);
        let semi_size = Object::align((usable as f64 * cfg.semi_ratio) as usize).max(4096);
        while eden_size > 4096 && RESERVED as usize + eden_size + 2 * semi_size + TAIL_MIN > cap {
            eden_size = eden_size / 2;
        }
        let eden_start = RESERVED;
        let eden_end = eden_start + eden_size as u32;
        let semi0 = (eden_end, eden_end + semi_size as u32);
        let semi1 = (semi0.1, semi0.1 + semi_size as u32);
        let young_end = semi1.1;
        assert!(
            (young_end as usize) + TAIL_MIN <= cap,
            "HeapConfig::initial={cap} 太小：eden({eden_size}) + 2×semi({semi_size}) 之后没有尾部空间"
        );

        Heap {
            cards: CardTable::new(young_end, cap),
            starts: StartMap::new(young_end, cap),
            region,
            cfg,
            layout: LayoutTable::new(),
            young_start: eden_start,
            eden_start,
            eden_end,
            eden_soft_end: Cell::new(eden_end),
            eden_soft_min: eden_start + eden_size as u32 / 4,
            semi: [semi0, semi1],
            semi_idx: 0,
            semi_top: Cell::new(0),
            semi_used: 0,
            semi_objects: 0,
            eden_top: Cell::new(eden_start),
            eden_objects: Cell::new(0),
            young_end,
            old: OldSpace::new(young_end),
            roots: Rc::new(RefCell::new(Vec::new())),
            satb: Vec::new(),
            tlab: Tlab::new(),
            stats: Stats::default(),
            history: VecDeque::new(),
            last_survival: 0.0,
        }
    }

    // ===================== 查询 =====================

    pub fn cfg(&self) -> &HeapConfig {
        &self.cfg
    }

    pub fn capacity(&self) -> usize {
        self.region.cap()
    }

    pub fn layout(&self) -> &LayoutTable {
        &self.layout
    }

    /// 堆内存基址。偏移是相对它的，`grow_capacity`（realloc）之后会变 ——
    /// 编译代码每次访存前都得重新读一次，不能缓存。
    #[inline]
    pub fn base(&self) -> *mut u8 {
        self.region.base()
    }

    pub fn spaces(&self) -> Spaces {
        Spaces {
            young_start: self.young_start,
            young_end: self.young_end,
            tail_start: self.young_end,
            cap: self.region.cap() as u32,
        }
    }

    #[inline]
    pub fn eden_used(&self) -> u32 {
        self.eden_top.get().saturating_sub(self.eden_start)
    }

    #[inline]
    pub fn eden_size(&self) -> u32 {
        self.eden_end - self.eden_start
    }

    #[inline]
    pub fn eden_limit(&self) -> u32 {
        self.eden_soft_end.get()
    }

    #[inline]
    pub fn semi_used(&self) -> u32 {
        self.semi_used
    }

    #[inline]
    pub fn young_used(&self) -> u32 {
        self.eden_used() + self.semi_used
    }

    #[inline]
    pub fn old_live_bytes(&self) -> usize {
        self.old.live_bytes()
    }

    /// 老年代存活字节 / 尾部容量
    pub fn old_ratio(&self) -> f64 {
        let cap = self.region.cap() as u32;
        let tail = cap.saturating_sub(self.young_end) as f64;
        if tail <= 0.0 {
            1.0
        } else {
            self.old.live_bytes() as f64 / tail
        }
    }

    #[inline]
    pub fn tlab_used(&self) -> u32 {
        self.tlab.used()
    }

    #[inline]
    pub fn tlab_size(&self) -> u32 {
        self.tlab.size()
    }

    // ===================== 类布局 =====================

    pub fn register_object(&mut self, name: &str, fields: &[SlotKind]) -> u32 {
        self.layout.register_object(name, fields)
    }

    pub fn register_array(&mut self, name: &str, elem_size: u32, ref_elem: bool) -> u32 {
        self.layout.register_array(name, elem_size, ref_elem)
    }

    // ===================== 根集 =====================

    pub fn roots_handle(&self) -> Rc<RefCell<Vec<u32>>> {
        self.roots.clone()
    }

    pub fn push_root(&self, h: Handle) -> Handle {
        self.roots.borrow_mut().push(h.0);
        h
    }

    pub fn pop_root(&self) -> Option<Handle> {
        self.roots.borrow_mut().pop().map(Handle)
    }

    pub fn roots_len(&self) -> usize {
        self.roots.borrow().len()
    }

    pub fn truncate_roots(&self, n: usize) {
        self.roots.borrow_mut().truncate(n);
    }

    /// 给外部批量遍历 / 批量改写根表（JIT 的影子栈）
    pub fn with_roots<R>(&self, f: impl FnOnce(&mut Vec<u32>) -> R) -> R {
        let mut g = self.roots.borrow_mut();
        f(&mut g)
    }

    /// RAII 根作用域
    pub fn root_scope(&self) -> RootScope {
        RootScope::new(self)
    }

    // ===================== 分配 =====================

    /// 按类布局分配一个对象。热路径：TLAB bump + 写头 + 清零字段。
    #[inline]
    pub fn alloc(&mut self, class_id: u32) -> Handle {
        let (size, flags) = {
            let cls = self.layout.class(class_id);
            let flags = object::with_field_count(array_flag(cls.is_array), cls.field_count);
            (cls.total_size(0), flags)
        };
        if size < self.cfg.los_threshold && self.eden_top.get() < self.eden_soft_end.get() {
            if let Some(off) = self.tlab.alloc(size) {
                self.eden_objects.set(self.eden_objects.get() + 1);
                unsafe { object::init(self.region.base(), off, class_id, flags, 0, size) };
                return Handle(off);
            }
        }
        self.alloc_raw(class_id, 0, size, flags)
    }

    /// 显式指定字段数的分配（`alloc(class_id, field_count)` 的形态）：
    /// 用于字段数由调用方决定、类布局不是最终形状的场景。
    /// 注意 GC 追引用仍然只看 `class_id` 的位图 —— 字段数只影响大小。
    pub fn alloc_sized(&mut self, class_id: u32, field_count: u32) -> Handle {
        let size = Object::size_for_fields(field_count as usize) as u32;
        let is_array = self.layout.class(class_id).is_array;
        let flags = object::with_field_count(array_flag(is_array), field_count);
        self.alloc_raw(class_id, 0, size, flags)
    }

    /// 分配数组：`len` 个元素，元素大小 / 是否引用取自类布局
    pub fn alloc_array(&mut self, class_id: u32, len: u32) -> Handle {
        let size = self.layout.class(class_id).total_size(len);
        self.alloc_raw(class_id, len, size, FLAG_ARRAY)
    }

    fn alloc_raw(&mut self, class_id: u32, length: u32, size: u32, flags: u32) -> Handle {
        debug_assert!(size as usize >= HEADER, "对象大小必须 >= 头部");
        // 大对象 / 装不进 eden 的对象直接进 LOS：避免复制式回收反复搬大数组
        if size >= self.cfg.los_threshold || size >= self.eden_size() {
            return self.alloc_los(class_id, length, size, flags);
        }
        if self.eden_top.get() >= self.eden_soft_end.get() {
            self.minor_heap();
        }
        if let Some(off) = self.bump(size) {
            self.eden_objects.set(self.eden_objects.get() + 1);
            self.init_at(off, class_id, length, size, flags);
            return Handle(off);
        }
        // 兜底：eden 里确实放不下
        self.alloc_los(class_id, length, size, flags)
    }

    /// TLAB / eden bump。返回**未初始化**的偏移。
    #[inline]
    fn bump(&mut self, size: u32) -> Option<u32> {
        if let Some(off) = self.tlab.alloc(size) {
            return Some(off);
        }
        let top = self.eden_top.get();
        let room = self.eden_end.saturating_sub(top);
        if room < size {
            return None;
        }
        let chunk = room.min(TLAB_MAX.max(size));
        self.tlab.reserve(top, chunk);
        self.eden_top.set(top + chunk);
        self.tlab.alloc(size)
    }

    /// 无锁 TLAB bump（`&self`）：JIT 快路径用。返回已初始化的 `class_id=0` 对象。
    ///
    /// `size` 必须 >= `HEADER` 且 8 字节对齐；空间不足返回 `None`，调用方
    /// 就 `refill_tlab()` 或走 `alloc`。
    pub fn tlab_alloc(&self, size: u32) -> Option<Handle> {
        assert!(
            size as usize >= HEADER,
            "tlab_alloc 的 size 必须 >= 对象头（{size} < {HEADER}）"
        );
        let off = self.tlab.alloc(size)?;
        // 裸分配没有类布局：字段数从大小反推，保证「对象多大」仍然自描述
        let fields = size.saturating_sub(HEADER as u32) / SLOT_BYTES as u32;
        let flags = object::with_field_count(0, fields);
        unsafe { object::init(self.region.base(), off, 0, flags, 0, size) };
        self.eden_objects.set(self.eden_objects.get() + 1);
        Some(Handle(off))
    }

    /// 同上，但按类布局算大小
    pub fn tlab_alloc_object(&self, class_id: u32) -> Option<Handle> {
        let (size, flags) = {
            let cls = self.layout.class(class_id);
            let flags = object::with_field_count(array_flag(cls.is_array), cls.field_count);
            (cls.total_size(0), flags)
        };
        let off = self.tlab.alloc(size)?;
        unsafe { object::init(self.region.base(), off, class_id, flags, 0, size) };
        self.eden_objects.set(self.eden_objects.get() + 1);
        Some(Handle(off))
    }

    /// 从 eden 切一块新的 TLAB。返回是否切到了。
    pub fn refill_tlab(&mut self) -> bool {
        let top = self.eden_top.get();
        let room = self.eden_end.saturating_sub(top);
        if room < TLAB_MIN / 8 {
            self.tlab.reset();
            return false;
        }
        let chunk = room.min(TLAB_MAX);
        self.tlab.reserve(top, chunk);
        self.eden_top.set(top + chunk);
        true
    }

    fn alloc_los(&mut self, class_id: u32, length: u32, size: u32, flags: u32) -> Handle {
        let flags = flags | FLAG_LOS;
        let cap = self.region.cap() as u32;
        if let Some(off) = self.old.alloc_los(cap, size) {
            self.init_at(off, class_id, length, size, flags);
            self.starts.set(off);
            return Handle(off);
        }
        // 尾部满了：先真正回收一轮
        self.major_heap();
        let cap = self.region.cap() as u32;
        if let Some(off) = self.old.alloc_los(cap, size) {
            self.init_at(off, class_id, length, size, flags);
            self.starts.set(off);
            return Handle(off);
        }
        if self.grow_capacity(size as usize + TAIL_MIN) {
            let cap = self.region.cap() as u32;
            if let Some(off) = self.old.alloc_los(cap, size) {
                self.init_at(off, class_id, length, size, flags);
                self.starts.set(off);
                return Handle(off);
            }
        }
        panic!("堆耗尽（max={} 字节）：无法分配 {size} 字节的 LOS 对象", self.cfg.max);
    }

    #[inline]
    fn init_at(&self, off: u32, class_id: u32, length: u32, size: u32, flags: u32) {
        unsafe { object::init(self.region.base(), off, class_id, flags, length, size) };
    }

    /// handle -> 对象头指针
    #[inline]
    pub fn get(&self, h: Handle) -> *mut Object {
        assert!(!h.is_null(), "handle 是 null");
        unsafe { object::hdr(self.region.base(), h.0) }
    }

    #[inline]
    pub fn class_of(&self, h: Handle) -> u32 {
        unsafe { object::class_of(self.region.base(), h.0) }
    }

    #[inline]
    pub fn size_of(&self, h: Handle) -> u32 {
        unsafe { object::size_at(self.region.base(), h.0, &self.layout) }
    }

    #[inline]
    pub fn is_array(&self, h: Handle) -> bool {
        unsafe { object::flags_of(self.region.base(), h.0) & FLAG_ARRAY != 0 }
    }

    // ===================== 字段访问（写屏障接线） =====================

    /// 第 `idx` 个槽 / 元素的地址。
    ///
    /// 普通对象按 4 字节槽寻址；数组按元素大小寻址（元素从 `ARRAY_HEADER` 开始）。
    /// 判断只看对象头的 `FLAG_ARRAY` 位 —— 对象头就在要访问的那行 cache line 上，
    /// 这个分支几乎不花钱。
    #[inline]
    fn slot_ptr(&self, off: u32, idx: u32) -> *mut u32 {
        unsafe {
            let base = self.region.base();
            if object::flags_of(base, off) & FLAG_ARRAY != 0 {
                debug_assert_eq!(
                    self.layout.class(object::class_of(base, off)).elem_size,
                    SLOT_BYTES as u32,
                    "只有 4 字节元素才有「槽」语义"
                );
                object::elem_slot(base, off, idx)
            } else {
                object::field_slot(base, off, idx)
            }
        }
    }

    /// 写引用槽：过写屏障。老对象存年轻引用会把老对象所在卡标脏。
    pub fn set_ref(&mut self, obj: Handle, idx: u32, val: Handle) {
        let spaces = self.spaces();
        let kind = self.cfg.barrier;
        unsafe {
            let sp = self.slot_ptr(obj.0, idx);
            let old = *sp;
            *sp = val.0;
            barrier::on_store(
                kind,
                &spaces,
                &mut self.cards,
                &mut self.satb,
                obj.0,
                old,
                val.0,
            );
        }
    }

    /// 写引用槽但**不过**屏障（唯一用途：证明屏障确实在起作用的反向实验）
    pub fn set_ref_raw(&mut self, obj: Handle, idx: u32, val: Handle) {
        unsafe {
            *self.slot_ptr(obj.0, idx) = val.0;
        }
    }

    #[inline]
    pub fn get_ref(&self, obj: Handle, idx: u32) -> Handle {
        unsafe { Handle(*self.slot_ptr(obj.0, idx)) }
    }

    #[inline]
    pub fn set_i32(&mut self, obj: Handle, idx: u32, v: i32) {
        unsafe { *self.slot_ptr(obj.0, idx) = v as u32 };
    }

    #[inline]
    pub fn get_i32(&self, obj: Handle, idx: u32) -> i32 {
        unsafe { *self.slot_ptr(obj.0, idx) as i32 }
    }

    /// 64 位值占**两个槽**（槽宽 4 字节）
    pub fn set_i64(&mut self, obj: Handle, idx: u32, v: i64) {
        unsafe {
            let p = self.slot_ptr(obj.0, idx) as *mut u64;
            p.write_unaligned(v as u64);
        }
    }

    pub fn get_i64(&self, obj: Handle, idx: u32) -> i64 {
        unsafe {
            let p = self.slot_ptr(obj.0, idx) as *const u64;
            p.read_unaligned() as i64
        }
    }

    #[inline]
    pub fn read_slot(&self, obj: Handle, idx: u32) -> u32 {
        unsafe { *self.slot_ptr(obj.0, idx) }
    }

    #[inline]
    pub fn write_slot(&mut self, obj: Handle, idx: u32, v: u32) {
        unsafe { *self.slot_ptr(obj.0, idx) = v };
    }

    #[inline]
    pub fn slot_addr(&self, obj: Handle, idx: u32) -> *mut u32 {
        self.slot_ptr(obj.0, idx)
    }

    // ===================== 回收 =====================

    /// 年轻代回收（复制 eden + survivor，超龄晋升）
    pub fn minor(&mut self) -> MinorReport {
        self.minor_heap()
    }

    /// 老年代标记-清扫
    pub fn major(&mut self) -> MajorReport {
        self.major_heap()
    }

    /// 全量：minor + major
    pub fn full(&mut self) -> MajorReport {
        self.minor_heap();
        let m = self.major_heap();
        self.maybe_shrink();
        m
    }

    /// 按策略自动决定：老年代吃紧就跑 full，否则跑 minor
    pub fn collect(&mut self) {
        if self.old_ratio() >= self.cfg.major_ratio {
            let _ = self.full();
        } else {
            let _ = self.minor_heap();
        }
    }

    /// 老年代压缩（会移动对象 → 外部 handle 必须从根表重新读）
    pub fn compact(&mut self) -> MajorReport {
        self.major_heap();
        let young_objects = self.eden_objects.get() + self.semi_objects;
        let cap = self.region.cap() as u32;
        let roots_cell = self.roots.clone();
        let mut roots = roots_cell.borrow_mut();
        let rep = {
            let mut env = self.make_env(cap, &mut roots, young_objects, 0);
            old::compact(&mut env)
        };
        drop(roots);
        self.record(GcKind::Full, rep.pause_ns, 0, 0, 0, 0, 0);
        rep
    }

    fn minor_heap(&mut self) -> MinorReport {
        self.prepare_minor();
        let young_objects = self.eden_objects.get() + self.semi_objects;
        let young_bytes = self.young_used();
        let old_before = self.old.live_bytes() as u32;
        let cap = self.region.cap() as u32;
        let roots_cell = self.roots.clone();
        let mut roots = roots_cell.borrow_mut();
        let rep = {
            let mut env = self.make_env(cap, &mut roots, young_objects, young_bytes);
            young::minor(&mut env)
        };
        drop(roots);

        // 年轻代复位
        self.eden_top.set(self.eden_start);
        self.eden_objects.set(0);
        self.tlab.reset();
        self.semi_idx = 1 - self.semi_idx;
        let from = self.semi[self.semi_idx as usize].0;
        self.semi_used = self.semi_top.get().saturating_sub(from);
        self.semi_objects = rep.survived;

        // 自适应 eden 上限
        self.adapt_eden(young_objects, rep.survived + rep.promoted);

        let before = young_bytes as u64;
        let after = self.semi_used as u64;
        self.stats.minor_count += 1;
        self.stats.last_pause_ns = rep.pause_ns;
        self.stats.max_pause_ns = self.stats.max_pause_ns.max(rep.pause_ns);
        self.stats.total_pause_ns += rep.pause_ns;
        self.stats.max_step_ns = self.stats.max_step_ns.max(rep.max_step_ns);
        self.stats.promoted_objects += rep.promoted as u64;
        self.stats.promoted_bytes += rep.promoted_bytes as u64;
        self.stats.copied_bytes += rep.survived_bytes as u64;
        self.stats.collected_objects += rep.collected as u64;
        self.stats.collected_bytes += before.saturating_sub(after);
        self.record(
            GcKind::Minor,
            rep.pause_ns,
            rep.collected as u64,
            rep.promoted as u64,
            before as u32,
            old_before,
            self.old.live_bytes() as u32,
        );
        rep
    }

    fn major_heap(&mut self) -> MajorReport {
        let young_objects = self.eden_objects.get() + self.semi_objects;
        let cap = self.region.cap() as u32;
        let old_before = self.old.live_bytes() as u32;
        let roots_cell = self.roots.clone();
        let mut roots = roots_cell.borrow_mut();
        let rep = {
            let mut env = self.make_env(cap, &mut roots, young_objects, self.young_used());
            old::major(&mut env)
        };
        drop(roots);

        self.stats.major_count += 1;
        self.stats.last_pause_ns = rep.pause_ns;
        self.stats.max_pause_ns = self.stats.max_pause_ns.max(rep.pause_ns);
        self.stats.total_pause_ns += rep.pause_ns;
        self.stats.collected_objects += rep.freed as u64 + rep.los_freed as u64;
        self.stats.collected_bytes += rep.freed_bytes as u64;
        self.record(
            GcKind::Major,
            rep.pause_ns,
            (rep.freed + rep.los_freed) as u64,
            0,
            self.young_used(),
            old_before,
            self.old.live_bytes() as u32,
        );
        rep
    }

    fn make_env<'a>(
        &'a mut self,
        cap: u32,
        roots: &'a mut Vec<u32>,
        young_objects: usize,
        young_bytes: u32,
    ) -> GcEnv<'a> {
        GcEnv {
            base: self.region.base(),
            cap,
            layout: &self.layout,
            young_start: self.young_start,
            young_end: self.young_end,
            semi: self.semi,
            semi_idx: self.semi_idx,
            to_top: &self.semi_top,
            old: &mut self.old,
            cards: &mut self.cards,
            starts: &mut self.starts,
            roots,
            satb: &mut self.satb,
            tenuring_threshold: self.cfg.tenuring_threshold.max(1),
            work: Vec::new(),
            scanned: Vec::new(),
            young_objects,
            young_bytes,
            copied: 0,
            promoted: 0,
            promoted_bytes: 0,
            survived: 0,
            survived_bytes: 0,
            max_step_ns: 0,
            marked: 0,
            failures: 0,
        }
    }

    /// minor 之前预检：最坏情况「所有年轻对象都晋升」，尾部必须有地方放。
    fn prepare_minor(&mut self) {
        let need = self.young_used() as usize;
        if need == 0 {
            return;
        }
        let cap = self.region.cap() as u32;
        let free_tail = cap.saturating_sub(self.old.top()) as usize;
        if free_tail >= need {
            return;
        }
        // 先真的回收一轮老年代
        self.major_heap();
        let cap = self.region.cap() as u32;
        let free_tail = cap.saturating_sub(self.old.top()) as usize;
        if free_tail < need {
            self.grow_capacity(need + TAIL_MIN);
        }
    }

    /// 按上次存活率调 eden 的软上限：存活率高就早点回收（缩小 eden）
    fn adapt_eden(&mut self, before: usize, survivors: usize) {
        let rate = if before == 0 {
            0.0
        } else {
            survivors as f64 / before as f64
        };
        self.last_survival = rate;
        let span = (self.eden_end - self.eden_start) as f64;
        let target = if rate > 0.5 {
            span * 0.5
        } else if rate > 0.2 {
            span * 0.75
        } else {
            span
        };
        let target = (target as u32).max(self.eden_soft_min);
        self.eden_soft_end.set(self.eden_start + target);
    }

    // ===================== 增长 / 收缩 =====================

    pub fn grow_capacity(&mut self, min_need: usize) -> bool {
        let cap = self.region.cap();
        if cap >= self.cfg.max {
            return false;
        }
        let mut next = cap.saturating_mul(2).max(cap + min_need);
        next = next.min(self.cfg.max);
        if next <= cap {
            return false;
        }
        next = Object::align(next);
        self.region.grow(next);
        self.cards.resize(next);
        self.starts.resize(next);
        self.stats.growth_events += 1;
        true
    }

    /// 收缩到「实际用到的字节 + 余量」。只截断 `old.top()` 之上的空间，
    /// 所以不会移动任何对象（偏移全部保持有效）。
    pub fn shrink_to_fit(&mut self) {
        let cap = self.region.cap();
        let floor = (self.young_end as usize) + TAIL_MIN;
        let want = Object::align(self.old.top() as usize)
            .max(floor)
            .max(self.cfg.min_capacity);
        if want + TAIL_MIN < cap {
            self.region.shrink(want);
            let new_cap = self.region.cap();
            self.cards.resize(new_cap);
            self.starts.resize(new_cap);
            self.stats.shrink_events += 1;
        }
    }

    fn maybe_shrink(&mut self) {
        let cap = self.region.cap();
        if cap <= self.cfg.min_capacity {
            return;
        }
        let used = self.old.top() as f64;
        let free_ratio = 1.0 - used / cap as f64;
        if free_ratio > self.cfg.min_free_ratio {
            self.shrink_to_fit();
        }
    }

    // ===================== 统计 =====================

    pub fn stats(&self) -> Stats {
        let mut s = self.stats;
        s.eden_used = self.eden_used();
        s.young_used = self.young_used();
        s.old_used = self.old.live_bytes() as u32;
        s.old_live = self.old.live.len();
        s.los_live = self.old.los_live.len();
        s.old_free_bytes = self.old.free.bytes();
        s.los_free_bytes = self.old.los_free.bytes();
        s.capacity = self.region.cap();
        s.eden_limit = self.eden_soft_end.get();
        s.tenuring_threshold = self.cfg.tenuring_threshold;
        s.card_dirty = self.cards.dirty_len();
        s.satb_len = self.satb.len();
        s
    }

    pub fn history(&self) -> impl Iterator<Item = &GcRecord> {
        self.history.iter()
    }

    /// 上次 minor 的存活率（诊断用）
    pub fn last_survival(&self) -> f64 {
        self.last_survival
    }

    /// 老年代存活对象数 / LOS 对象数
    pub fn old_live_count(&self) -> usize {
        self.old.live.len()
    }

    pub fn los_live_count(&self) -> usize {
        self.old.los_live.len()
    }

    /// 老对象偏移表（诊断 / 测试用）
    pub fn old_objects(&self) -> &[u32] {
        &self.old.live
    }

    pub fn los_objects(&self) -> &[u32] {
        &self.old.los_live
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        kind: GcKind,
        pause_ns: u128,
        collected: u64,
        promoted: u64,
        young_before: u32,
        old_before: u32,
        old_after: u32,
    ) {
        if self.cfg.history == 0 {
            return;
        }
        if self.history.len() >= self.cfg.history {
            self.history.pop_front();
        }
        self.history.push_back(GcRecord {
            kind,
            pause_ns,
            collected,
            promoted,
            young_before,
            old_before,
            old_after,
        });
    }
}

#[inline]
fn array_flag(is_array: bool) -> u32 {
    if is_array {
        FLAG_ARRAY
    } else {
        0
    }
}

// ===================== JIT 直调入口 =====================
//
// 这些函数就是给编译代码用的：只有裸指针和整数，没有 Rust 类型。

/// `jrt_alloc(heap, class_id, field_count)` —— field_count 为 0 时按类布局取大小
#[no_mangle]
pub unsafe extern "C" fn jrt_alloc(heap: *mut Heap, class_id: u32, field_count: u32) -> u32 {
    let h = &mut *heap;
    if field_count == 0 {
        h.alloc(class_id).raw()
    } else {
        h.alloc_sized(class_id, field_count).raw()
    }
}

/// `jrt_alloc_array(heap, class_id, len)`
#[no_mangle]
pub unsafe extern "C" fn jrt_alloc_array(heap: *mut Heap, class_id: u32, len: u32) -> u32 {
    (&mut *heap).alloc_array(class_id, len).raw()
}

/// `jrt_set_ref(heap, obj, idx, val)` —— 带写屏障
#[no_mangle]
pub unsafe extern "C" fn jrt_set_ref(heap: *mut Heap, obj: u32, idx: u32, val: u32) {
    (&mut *heap).set_ref(Handle(obj), idx, Handle(val))
}

/// `jrt_get_ref(heap, obj, idx)`
#[no_mangle]
pub unsafe extern "C" fn jrt_get_ref(heap: *const Heap, obj: u32, idx: u32) -> u32 {
    (&*heap).get_ref(Handle(obj), idx).raw()
}

/// `jrt_root(heap, handle)` —— 压进影子栈
#[no_mangle]
pub unsafe extern "C" fn jrt_root(heap: *const Heap, h: u32) -> u32 {
    (&*heap).push_root(Handle(h)).raw()
}

/// `jrt_collect(heap)` —— 按策略回收一轮
#[no_mangle]
pub unsafe extern "C" fn jrt_collect(heap: *mut Heap) {
    (&mut *heap).collect()
}

/// `jrt_alloc_size`：对象总字节数查询
#[no_mangle]
pub unsafe extern "C" fn jrt_size_of(heap: *const Heap, obj: u32) -> u32 {
    (&*heap).size_of(Handle(obj))
}

/// 清理编译告警用的常量引用（`FLAG_MARK` 只在 GC 内部用）
const _: () = assert!(FLAG_MARK == 2);
