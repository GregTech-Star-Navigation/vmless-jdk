//! 兼容层：0.1 的旧 API，**冻结**，只为老基准与外部调用方保留。
//!
//! 这里的实现是按老语义写的（8 字节引用槽、单一 `ClassMeta` 布局），
//! 新架构的多类 / 根集 / 写屏障 / 分代 / LOS 全部在 `heap` / `layout` / `roots`
//! / `cards` / `young` / `old` 里。老实现只共享最底层的 `alloc::Region`。
//!
//! 保留的名字：`JavaObject`(+`size_for`)、`HEADER`、`BumpArena`、`MarkSweep`、
//! `FieldKind`、`ClassMeta`、`Heap`、`CompactObject`、`COMPACT_HEADER`、`CompactMeta`、
//! `CompactHeap`、`IncrementalState`、`GcPhase`，以及 `gen::GenerationalHeap` / `gen::MinorStats`。

use std::sync::atomic::{AtomicUsize, Ordering};

pub use crate::alloc::Region;

/// Java 对象头在 JVM 里是 mark word + klass pointer（16 字节）。
/// 我们保留 class_id 与标志位，但不保留 GC 元数据——回收靠 Rust 侧。
#[repr(C)]
pub struct JavaObject {
    pub class_id: u32,
    pub flags: u32,
    /// 字段槽：每个槽 8 字节，可存 int/long/引用。
    pub field_len: u32,
    _pad: u32,
}

pub const HEADER: usize = std::mem::size_of::<JavaObject>(); // 16

impl JavaObject {
    /// 对象总字节数：头 + 字段槽
    pub fn size_for(field_len: u32) -> usize {
        HEADER + (field_len as usize) * 8
    }
}

/// bump 分配器：一次分配大块，指针单调递增；整块一次释放。
/// 对应"编译器进程"这种短生命周期场景——不需要逐个回收。
pub struct BumpArena {
    /// 底层内存交给新架构的 `alloc::Region`（连续区，可 realloc）；
    /// bump 指针仍是原子量，保持老的 `Send + Sync` 语义。
    region: Region,
    used: AtomicUsize,
}

unsafe impl Send for BumpArena {}
unsafe impl Sync for BumpArena {}

impl BumpArena {
    pub fn new(cap: usize) -> Self {
        BumpArena { region: Region::new(cap), used: AtomicUsize::new(0) }
    }

    #[inline]
    fn base(&self) -> *mut u8 {
        self.region.base()
    }

    #[inline]
    fn cap(&self) -> usize {
        self.region.cap()
    }

    /// 分配 size 字节，16 字节对齐；返回偏移（0 表示失败）
    pub fn alloc(&self, size: usize) -> Option<usize> {
        let aligned = (size + 15) & !15;
        let start = self.used.fetch_add(aligned, Ordering::Relaxed);
        if start + aligned > self.cap() {
            self.used.fetch_sub(aligned, Ordering::Relaxed);
            return None;
        }
        Some(start)
    }

    pub fn alloc_object(&self, field_len: u32) -> Option<*mut JavaObject> {
        let size = JavaObject::size_for(field_len);
        let off = self.alloc(size)?;
        let p = unsafe { self.base().add(off) } as *mut JavaObject;
        unsafe {
            (*p).class_id = 0;
            (*p).flags = 0;
            (*p).field_len = field_len;
            (*p)._pad = 0;
        }
        Some(p)
    }

    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    pub fn capacity(&self) -> usize {
        self.cap()
    }
}

/// 标记清扫骨架：真实实现要接类元数据里的引用位图。
pub struct MarkSweep {
    pub marked: usize,
    pub swept: usize,
}

impl MarkSweep {
    pub fn new() -> Self {
        MarkSweep { marked: 0, swept: 0 }
    }
    pub fn collect(&mut self, live: impl Iterator<Item = *mut JavaObject>) {
        for _ in live {
            self.marked += 1;
        }
        self.swept += 1;
    }
}

impl Default for MarkSweep {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_16_bytes() {
        assert_eq!(HEADER, 16);
        assert_eq!(JavaObject::size_for(4), 16 + 32);
    }

    #[test]
    fn arena_allocates_and_counts() {
        let a = BumpArena::new(1 << 20);
        let o = a.alloc_object(4).expect("alloc");
        unsafe { (*o).class_id = 42 };
        assert_eq!(a.used(), 48); // 16 头 + 32 字段，按 16 对齐
        unsafe { assert_eq!((*o).class_id, 42) };
    }

