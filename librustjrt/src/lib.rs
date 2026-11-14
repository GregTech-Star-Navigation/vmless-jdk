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
