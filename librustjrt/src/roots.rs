//! 根集：`RootScope`（RAII 注册/注销）、handle、影子栈。
//!
//! 旧 API 让调用方在栈上临时拼 `Vec<*mut T>` 传给 `collect`，GC 一旦和编译代码
//! 配合就没法遍历 —— 编译代码的根在寄存器里。这里的根表是**堆对象自己持有**的
//! 一张偏移表：JIT 把活的引用放进来，GC 就地从表里读、并把被移动的对象写回表里。

use std::cell::RefCell;
use std::rc::Rc;

use crate::heap::Heap;

/// 堆内对象引用（相对基址的 4 字节偏移，`0` = null）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub struct Handle(pub u32);

impl Handle {
    pub const NULL: Handle = Handle(0);

    #[inline]
    pub fn raw(self) -> u32 {
        self.0
    }

    #[inline]
    pub fn from_raw(v: u32) -> Handle {
        Handle(v)
    }

    #[inline]
    pub fn is_null(self) -> bool {
        self.0 == 0
    }
}

/// 根作用域：构造时记住水位，drop 时截断回水位（= 自动注销这批根）。
///
/// 内部持有根表的 `Rc`，**不借用 `Heap`** —— 所以作用域还活着的时候依然可以
/// `heap.minor()` 或 `heap.collect()`（这是它能和 RAII 共存的前提）。
pub struct RootScope {
    roots: Rc<RefCell<Vec<u32>>>,
    base: usize,
}

impl RootScope {
    pub fn new(heap: &Heap) -> RootScope {
        let roots = heap.roots_handle();
        let base = roots.borrow().len();
        RootScope { roots, base }
    }

    /// 把一个 handle 登记为根；返回同一个 handle（便于链式写）
    pub fn root(&self, h: Handle) -> Handle {
        self.roots.borrow_mut().push(h.0);
        h
    }

    pub fn root_offset(&self, off: u32) -> u32 {
        self.roots.borrow_mut().push(off);
        off
    }

    /// 作用域内登记了几个根
    pub fn len(&self) -> usize {
        self.roots.borrow().len() - self.base
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 读回第 `i` 个根（GC 之后它可能已经被改写成新地址）
    pub fn handle(&self, i: usize) -> Handle {
        Handle(self.roots.borrow()[self.base + i])
    }

    /// 改写第 `i` 个根（JIT 侧寄存器里的引用变了就更新这里）
    pub fn set(&self, i: usize, h: Handle) {
        let mut r = self.roots.borrow_mut();
        r[self.base + i] = h.0;
    }

    /// 本作用域在全局根表里的起始下标
    pub fn index(&self) -> usize {
        self.base
    }
}

impl Drop for RootScope {
    fn drop(&mut self) {
        self.roots.borrow_mut().truncate(self.base);
    }
}