    #[test]
    fn arena_rejects_oversize() {
        let a = BumpArena::new(64);
        assert!(a.alloc(1024).is_none());
    }
}

// ===================== 类元数据 + 标记清扫 =====================

/// 字段类别：决定这个槽是否可能是对象引用（清扫时要追踪）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FieldKind {
    Prim,
    Ref,
}

/// 类元数据：名字、字段布局、对象大小。
#[derive(Debug)]
pub struct ClassMeta {
    pub name: String,
    pub fields: Vec<FieldKind>,
}

impl ClassMeta {
    pub fn field_len(&self) -> u32 {
        self.fields.len() as u32
    }
    pub fn size(&self) -> usize {
        JavaObject::size_for(self.field_len())
    }
    pub fn is_ref(&self, idx: usize) -> bool {
        matches!(self.fields.get(idx), Some(FieldKind::Ref))
    }
}

/// 一个带真实回收的堆：按大小分级 + 标记清扫。
///
/// - 分配：从对应 size class 的空闲链表取，没有就向 bump 区借。
/// - 回收：从 roots 出发按字段位图标记，未标记的对象进入空闲链表。
pub struct Heap {
    arena: BumpArena,
    /// 已分配对象（供清扫遍历）
    live: Vec<*mut JavaObject>,
    /// 空闲链表：size -> 可复用地址
    free: Vec<(*mut JavaObject, usize)>, // (ptr, size)
    pub collected: usize,
}

impl Heap {
    pub fn new(cap: usize) -> Self {
        Heap { arena: BumpArena::new(cap), live: Vec::new(), free: Vec::new(), collected: 0 }
    }

    pub fn alloc(&mut self, meta: &ClassMeta) -> *mut JavaObject {
        let size = meta.size();
        if let Some(pos) = self.free.iter().position(|&(_, s)| s == size) {
            let (p, _) = self.free.swap_remove(pos);
            self.init(p, meta);
            self.live.push(p);
            return p;
        }
        let p = self.arena.alloc_object(meta.field_len()).expect("heap exhausted");
        self.init(p, meta);
        self.live.push(p);
        p
    }

    fn init(&self, p: *mut JavaObject, meta: &ClassMeta) {
        unsafe {
            (*p).flags = 0;
            (*p).field_len = meta.field_len();
            let fields = (p as *mut u8).add(HEADER) as *mut u64;
            for i in 0..meta.field_len() as usize {
                *fields.add(i) = 0;
            }
        }
    }

    pub fn set_ref(&self, obj: *mut JavaObject, idx: usize, val: *mut JavaObject) {
        unsafe {
            let fields = (obj as *mut u8).add(HEADER) as *mut u64;
            *fields.add(idx) = val as u64;
        }
    }

    pub fn get_ref(&self, obj: *mut JavaObject, idx: usize) -> *mut JavaObject {
        unsafe {
            let fields = (obj as *mut u8).add(HEADER) as *mut u64;
            *fields.add(idx) as *mut JavaObject
        }
    }

    /// 标记清扫：metas[i] 对应 live[i] 的类。
    pub fn collect(&mut self, roots: &[*mut JavaObject], metas: &[&ClassMeta]) {
        const MARK: u32 = 1;
        // 标记：用对象头 flags 的最低位，避免 O(n^2) 的 Vec::contains
        let mut work: Vec<*mut JavaObject> = roots.to_vec();
        while let Some(o) = work.pop() {
            if o.is_null() {
                continue;
            }
            unsafe {
                if (*o).flags & MARK != 0 {
                    continue;
                }
                (*o).flags |= MARK;
            }
            let meta = metas.iter().find(|m| m.size() == obj_size(o)).copied();
            if let Some(meta) = meta {
                for i in 0..meta.field_len() as usize {
                    if meta.is_ref(i) {
                        let r = self.get_ref(o, i);
                        if !r.is_null() {
                            work.push(r);
                        }
                    }
                }
            }
        }
        // 清扫
        let mut kept = Vec::with_capacity(self.live.len());
        for &o in &self.live {
            let marked = unsafe { (*o).flags & MARK != 0 };
            if marked {
                unsafe { (*o).flags &= !MARK };
                kept.push(o);
            } else {
                self.free.push((o, obj_size(o)));
                self.collected += 1;
            }
        }
        self.live = kept;
    }

