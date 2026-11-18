//! 根集：`RootRegion`（指针式帧根区）、`RootScope`（RAII 注册/注销）、handle。
//!
//! 旧 API 让调用方在栈上临时拼 `Vec<*mut T>` 传给 `collect`，GC 一旦和编译代码
//! 配合就没法遍历 —— 编译代码的根在寄存器里。这里的根表是**堆对象自己持有**的
//! 一张偏移表：JIT 把活的引用放进来，GC 就地从表里读、并把被移动的对象写回表里。
//!
//! 两种根并存：
//!
//! * **值式**（`RootScope`，`Vec<u32>`）：登记的是偏移的**拷贝**，所以调用方必须
//!   在「值可能变」的每个点上重新同步一遍（JIT 曾经就是每个调用点 push/pop）。
//!   现在只剩常驻根（异常对象 / 字符串常量 / 静态引用字段）和解释器在用。
//! * **指针式**（[`RootRegion`]）：登记的是**帧根区的地址**，GC 就地读、就地写回，
//!   JIT 帧只在进入时登记一次、退出时注销 —— 调用点不再碰根表。
//!
//! 为什么指针式是主力：值式那对 push/pop 实测 34.7ns（`with_heap` 空过就 6.0ns/次，
//! `Rc+Vec` 再 5.8ns，其余是逐槽拷贝与 ABI 穿越），而 16M 次虚调用里它占了 83%
//! 的时间。指针式把这份成本压成「每帧一次登记」，收益覆盖**所有**方法调用。

use std::cell::RefCell;
use std::rc::Rc;

use crate::heap::Heap;

/// 指针式帧根区：只记「地址 + 槽数」，**不拷贝值**。
///
/// GC 标记时按地址就地读这块内存（`n` 个 4 字节槽，`0` = null），搬动对象
/// 后就地把新偏移写回去 —— 所以帧里的引用永远是 GC 见过的那个值，不需要「回来
/// 再刷新一遍」。
///
/// # 两个硬约束（违反其一是漏根或读野内存）
///
/// 1. **地址必须从登记到注销保持不变**。JIT 帧根区一律落在帧自己的栈槽里
///    （`create_sized_stack_slot`），地址在帧的生命周期内稳定；**绝不能**放在会
///    realloc 的 `Vec` / `String` 里 —— 那是「搬一次就漏一片」。
/// 2. **注销必须严格 LIFO**，而且每个退出路径（含异常传播出方法）都要走到。
///    漏注销的条目是一根**悬空的栈地址**：下一轮 GC 会去读一个已经作废的帧。
///    `Heap::pop_region` 因此校验栈顶指针，不平衡当场报错。
#[derive(Clone, Copy)]
pub struct RootRegion {
    /// 帧根区首地址（`n` 个连续 `u32`）
    pub ptr: *mut u32,
    pub n: u32,
}

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
