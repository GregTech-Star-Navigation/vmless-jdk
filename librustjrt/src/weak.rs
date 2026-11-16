//! 弱引用与 finalization 队列。
//!
//! 两件事的共同点：都关心「这个对象**没被标到**」这个事实，而只有清扫阶段知道它。
//!
//! # 弱引用
//!
//! `WeakRef` 不阻止对象被回收。GC 结束后如果目标已死，句柄解析为空（`None`）；
//! 如果目标活着但**被移动了**（minor 复制 / 老年代压缩），槽位跟着改写成新偏移 ——
//! 所以弱引用的语义是「弱、但稳定」，不是「一次性观测」。
//!
//! 目标偏移到槽位的反向索引只在**有弱引用时**才建（`cycle_begin`），
//! 没有弱引用的堆每轮 GC 不额外花一个字节，也不额外做一次查找。
//!
//! # finalization 队列
//!
//! 对象带 `FLAG_FINALIZE` 且这轮没被标到 → 进队列，并且**这一轮先不回收**
//! （复活一轮），让外部 `take_finalized()` 拿到它时对象内容还是完好的。
//! 下一轮它是真的死了。`FLAG_FINALIZED` 保证只入队一次。
//!
//! 这条「复活一轮」的取舍：不复活的话队列里给出去的偏移立刻是野的，外部
//! finalizer 读一下就是 UB。代价是带 finalizer 的对象晚一轮释放。

use std::collections::{HashMap, VecDeque};

/// 弱引用句柄：`(代号 << 32) | 槽下标`
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct WeakRef(u64);

impl WeakRef {
    pub const NULL: WeakRef = WeakRef(u64::MAX);

    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }

    #[inline]
    pub const fn index(self) -> u32 {
        self.0 as u32
    }

    #[inline]
    pub const fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }

    #[inline]
    pub const fn is_null(self) -> bool {
        self.0 == u64::MAX
    }

    #[inline]
    const fn new(index: u32, generation: u32) -> WeakRef {
        WeakRef(((generation as u64) << 32) | index as u64)
    }
}

#[derive(Clone, Copy, Debug)]
struct WeakSlot {
    /// 目标对象偏移；`0` = 已被清空
    off: u32,
    /// 代号；`0` = 空闲槽
    generation: u32,
}

/// 弱引用表。生命周期跟着 `Heap` 走。
pub struct WeakTable {
    slots: Vec<WeakSlot>,
    free: VecDeque<u32>,
    live: usize,
    /// 目标偏移 → 指向它的槽下标（只在本轮 GC 期间有效）
    index: HashMap<u32, Vec<u32>>,
    indexing: bool,
    /// 本轮回填了多少个槽（诊断）
    retargeted: u32,
}

impl Default for WeakTable {
    fn default() -> Self {
        Self::new()
    }
}

impl WeakTable {
    pub fn new() -> WeakTable {
        WeakTable {
            slots: Vec::new(),
            free: VecDeque::new(),
            live: 0,
            index: HashMap::new(),
            indexing: false,
            retargeted: 0,
        }
    }

    /// 建一个指向 `off` 的弱引用
    pub fn alloc(&mut self, off: u32) -> WeakRef {
        if off == 0 {
            return WeakRef::NULL;
        }
        let (idx, generation) = match self.free.pop_front() {
            Some(i) => {
                let s = &mut self.slots[i as usize];
                s.off = off;
                // 代号在 free 时已经 +1，原样保留（重置成 1 会让旧句柄复活）
                s.generation = s.generation.max(1);
                (i, s.generation)
            }
            None => {
                let i = self.slots.len() as u32;
                self.slots.push(WeakSlot { off, generation: 1 });
                (i, 1)
            }
        };
        self.live += 1;
        if self.indexing {
            self.index.entry(off).or_default().push(idx);
        }
        WeakRef::new(idx, generation)
    }

    /// 解析成当前偏移。目标已死 → `None`。
    #[inline]
    pub fn get(&self, w: WeakRef) -> Option<u32> {
        if w.is_null() {
            return None;
        }
        let s = self.slots.get(w.index() as usize)?;
        if s.generation == 0 || s.generation != w.generation() || s.off == 0 {
            None
        } else {
            Some(s.off)
        }
    }

    /// 释放槽位（外部不再需要这个弱引用时调）
    pub fn free(&mut self, w: WeakRef) {
        if let Some(s) = self.slots.get_mut(w.index() as usize) {
            if s.generation == w.generation() && s.generation != 0 {
                s.generation = s.generation.wrapping_add(1).max(1);
                s.off = 0;
                self.live -= 1;
                self.free.push_back(w.index());
            }
        }
    }