    pub fn live_count(&self) -> usize {
        self.live.len()
    }

    /// 已向 arena 借出的字节数（用于验证空闲槽复用）
    pub fn arena_used(&self) -> usize {
        self.arena.used()
    }
}

fn obj_size(o: *mut JavaObject) -> usize {
    unsafe { JavaObject::size_for((*o).field_len) }
}

#[cfg(test)]
mod gc_tests {
    use super::*;

    fn node_meta() -> ClassMeta { ClassMeta { name: "Node".into(), fields: vec![FieldKind::Ref, FieldKind::Prim] } }

    #[test]
    fn mark_sweep_reclaims_unreachable() {
        let mut h = Heap::new(1 << 20);
        let m = node_meta();
        let root = h.alloc(&m);
        let a = h.alloc(&m);
        let b = h.alloc(&m);
        h.set_ref(root, 0, a);
        h.set_ref(a, 0, b);
        assert_eq!(h.live_count(), 3);

        // a -> b 这条链可达，全部保留
        h.collect(&[root], &[&m]);
        assert_eq!(h.collected, 0);
        assert_eq!(h.live_count(), 3);

        // 断掉 root->a，a 与 b 都不可达
        h.set_ref(root, 0, std::ptr::null_mut());
        h.collect(&[root], &[&m]);
        assert_eq!(h.collected, 2);
        assert_eq!(h.live_count(), 1);
    }

    #[test]
    fn freed_slots_are_reused() {
        let mut h = Heap::new(1 << 20);
        let m = node_meta();
        let root = h.alloc(&m);
        let tmp = h.alloc(&m);
        let _ = tmp;
        h.collect(&[root], &[&m]);
        assert_eq!(h.collected, 1);
        let used_before = h.arena.used();
        let reused = h.alloc(&m);
        assert_eq!(h.arena.used(), used_before, "应当复用空闲槽，不再向 arena 借内存");
        h.set_ref(root, 0, reused);
    }
}

// ===================== 紧凑布局（对标 HotSpot 压缩指针） =====================

/// 紧凑对象头：8 字节（class_id + 标志位），比 HotSpot 的 12~16 字节更小。
#[repr(C)]
pub struct CompactObject {
    pub class_id: u32,
    pub flags: u32,
    // 后面紧跟 field_len 个 4 字节槽（i32 或压缩引用）
}

pub const COMPACT_HEADER: usize = std::mem::size_of::<CompactObject>(); // 8

/// 紧凑堆：头 8 字节 + 每槽 4 字节，按 8 字节对齐分配。
pub struct CompactMeta {
    pub name: &'static str,
    pub fields: Vec<FieldKind>,
}

impl CompactMeta {
    pub fn size(&self) -> usize {
        // 4 字节对齐：1 个字段的对象只要 12 字节（8 头 + 4）
        let raw = COMPACT_HEADER + self.fields.len() * 4;
        (raw + 3) & !3
    }
    pub fn field_len(&self) -> u32 {
        self.fields.len() as u32
    }
    pub fn is_ref(&self, i: usize) -> bool {
        matches!(self.fields.get(i), Some(FieldKind::Ref))
    }
}

pub struct CompactHeap {
    base: *mut u8,
    cap: usize,
    used: usize,
    /// 存活对象偏移
    live: Vec<usize>,
    /// 按尺寸分桶的空闲表（线性扫描几个尺寸，比 HashMap 快得多）
    free: Vec<(usize, Vec<usize>)>,
    /// 新建空闲桶时的预留容量（= 本轮 live 数），避免清扫中扩容尖峰
    free_hint: usize,
    /// 尺寸类别表：类别号 → 字节数（类别号存进对象头 flags 的高位）
    size_classes: Vec<usize>,
    /// 尺寸 → 类别号 的 O(1) 查找表，下标 = size / 4
    size_class_of: Vec<u32>,
    pub collected: usize,
    #[allow(dead_code)] // 兼容层冻结：老实现留下的字段，保留结构不做删改
    metas: Vec<usize>, // 对象偏移 -> meta 下标（简化：同尺寸复用）
}

unsafe impl Send for CompactHeap {}

