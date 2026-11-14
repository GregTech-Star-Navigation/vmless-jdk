//! Rust 版 Java 运行时的最小子集：对象布局 + bump 分配器 + 标记清扫骨架。
//!
//! 目标：Java 对象不再由 HotSpot 的 GC 堆管理，而是由 Rust 分配器分配、
//! 由 Rust 侧的生命周期策略回收。当前版本先把"对象是什么、怎么分配"钉死。

use std::alloc::{alloc, dealloc, Layout};
use std::sync::atomic::{AtomicUsize, Ordering};

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
    base: *mut u8,
    cap: usize,
    used: AtomicUsize,
}

unsafe impl Send for BumpArena {}
unsafe impl Sync for BumpArena {}

impl BumpArena {
    pub fn new(cap: usize) -> Self {
        let layout = Layout::from_size_align(cap, 16).expect("layout");
        let base = unsafe { alloc(layout) };
        assert!(!base.is_null(), "arena alloc failed");
        BumpArena { base, cap, used: AtomicUsize::new(0) }
    }

    /// 分配 size 字节，16 字节对齐；返回偏移（0 表示失败）
    pub fn alloc(&self, size: usize) -> Option<usize> {
        let aligned = (size + 15) & !15;
        let start = self.used.fetch_add(aligned, Ordering::Relaxed);
        if start + aligned > self.cap {
            self.used.fetch_sub(aligned, Ordering::Relaxed);
            return None;
        }
        Some(start)
    }

    pub fn alloc_object(&self, field_len: u32) -> Option<*mut JavaObject> {
        let size = JavaObject::size_for(field_len);
        let off = self.alloc(size)?;
        let p = unsafe { self.base.add(off) } as *mut JavaObject;
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
        self.cap
    }
}

impl Drop for BumpArena {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.cap, 16).expect("layout");
        unsafe { dealloc(self.base, layout) };
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
    pub name: &'static str,
    pub fields: &'static [FieldKind],
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
        let mut marked: Vec<*mut JavaObject> = Vec::new();
        let mut work: Vec<*mut JavaObject> = roots.to_vec();
        while let Some(o) = work.pop() {
            if o.is_null() || marked.contains(&o) {
                continue;
            }
            marked.push(o);
            let meta = metas.iter().find(|m| m.size() == obj_size(o)).copied();
            if let Some(meta) = meta {
                for i in 0..meta.field_len() as usize {
                    if meta.is_ref(i) {
                        let r = self.get_ref(o, i);
                        if !r.is_null() && !marked.contains(&r) {
                            work.push(r);
                        }
                    }
                }
            }
        }
        let mut kept = Vec::new();
        for &o in &self.live {
            if marked.contains(&o) {
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
}

fn obj_size(o: *mut JavaObject) -> usize {
    unsafe { JavaObject::size_for((*o).field_len) }
}

#[cfg(test)]
mod gc_tests {
    use super::*;

    static NODE: ClassMeta = ClassMeta { name: "Node", fields: &[FieldKind::Ref, FieldKind::Prim] };

    #[test]
    fn mark_sweep_reclaims_unreachable() {
        let mut h = Heap::new(1 << 20);
        let root = h.alloc(&NODE);
        let a = h.alloc(&NODE);
        let b = h.alloc(&NODE);
        h.set_ref(root, 0, a);
        h.set_ref(a, 0, b);
        assert_eq!(h.live_count(), 3);

        // a -> b 这条链可达，全部保留
        h.collect(&[root], &[&NODE]);
        assert_eq!(h.collected, 0);
        assert_eq!(h.live_count(), 3);

        // 断掉 root->a，a 与 b 都不可达
        h.set_ref(root, 0, std::ptr::null_mut());
        h.collect(&[root], &[&NODE]);
        assert_eq!(h.collected, 2);
        assert_eq!(h.live_count(), 1);
    }

    #[test]
    fn freed_slots_are_reused() {
        let mut h = Heap::new(1 << 20);
        let root = h.alloc(&NODE);
        let tmp = h.alloc(&NODE);
        let _ = tmp;
        h.collect(&[root], &[&NODE]);
        assert_eq!(h.collected, 1);
        let used_before = h.arena.used();
        let reused = h.alloc(&NODE);
        assert_eq!(h.arena.used(), used_before, "应当复用空闲槽，不再向 arena 借内存");
        h.set_ref(root, 0, reused);
    }
}
