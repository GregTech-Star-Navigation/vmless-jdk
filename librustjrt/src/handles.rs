//! 稳定句柄表：外部持有的 handle 在对象移动后依然有效。
//!
//! # 为什么需要
//!
//! 堆内的原生表示是「相对基址的 u32 偏移」。偏移寻址解决了 `realloc` 搬块的问题
//! （基址变、偏移不变），但**解决不了压缩**：老年代压缩会把对象从 `0x1200` 挪到
//! `0x1000`，任何存在寄存器 / 外部结构里的偏移立刻变成野地址。
//!
//! 这里引入一层间接：外部只拿 `StableHandle`（表下标 + 代号），对象真实偏移存在
//! 表里。压缩完成后把重映射表喂给 [`HandleTable::retarget`]，全部槽位一次性改好，
//! **外部手里的 handle 一个字节都不用动**。
//!
//! ```text
//! StableHandle(gen=1, idx=3)  ──►  slots[3] = { off: 0x2004, gen: 1 }
//!                                        │
//!                              compact 0x2004 → 0x1000
//!                                        ▼
//!                                  { off: 0x1000, gen: 1 }
//! ```
//!
//! 代号（gen）用来防 ABA：槽位被释放后重新分配时代号 +1，旧 handle 立刻解析失败，
//! 而不是悄悄指向一个不相干的新对象。
//!
//! 偏移直用的旧 API（`Handle`）一个都没动 —— 它现在扮演「兼容层」的角色：
//! 单线程、不压实时直接用它更快。

use std::collections::VecDeque;

/// 稳定句柄：`(代号 << 32) | 表下标`。
///
/// 8 字节、`Copy`、可哈希，可以塞进 JNI 局部引用表或 JIT 的常量池。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct StableHandle(u64);

impl StableHandle {
    /// 空句柄（永不解析成功）
    pub const NULL: StableHandle = StableHandle(u64::MAX);

    /// 从原始 64 位值还原（给 FFI / 序列化用）
    #[inline]
    pub const fn from_raw(v: u64) -> StableHandle {
        StableHandle(v)
    }

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
    const fn new(index: u32, generation: u32) -> StableHandle {
        StableHandle(((generation as u64) << 32) | index as u64)
    }
}

#[derive(Clone, Copy, Debug)]
struct Slot {
    /// 对象偏移；`0` 表示该槽当前不指向任何对象（已被回收）
    off: u32,
    /// 代号；`0` = 空闲槽
    generation: u32,
}

const FREE: Slot = Slot { off: 0, generation: 0 };

/// 句柄表。容量随对象数增长，空闲槽循环复用。
pub struct HandleTable {
    slots: Vec<Slot>,
    free: VecDeque<u32>,
    live: usize,
    /// 累计做过多少次重映射（诊断）
    retargets: u64,
}

impl Default for HandleTable {
    fn default() -> Self {
        Self::new()
    }
}

impl HandleTable {
    pub fn new() -> HandleTable {
        HandleTable { slots: Vec::new(), free: VecDeque::new(), live: 0, retargets: 0 }
    }

    /// 为偏移 `off` 建一个稳定句柄。
    pub fn alloc(&mut self, off: u32) -> StableHandle {
        if let Some(idx) = self.free.pop_front() {
            let s = &mut self.slots[idx as usize];
            debug_assert_eq!(s.generation, 0, "空闲槽的代号必须是 0");
            // 代号在 free 时已经 +1 了，这里原样保留 —— 重置成 1 就会让旧 handle 复活
            s.generation = s.generation.max(1);
            s.off = off;
            self.live += 1;
            return StableHandle::new(idx, self.slots[idx as usize].generation);
        }
        let idx = self.slots.len() as u32;
        self.slots.push(Slot { off, generation: 1 });
        self.live += 1;
        StableHandle::new(idx, 1)
    }

    /// 解析成当前偏移。句柄失效（槽已释放 / 对象已被回收）返回 `None`。
    #[inline]
    pub fn resolve(&self, h: StableHandle) -> Option<u32> {
        if h.is_null() {
            return None;
        }
        let s = self.slots.get(h.index() as usize)?;
        if s.generation == 0 || s.generation != h.generation() || s.off == 0 {
            None
        } else {
            Some(s.off)
        }
    }

    /// 改写句柄指向的对象（同一槽位的「重新绑定」）。
    #[inline]
    pub fn set(&mut self, h: StableHandle, off: u32) -> bool {
        let gen = h.generation();
        match self.slots.get_mut(h.index() as usize) {
            Some(s) if s.generation == gen && gen != 0 => {
                s.off = off;
                true
            }
            _ => false,
        }
    }