impl CompactHeap {
    pub fn new(cap: usize) -> Self {
        let layout = std::alloc::Layout::from_size_align(cap, 8).expect("layout");
        let base = unsafe { std::alloc::alloc(layout) };
        assert!(!base.is_null(), "compact heap alloc failed");
        CompactHeap {
            base,
            cap,
            used: 0,
            live: Vec::new(),
            free: Vec::new(),
            free_hint: 0,
            size_classes: Vec::new(),
            size_class_of: Vec::new(),
            collected: 0,
            metas: Vec::new(),
        }
    }

    pub fn alloc(&mut self, meta: &CompactMeta) -> *mut CompactObject {
        let size = meta.size();
        let off = if let Some(b) = self.free.iter_mut().find(|(s, v)| *s == size && !v.is_empty()) {
            b.1.pop().unwrap()
        } else {
            let off = self.used;
            self.used += size;
            assert!(self.used <= self.cap, "compact heap full");
            off
        };
        // 找到或登记尺寸类别（O(1)：按 size/4 索引）
        let slot = size / 4;
        if slot >= self.size_class_of.len() {
            self.size_class_of.resize(slot + 8, u32::MAX);
        }
        let class_idx = if self.size_class_of[slot] == u32::MAX {
            let idx = self.size_classes.len() as u32;
            self.size_classes.push(size);
            self.size_class_of[slot] = idx;
            idx
        } else {
            self.size_class_of[slot]
        };
        let p = unsafe { self.base.add(off) } as *mut CompactObject;
        unsafe {
            (*p).class_id = 0;
            // 低 1 位是标记位，1..=8 位是尺寸类别号
            (*p).flags = class_idx << 1;
        }
        // 清零字段槽
        unsafe {
            let f = (p as *mut u8).add(COMPACT_HEADER) as *mut u32;
            for i in 0..meta.field_len() as usize {
                *f.add(i) = 0;
            }
        }
        self.live.push(off);
        p
    }

    #[allow(dead_code)]
    fn size_at(&self, off: usize) -> usize {
        // 从对象头的 class_id 取不到大小，这里用 live 记录的简化方式：
        // 实际实现应存 size class；基准里所有对象同尺寸，直接按 16 处理。
        let _ = off;
        16
    }

    pub fn set_slot(&mut self, o: *mut CompactObject, idx: usize, v: u32) {
        unsafe {
            let f = (o as *mut u8).add(COMPACT_HEADER) as *mut u32;
            *f.add(idx) = v;
        }
    }

    pub fn get_slot(&self, o: *mut CompactObject, idx: usize) -> u32 {
        unsafe {
            let f = (o as *const u8).add(COMPACT_HEADER) as *const u32;
            *f.add(idx)
        }
    }

    pub fn used_bytes(&self) -> usize {
        self.used
    }

    /// 对象 → 相对 base 的偏移（存进引用槽）
    pub fn offset_of(&self, o: *mut CompactObject) -> u32 {
        (o as usize - self.base as usize) as u32
    }

    pub fn live_count(&self) -> usize {
        self.live.len()
    }
}

impl CompactHeap {
    /// 紧凑堆的标记清扫：位图存在对象头 flags 的最低位。
    pub fn collect(&mut self, roots: &[*mut CompactObject], meta: &CompactMeta) {
        const MARK: u32 = 1;
        let mut work: Vec<*mut CompactObject> = roots.to_vec();
        while let Some(o) = work.pop() {
            if o.is_null() {
                continue;
            }
            unsafe {
                if (*o).flags & MARK != 0 {
                    continue;
                }
                (*o).flags |= MARK;
            }
            for i in 0..meta.field_len() as usize {
                if meta.is_ref(i) {
                    let v = self.get_slot(o, i);
                    if v != 0 {
                        // 槽里存的是相对 base 的偏移，不是绝对指针
                        work.push(unsafe { self.base.add(v as usize) } as *mut CompactObject);
                    }
                }
            }
        }
        let mut kept = Vec::with_capacity(self.live.len());
        for &off in &self.live {
            let p = unsafe { self.base.add(off) } as *mut CompactObject;
            let marked = unsafe { (*p).flags & MARK != 0 };
            if marked {
                unsafe { (*p).flags &= !MARK };
                kept.push(off);
            } else {
                let cls = (unsafe { (*p).flags } >> 1) as usize;
                let sz = *self.size_classes.get(cls).unwrap_or(&meta.size());
                push_free(&mut self.free, sz, off, self.free_hint);
                self.collected += 1;
            }
        }
        self.live = kept;
    }
}

