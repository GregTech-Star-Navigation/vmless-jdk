//! 卡表 / 记忆集 + 对象起点图。
//!
//! 这是修掉「老 → 年轻引用丢失」缺陷的地方：写屏障把**老对象所在的卡**标脏，
//! minor 的根集 = 显式根 + 所有脏卡里的对象（靠 `StartMap` 把卡解析成对象）。

use crate::object::ALIGN;

/// 256 字节一张卡
pub const CARD_SHIFT: u32 = 8;
pub const CARD_BYTES: u32 = 1 << CARD_SHIFT;

/// 卡表：只覆盖老年代（含 LOS）那一段 `[space_start, cap)`。
pub struct CardTable {
    space_start: u32,
    bits: Vec<u8>,
    dirty: Vec<u32>,
}

impl CardTable {
    pub fn new(space_start: u32, cap: usize) -> CardTable {
        let n = cards_for(space_start, cap);
        CardTable { space_start, bits: vec![0; n], dirty: Vec::new() }
    }

    #[inline]
    pub fn space_start(&self) -> u32 {
        self.space_start
    }

    /// 堆扩容后同步尺寸（已标脏的位保留）
    pub fn resize(&mut self, cap: usize) {
        let n = cards_for(self.space_start, cap);
        self.bits.resize(n, 0);
    }

    #[inline]
    fn index(&self, off: u32) -> usize {
        ((off - self.space_start) >> CARD_SHIFT) as usize
    }

    /// 第 `card` 张卡的起始偏移
    #[inline]
    pub fn card_lo(&self, card: u32) -> u32 {
        self.space_start + (card << CARD_SHIFT)
    }

    /// 标脏；只有从「干净 → 脏」时才记入列表（去重）
    #[inline]
    pub fn mark(&mut self, off: u32) {
        if off < self.space_start {
            return;
        }
        let i = self.index(off);
        if i >= self.bits.len() {
            return;
        }
        if self.bits[i] == 0 {
            self.bits[i] = 1;
            self.dirty.push(i as u32);
        }
    }

    #[inline]
    pub fn is_dirty(&self, off: u32) -> bool {
        if off < self.space_start {
            return false;
        }
        match self.bits.get(self.index(off)) {
            Some(&b) => b != 0,
            None => false,
        }
    }

    /// 取走脏卡列表并清位（minor 收集根集时用）
    pub fn drain(&mut self) -> Vec<u32> {
        for &i in &self.dirty {
            if let Some(b) = self.bits.get_mut(i as usize) {
                *b = 0;
            }
        }
        std::mem::take(&mut self.dirty)
    }

    /// 清空（不返回列表）
    pub fn clear(&mut self) {
        for &i in &self.dirty {
            if let Some(b) = self.bits.get_mut(i as usize) {
                *b = 0;
            }
        }
        self.dirty.clear();
    }

    pub fn dirty_len(&self) -> usize {
        self.dirty.len()
    }
}

fn cards_for(space_start: u32, cap: usize) -> usize {
    let span = (cap as u64).saturating_sub(space_start as u64);
    ((span >> CARD_SHIFT) + 1) as usize
}

/// 对象起点图：1 bit / 8 字节，只覆盖老年代。
///
/// 有了它，「脏卡 → 卡里有哪些对象」就是精确的（不用再扫整个老年代找对象头）。
pub struct StartMap {
    start: u32,
    bits: Vec<u64>,
}

impl StartMap {
    pub fn new(start: u32, cap: usize) -> StartMap {
        let bits = vec![0u64; words_for(start, cap)];
        StartMap { start, bits }
    }

    pub fn resize(&mut self, cap: usize) {
        self.bits.resize(words_for(self.start, cap), 0);
    }

    #[inline]
    fn bit_index(&self, off: u32) -> usize {
        (off - self.start) as usize / ALIGN
    }

    #[inline]
    pub fn set(&mut self, off: u32) {
        if off < self.start {
            return;
        }
        let b = self.bit_index(off);
        let w = b / 64;
        if w < self.bits.len() {
            self.bits[w] |= 1u64 << (b % 64);
        }
    }

    #[inline]
    pub fn clear(&mut self, off: u32) {
        if off < self.start {
            return;
        }
        let b = self.bit_index(off);
        let w = b / 64;
        if w < self.bits.len() {
            self.bits[w] &= !(1u64 << (b % 64));
        }
    }

    #[inline]
    pub fn contains(&self, off: u32) -> bool {
        if off < self.start {
            return false;
        }
        let b = self.bit_index(off);
        match self.bits.get(b / 64) {
            Some(&w) => w & (1u64 << (b % 64)) != 0,
            None => false,
        }
    }

    /// 把 `[lo, hi)` 内的所有对象起点追加到 `out`
    pub fn objects_in(&self, lo: u32, hi: u32, out: &mut Vec<u32>) {
        if hi <= self.start || self.bits.is_empty() {
            return;
        }
        let lo = lo.max(self.start);
        let b0 = self.bit_index(lo);
        let b1 = self.bit_index(hi.max(lo));
        let total = self.bits.len() * 64;
        if b0 >= total {
            return;
        }
        let b1 = b1.min(total);
        let mut w = b0 / 64;
        while w * 64 < b1 {
            let mut word = self.bits[w];
            if w == b0 / 64 {
                word &= !0u64 << (b0 % 64);
            }
            if w == (b1 - 1) / 64 {
                let top = b1 % 64;
                if top != 0 {
                    word &= (1u64 << top) - 1;
                }
            }
            while word != 0 {
                let t = word.trailing_zeros();
                let bit = (w * 64 + t as usize) as u32;
                out.push(self.start + bit * ALIGN as u32);
                word &= word - 1;
            }
            w += 1;
        }
    }

    /// 已登记的对象起点个数（诊断用）
    pub fn count_ones(&self) -> usize {
        self.bits.iter().map(|w| w.count_ones() as usize).sum()
    }
}

fn words_for(start: u32, cap: usize) -> usize {
    let span = (cap as u64).saturating_sub(start as u64) as usize;
    span.div_ceil(ALIGN * 64).max(1)
}