    /// 释放槽位：代号 +1，旧 handle 从此解析失败。
    pub fn free(&mut self, h: StableHandle) {
        let idx = h.index() as usize;
        match self.slots.get_mut(idx) {
            Some(s) if s.generation == h.generation() && s.generation != 0 => {
                // 代号回绕时跳过 0（0 是「空闲」标记）
                s.generation = s.generation.wrapping_add(1).max(1);
                s.off = 0;
                self.live -= 1;
                self.free.push_back(h.index());
            }
            _ => {}
        }
    }

    /// 把对象标成「已被回收」但**保留槽位**（弱引用语义：句柄还在，解析为空）。
    pub fn clear(&mut self, h: StableHandle) {
        if let Some(s) = self.slots.get_mut(h.index() as usize) {
            if s.generation == h.generation() {
                s.off = 0;
            }
        }
    }

    /// 压缩后重映射：把所有指向 `>= tail_start` 的槽位按 map 改写。
    ///
    /// `map` 必须是**按旧偏移升序**排好的 `(old, new)`。找不到的（对象被回收了）
    /// 一律清零 —— 这与「GC 后失效句柄解析为空」是同一个语义。
    pub fn retarget(&mut self, map: &[(u32, u32)]) -> usize {
        if map.is_empty() {
            return 0;
        }
        let mut changed = 0usize;
        let lo = map[0].0;
        for s in self.slots.iter_mut() {
            if s.generation == 0 || s.off == 0 || s.off < lo {
                continue;
            }
            match remap(map, s.off) {
                Some(n) if n != s.off => {
                    s.off = n;
                    changed += 1;
                }
                Some(_) => {}
                None => {
                    s.off = 0;
                    changed += 1;
                }
            }
        }
        if changed > 0 {
            self.retargets += 1;
        }
        changed
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
    pub fn retarget_count(&self) -> u64 {
        self.retargets
    }

    /// 遍历所有「指向 offset」的槽位（诊断 / 测试断言用）
    pub fn offsets(&self) -> impl Iterator<Item = u32> + '_ {
        self.slots.iter().filter(|s| s.generation != 0 && s.off != 0).map(|s| s.off)
    }
}

/// 在升序 `(old, new)` 表里查 `off`
pub fn remap(map: &[(u32, u32)], off: u32) -> Option<u32> {
    map.binary_search_by_key(&off, |&(o, _)| o).ok().map(|i| map[i].1)
}

// 让 `FREE` 常量不触发 dead_code（它在 `new` 的语义里是文档，槽位回收就地写字段）
const _: () = assert!(FREE.generation == 0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_follows_retarget() {
        let mut t = HandleTable::new();
        let a = t.alloc(0x1000);
        let b = t.alloc(0x2000);
        assert_eq!(t.resolve(a), Some(0x1000));
        // 压缩：0x1000 → 0x800，0x2000 → 0x1000
        let changed = t.retarget(&[(0x1000, 0x800), (0x2000, 0x1000)]);
        assert_eq!(changed, 2);
        assert_eq!(t.resolve(a), Some(0x800));
        assert_eq!(t.resolve(b), Some(0x1000));
    }

    #[test]
    fn freed_handle_does_not_alias_reused_slot() {
        let mut t = HandleTable::new();
        let a = t.alloc(0x10);
        t.free(a);
        let b = t.alloc(0x20);
        assert_eq!(b.index(), a.index(), "槽位应该被复用");
        assert_ne!(b.generation(), a.generation(), "代号必须变");
        assert_eq!(t.resolve(a), None, "旧 handle 不能悄悄指向新对象");
        assert_eq!(t.resolve(b), Some(0x20));
    }
}

impl HandleTable {
    /// 清掉指向已回收对象的槽位。`is_live(off)` 由调用方提供（通常是对象起点图）。
    ///
    /// 没有这一步，普通 `major()`（不压缩）之后句柄里留着的就是已释放的偏移 ——
    /// 解析出来看着是个有效数字，其实指向别人的数据。压缩路径有 `retarget` 兜着，
    /// 清扫路径必须有这个。
    pub fn prune(&mut self, is_live: impl Fn(u32) -> bool) -> usize {
        let mut n = 0usize;
        for s in self.slots.iter_mut() {
            if s.generation == 0 || s.off == 0 {
                continue;
            }
            if !is_live(s.off) {
                s.off = 0;
                n += 1;
            }
        }
        n
    }
}

impl HandleTable {
    /// 年轻代复制之后修句柄：`f(旧偏移)` 返回 `Some(新偏移)` = 活着搬过去了，
    /// `None` = 对象死了（槽位清空）。
    ///
    /// 和弱引用的处理是同一套：遍历槽位，`O(句柄数)`，与年轻代大小无关。
    /// 用转发位判死活的前提是 from-space 还没被下一次分配覆盖。
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