// ===================== 增量（切片）标记清扫 =====================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GcPhase {
    Idle,
    Mark,
    Sweep,
}

/// 增量 GC：把标记与清扫切成小片，每片只处理 budget 个对象。
/// 这样最大停顿可控，不像一次性全量回收那样卡住几十毫秒。
pub struct IncrementalState {
    pub phase: GcPhase,
    stack: Vec<u32>,
    sweep_cursor: usize,
    write_cursor: usize,
    pub steps: usize,
    pub max_step_ns: u128,
}

impl Default for IncrementalState {
    fn default() -> Self {
        IncrementalState {
            phase: GcPhase::Idle,
            stack: Vec::new(),
            sweep_cursor: 0,
            write_cursor: 0,
            steps: 0,
            max_step_ns: 0,
        }
    }
}

impl CompactHeap {
    /// 开始一轮回收；roots 是相对 base 的偏移。
    pub fn begin_cycle(&mut self, state: &mut IncrementalState, roots: &[u32]) {
        const MARK: u32 = 1;
        state.stack.clear();
        state.stack.extend_from_slice(roots);
        state.sweep_cursor = 0;
        state.write_cursor = 0;
        state.phase = GcPhase::Mark;
        state.steps = 0;
        state.max_step_ns = 0;
        // 预分配空闲表，避免清扫过程中 Vec 扩容带来的毫秒级尖峰
        self.free_hint = self.live.len();
        for b in &mut self.free {
            b.1.reserve(self.free_hint);
        }
        let _ = MARK;
    }

    /// 推进一步：处理至多 budget 个对象。返回 true 表示本轮还没结束。
    pub fn step(&mut self, state: &mut IncrementalState, meta: &CompactMeta, budget: usize) -> bool {
        const MARK: u32 = 1;
        let t = std::time::Instant::now();
        match state.phase {
            GcPhase::Mark => {
                let mut done = 0;
                while done < budget {
                    let Some(off) = state.stack.pop() else { break };
                    if off == 0 {
                        continue;
                    }
                    let p = unsafe { self.base.add(off as usize) } as *mut CompactObject;
                    unsafe {
                        if (*p).flags & MARK != 0 {
                            continue;
                        }
                        (*p).flags |= MARK;
                    }
                    for i in 0..meta.field_len() as usize {
                        if meta.is_ref(i) {
                            let v = self.get_slot(p, i);
                            if v != 0 {
                                state.stack.push(v);
                            }
                        }
                    }
                    done += 1;
                }
                if state.stack.is_empty() {
                    state.phase = GcPhase::Sweep;
                }
                let ns = t.elapsed().as_nanos();
                state.steps += 1;
                state.max_step_ns = state.max_step_ns.max(ns);
                true
            }
            GcPhase::Sweep => {
                let end = (state.sweep_cursor + budget).min(self.live.len());
                for idx in state.sweep_cursor..end {
                    let off = self.live[idx];
                    let p = unsafe { self.base.add(off) } as *mut CompactObject;
                    let marked = unsafe { (*p).flags & MARK != 0 };
                    if marked {
                        unsafe { (*p).flags &= !MARK };
                        self.live[state.write_cursor] = off;
                        state.write_cursor += 1;
                    } else {
                        let cls = (unsafe { (*p).flags } >> 1) as usize;
                        let sz = *self.size_classes.get(cls).unwrap_or(&meta.size());
                        push_free(&mut self.free, sz, off, self.free_hint);
                        self.collected += 1;
                    }
                }
                state.sweep_cursor = end;
                let ns = t.elapsed().as_nanos();
                state.steps += 1;
                state.max_step_ns = state.max_step_ns.max(ns);
                if state.sweep_cursor >= self.live.len() {
                    self.live.truncate(state.write_cursor);
                    state.phase = GcPhase::Idle;
                    return false;
                }
                true
            }
            GcPhase::Idle => false,
        }
    }
}

/// 往对应尺寸的空闲桶里放一个偏移；桶不存在就新建。
fn push_free(free: &mut Vec<(usize, Vec<usize>)>, size: usize, off: usize, hint: usize) {
    if let Some(b) = free.iter_mut().find(|(s, _)| *s == size) {
        b.1.push(off);
    } else {
        let mut v = Vec::with_capacity(hint.max(8));
        v.push(off);
        free.push((size, v));
    }
}
