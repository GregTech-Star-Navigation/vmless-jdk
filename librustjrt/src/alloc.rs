//! 分配原语：堆内存区（可 realloc 增长）、TLAB bump、按精确大小的空闲表。

use std::alloc::{alloc, dealloc, realloc, Layout};
use std::cell::Cell;
use std::ptr::NonNull;

pub const MIN_ALIGN: usize = 8;

/// 一整块堆内存。偏移寻址让 `grow`（realloc 搬块）之后所有 handle 依然有效。
pub struct Region {
    ptr: NonNull<u8>,
    cap: usize,
}

impl Region {
    pub fn new(cap: usize) -> Region {
        let cap = cap.max(MIN_ALIGN * 64) & !(MIN_ALIGN - 1);
        let layout = Layout::from_size_align(cap, MIN_ALIGN).expect("region layout");
        let ptr = NonNull::new(unsafe { alloc(layout) })
            .unwrap_or_else(|| panic!("堆分配 {cap} 字节失败"));
        Region { ptr, cap }
    }

    #[inline]
    pub fn base(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    #[inline]
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// 扩到 `new_cap`（只增不减请用 `grow`）；内容前 `min(old,new)` 字节保留。
    pub fn grow(&mut self, new_cap: usize) {
        let new_cap = new_cap & !(MIN_ALIGN - 1);
        assert!(new_cap > self.cap, "grow 只能变大");
        let old_layout = Layout::from_size_align(self.cap, MIN_ALIGN).expect("layout");
        let p = unsafe { realloc(self.ptr.as_ptr(), old_layout, new_cap) };
        self.ptr = NonNull::new(p).unwrap_or_else(|| panic!("堆扩容到 {new_cap} 字节失败"));
        self.cap = new_cap;
    }

    /// 缩到 `new_cap`。**调用方必须保证 `new_cap` 以上没有任何活对象**（否则数据被截断）。
    pub fn shrink(&mut self, new_cap: usize) {
        let new_cap = (new_cap & !(MIN_ALIGN - 1)).max(MIN_ALIGN * 64);
        if new_cap >= self.cap {
            return;
        }
        let old_layout = Layout::from_size_align(self.cap, MIN_ALIGN).expect("layout");
        let p = unsafe { realloc(self.ptr.as_ptr(), old_layout, new_cap) };
        self.ptr = NonNull::new(p).unwrap_or_else(|| panic!("堆收缩到 {new_cap} 字节失败"));
        self.cap = new_cap;
    }
}

unsafe impl Send for Region {}

impl Drop for Region {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.cap, MIN_ALIGN).expect("layout");
        unsafe { dealloc(self.ptr.as_ptr(), layout) };
    }
}

/// 线程本地分配缓冲：只用 `Cell`，所以 `tlab_alloc(&self, …)` 不需要独占借用。
/// JIT 侧以后可以直接把这个 bump 编进代码（单线程够用；多线程时每线程一个 Tlab）。
pub struct Tlab {
    start: Cell<u32>,
    top: Cell<u32>,
    end: Cell<u32>,
}

impl Default for Tlab {
    fn default() -> Self {
        Self::new()
    }
}

impl Tlab {
    pub fn new() -> Tlab {
        Tlab { start: Cell::new(0), top: Cell::new(0), end: Cell::new(0) }
    }

    /// 无锁 bump。返回对象偏移，空间不够返回 `None`（调用方 refill 或走 slow path）。
    #[inline]
    pub fn alloc(&self, size: u32) -> Option<u32> {
        let top = self.top.get();
        let end = self.end.get();
        if top.checked_add(size)? <= end {
            self.top.set(top + size);
            Some(top)
        } else {
            None
        }
    }

    /// 从 `start` 起划出 `size` 字节
    pub fn reserve(&mut self, start: u32, size: u32) {
        self.start.set(start);
        self.top.set(start);
        self.end.set(start + size);
    }

    pub fn reset(&mut self) {
        self.start.set(0);
        self.top.set(0);
        self.end.set(0);
    }

    #[inline]
    pub fn top(&self) -> u32 {
        self.top.get()
    }

    #[inline]
    pub fn end(&self) -> u32 {
        self.end.get()
    }

    #[inline]
    pub fn remaining(&self) -> u32 {
        self.end.get().saturating_sub(self.top.get())
    }

    /// 本块已用字节
    #[inline]
    pub fn used(&self) -> u32 {
        self.top.get().saturating_sub(self.start.get())
    }

    /// 本块总字节
    #[inline]
    pub fn size(&self) -> u32 {
        self.end.get().saturating_sub(self.start.get())
    }
}

/// 按**精确大小**索引的空闲表（老年代 / LOS 复用）。
///
/// 对象大小只有少数几种（每个类的 size），所以线性扫几张桶比哈希快。
pub struct FreeList {
    buckets: Vec<(u32, Vec<u32>)>,
    bytes: usize,
}

impl Default for FreeList {
    fn default() -> Self {
        Self::new()
    }
}

impl FreeList {
    pub fn new() -> FreeList {
        FreeList { buckets: Vec::new(), bytes: 0 }
    }

    pub fn push(&mut self, size: u32, off: u32) {
        if let Some(b) = self.buckets.iter_mut().find(|(s, _)| *s == size) {
            b.1.push(off);
        } else {
            self.buckets.push((size, vec![off]));
        }
        self.bytes += size as usize;
    }

    #[inline]
    pub fn pop(&mut self, size: u32) -> Option<u32> {
        if self.buckets.is_empty() {
            return None;
        }
        let off = {
            let b = self.buckets.iter_mut().find(|(s, _)| *s == size)?;
            b.1.pop()?
        };
        self.bytes -= size as usize;
        Some(off)
    }

    pub fn has(&self, size: u32) -> bool {
        self.buckets
            .iter()
            .find(|(s, _)| *s == size)
            .map(|(_, v)| !v.is_empty())
            .unwrap_or(false)
    }

    /// 空闲槽字节数
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn count(&self) -> usize {
        self.buckets.iter().map(|(_, v)| v.len()).sum()
    }

    pub fn sizes(&self) -> impl Iterator<Item = u32> + '_ {
        self.buckets.iter().map(|(s, _)| *s)
    }

    pub fn clear(&mut self) {
        self.buckets.clear();
        self.bytes = 0;
    }
}