    pub fn clear(&mut self, w: WeakRef) {
        if let Some(s) = self.slots.get_mut(w.index() as usize) {
            if s.generation == w.generation() {
                s.off = 0;
            }
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.live
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    #[inline]
    pub fn retargeted(&self) -> u32 {
        self.retargeted
    }

    // ---- GC 周期钩子 ----

    /// 一轮 GC 开始：有弱引用才建反向索引
    pub fn cycle_begin(&mut self) {
        self.retargeted = 0;
        if self.live == 0 {
            self.indexing = false;
            return;
        }
        self.index.shrink_to_fit();
        self.index.clear();
        for i in 0..self.slots.len() {
            let s = self.slots[i];
            if s.generation != 0 && s.off != 0 {
                self.index.entry(s.off).or_default().push(i as u32);
            }
        }
        self.indexing = true;
    }

    /// 对象被搬走：把它对应的弱槽全部改写成新偏移。`O(1)`（没弱引用时是空操作）。
    #[inline(always)]
    pub fn on_move(&mut self, old: u32, new: u32) {
        if !self.indexing || self.index.is_empty() {
            return;
        }
        if let Some(list) = self.index.remove(&old) {
            for &i in &list {
                let s = &mut self.slots[i as usize];
                if s.off == old {
                    s.off = new;
                    self.retargeted += 1;
                }
            }
            self.index.entry(new).or_default().extend(list);
        }
    }

    /// 对象确认死亡：指向它的弱槽全部清空
    #[inline]
    pub fn on_death(&mut self, off: u32) {
        if self.indexing {
            if let Some(list) = self.index.remove(&off) {
                for &i in &list {
                    let s = &mut self.slots[i as usize];
                    if s.off == off {
                        s.off = 0;
                    }
                }
            }
        } else if self.live != 0 {
            // 没建索引（例如只有一轮没有活弱引用的 major）也要清，退化成线性扫
            for s in self.slots.iter_mut() {
                if s.off == off {
                    s.off = 0;
                }
            }
        }
    }

    /// 一轮 GC 结束：丢掉反向索引，放掉它占的桶
    pub fn cycle_end(&mut self) {
        self.indexing = false;
        self.index.clear();
        self.index.shrink_to_fit();
    }

    /// 压缩后的批量重映射（压缩是一次搬一堆，逐条 `on_move` 反而慢）
    pub fn retarget(&mut self, map: &[(u32, u32)]) -> usize {
        if map.is_empty() {
            return 0;
        }
        let lo = map[0].0;
        let mut n = 0usize;
        for s in self.slots.iter_mut() {
            if s.generation == 0 || s.off == 0 || s.off < lo {
                continue;
            }
            match crate::handles::remap(map, s.off) {
                Some(new) if new != s.off => {
                    s.off = new;
                    n += 1;
                }
                Some(_) => {}
                None => {
                    s.off = 0;
                    n += 1;
                }
            }
        }
        self.retargeted += n as u32;
        n
    }
}

/// 待 finalize 的对象队列（外部用 `Heap::take_finalized` / `drain_finalized` 取走）
#[derive(Default)]
pub struct FinalizerQueue {
    q: VecDeque<u32>,
}

impl FinalizerQueue {
    pub fn new() -> FinalizerQueue {
        FinalizerQueue { q: VecDeque::new() }
    }

    pub(crate) fn push(&mut self, off: u32) {
        self.q.push_back(off);
    }

    pub fn take(&mut self) -> Option<u32> {
        self.q.pop_front()
    }

    pub fn drain(&mut self, out: &mut Vec<u32>) {
        out.extend(self.q.drain(..));
    }

    pub fn len(&self) -> usize {
        self.q.len()
    }

    pub fn is_empty(&self) -> bool {
        self.q.is_empty()
    }

    pub fn clear(&mut self) {
        self.q.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weak_survives_move() {
        let mut w = WeakTable::new();
        let a = w.alloc(0x1000);
        w.cycle_begin();
        w.on_move(0x1000, 0x800);
        w.cycle_end();
        assert_eq!(w.get(a), Some(0x800));
    }

    #[test]
    fn weak_clears_on_death() {
        let mut w = WeakTable::new();
        let a = w.alloc(0x1000);
        let b = w.alloc(0x2000);
        w.cycle_begin();
        w.on_death(0x1000);
        w.cycle_end();
        assert_eq!(w.get(a), None);
        assert_eq!(w.get(b), Some(0x2000), "别的弱引用不受影响");
    }

    #[test]
    fn freed_weak_does_not_alias() {
        let mut w = WeakTable::new();
        let a = w.alloc(0x1000);
        w.free(a);
        let b = w.alloc(0x2000);
        assert_eq!(w.get(a), None);
        assert_eq!(w.get(b), Some(0x2000));
    }
}

impl WeakTable {
    /// 年轻代复制之后修弱引用：对每个槽位调 `f(旧偏移)`，
    /// `Some(新偏移)` = 对象活着搬过去了，`None` = 对象死了（清空槽位）。
    ///
    /// 遍历的是**槽位**而不是对象，所以代价是 `O(弱引用数)`，与年轻代大小无关。
    /// 用转发位判死活的前提是：此时 from-space 还没被下一次分配覆盖。
    pub(crate) fn fix_young(&mut self, f: impl Fn(u32) -> Option<u32>) -> usize {
        let mut n = 0usize;
        for s in self.slots.iter_mut() {
            if s.generation == 0 || s.off == 0 {
                continue;
            }
            match f(s.off) {
                Some(new) => {
                    if new != s.off {
                        s.off = new;
                        n += 1;
                    }
                }
                None => {
                    s.off = 0;
                    n += 1;
                }
            }
        }
        n
    }
}
