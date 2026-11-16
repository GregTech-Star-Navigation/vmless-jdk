//! 老年代：bump 分配（含 LOS 链表）+ 标记-清扫回收 + 可选压缩。
//!
//! 老年代和 LOS 都长在同一个尾部 arena 里（`tail_start` 之上），靠三条表分开记账：
//! `live`（普通老对象）、`los_live`（大对象）、两张按精确大小的空闲表。
//! 两者的区别只有一个：**复制式 young GC 不会碰 LOS**。

use std::cell::Cell;
use std::time::Instant;

use crate::alloc::FreeList;
use crate::heap::GcEnv;
use crate::object::{self, elem_slot, field_slot, flags_of, set_flags, size_at, FLAG_MARK};

/// 老年代 + LOS 的分配区与记账
pub struct OldSpace {
    pub tail_start: u32,
    top: Cell<u32>,
    /// 普通老对象（顺序无保证）
    pub live: Vec<u32>,
    /// 大对象（LOS）
    pub los_live: Vec<u32>,
    pub free: FreeList,
    pub los_free: FreeList,
    live_bytes: usize,
    los_bytes: usize,
}

impl OldSpace {
    pub fn new(tail_start: u32) -> OldSpace {
        OldSpace {
            tail_start,
            top: Cell::new(tail_start),
            live: Vec::new(),
            los_live: Vec::new(),
            free: FreeList::new(),
            los_free: FreeList::new(),
            live_bytes: 0,
            los_bytes: 0,
        }
    }

    /// 从尾部 bump（或复用空闲槽）。返回的槽**未被初始化**，调用方负责写对象头。
    #[inline]
    pub fn alloc(&mut self, cap: u32, size: u32) -> Option<u32> {
        if let Some(off) = self.free.pop(size) {
            self.live.push(off);
            self.live_bytes += size as usize;
            return Some(off);
        }
        let top = self.top.get();
        if top.checked_add(size)? <= cap {
            self.top.set(top + size);
            self.live.push(top);
            self.live_bytes += size as usize;
            Some(top)
        } else {
            None
        }
    }

    /// LOS 分配：独立链表，不参与复制
    #[inline]
    pub fn alloc_los(&mut self, cap: u32, size: u32) -> Option<u32> {
        if let Some(off) = self.los_free.pop(size) {
            self.los_live.push(off);
            self.los_bytes += size as usize;
            return Some(off);
        }
        let top = self.top.get();
        if top.checked_add(size)? <= cap {
            self.top.set(top + size);
            self.los_live.push(top);
            self.los_bytes += size as usize;
            Some(top)
        } else {
            None
        }
    }

    /// 尾部已 bump 出去多少字节（含已回收但未复用的空洞之前的全部）
    #[inline]
    pub fn committed(&self) -> u32 {
        self.top.get() - self.tail_start
    }

    #[inline]
    pub fn top(&self) -> u32 {
        self.top.get()
    }

    /// 压缩后重置尾部指针
    #[inline]
    pub fn set_top(&mut self, v: u32) {
        self.top.set(v);
    }

    /// 存活对象占的字节
    #[inline]
    pub fn live_bytes(&self) -> usize {
        self.live_bytes + self.los_bytes
    }

    #[inline]
    pub fn old_live_bytes(&self) -> usize {
        self.live_bytes
    }

    #[inline]
    pub fn los_bytes(&self) -> usize {
        self.los_bytes
    }

    fn note_free(&mut self, size: u32, los: bool) {
        if los {
            self.los_bytes = self.los_bytes.saturating_sub(size as usize);
        } else {
            self.live_bytes = self.live_bytes.saturating_sub(size as usize);
        }
    }
}

/// 一轮 major 的结果
#[derive(Clone, Copy, Debug, Default)]
pub struct MajorReport {
    pub freed: usize,
    pub freed_bytes: usize,
    pub los_freed: usize,
    pub kept: usize,
    pub marked: usize,
    pub pause_ns: u128,
    pub compacted: bool,
}

/// 老年代标记-清扫。根 = 显式根 + SATB 缓冲；年轻代对象作为中间节点被穿透
/// （年轻对象引用老对象时必须追踪，否则会把活的老对象扫掉）。
pub(crate) fn major(env: &mut GcEnv) -> MajorReport {
    let t0 = Instant::now();
    env.work.clear();
    env.scanned.clear();

    let roots: Vec<u32> = env.roots.iter().copied().filter(|&o| o != 0).collect();
    env.work.extend(roots);
    let satb = std::mem::take(env.satb);
    env.work.extend(satb.into_iter().filter(|&o| o != 0));

    // ---- 标记 ----
    while let Some(off) = env.work.pop() {
        if off == 0 {
            continue;
        }
        unsafe {
            let fl = flags_of(env.base, off);
            if fl & FLAG_MARK != 0 {
                continue;
            }
            set_flags(env.base, off, fl | FLAG_MARK);
            if env.is_young(off) {
                // 年轻对象的标记位只为去重，扫完要清
                env.scanned.push(off);
            } else {
                env.marked += 1;
            }
        }
        scan_refs(env, off);
    }

    // ---- 清扫 ----
    let mut freed = 0usize;
    let mut freed_bytes = 0usize;
    let mut kept = Vec::with_capacity(env.old.live.len());
    for i in 0..env.old.live.len() {
        let off = env.old.live[i];
        let fl = unsafe { flags_of(env.base, off) };
        if fl & FLAG_MARK != 0 {
            unsafe { set_flags(env.base, off, fl & !FLAG_MARK) };
            kept.push(off);
        } else {
            let size = unsafe { size_at(env.base, off, env.layout) };
            env.old.free.push(size, off);
            env.old.note_free(size, false);
            env.starts.clear(off);
            freed += 1;
            freed_bytes += size as usize;
        }
    }
    env.old.live = kept;

    let mut los_freed = 0usize;
    let mut los_kept = Vec::with_capacity(env.old.los_live.len());
    for i in 0..env.old.los_live.len() {
        let off = env.old.los_live[i];
        let fl = unsafe { flags_of(env.base, off) };
        if fl & FLAG_MARK != 0 {
            unsafe { set_flags(env.base, off, fl & !FLAG_MARK) };
            los_kept.push(off);
        } else {
            let size = unsafe { size_at(env.base, off, env.layout) };
            env.old.los_free.push(size, off);
            env.old.note_free(size, true);
            env.starts.clear(off);
            los_freed += 1;
            freed_bytes += size as usize;
        }
    }
    env.old.los_live = los_kept;

    // 年轻对象上的标记位清掉
    let young_marked = std::mem::take(&mut env.scanned);
    for &off in &young_marked {
        unsafe {
            let fl = flags_of(env.base, off);
            set_flags(env.base, off, fl & !FLAG_MARK);
        }
    }

    // ---- 重建卡表 ----
    // 老 → 年轻的边依然存在（幸存者还年轻），必须重新标脏，否则下一轮 minor 会漏掉
    env.cards.clear();
    for i in 0..env.old.live.len() {
        let off = env.old.live[i];
        if has_young_ref(env, off) {
            env.cards.mark(off);
        }
    }
    for i in 0..env.old.los_live.len() {
        let off = env.old.los_live[i];
        if has_young_ref(env, off) {
            env.cards.mark(off);
        }
    }

    MajorReport {
        freed,
        freed_bytes,
        los_freed,
        kept: env.old.live.len() + env.old.los_live.len(),
        marked: env.marked,
        pause_ns: t0.elapsed().as_nanos(),
        compacted: false,
    }
}

/// 遍历一个对象的引用槽，把引用值推进标记队列
pub(crate) fn scan_refs(env: &mut GcEnv, off: u32) {
    let base = env.base;
    let layout = env.layout;
    unsafe {
        let class_id = object::class_of(base, off);
        let cls = layout.class(class_id);
        if cls.is_array {
            if cls.ref_elem {
                let len = object::length_of(base, off);
                for i in 0..len {
                    let v = *elem_slot(base, off, i);
                    if v != 0 {
                        env.work.push(v);
                    }
                }
            }
            return;
        }
        let mut refs: [u32; 32] = [0; 32];
        let mut n = 0usize;
        let mut extra: Vec<u32> = Vec::new();
        cls.for_each_ref(|i| {
            if n < refs.len() {
                refs[n] = i;
                n += 1;
            } else {
                extra.push(i);
            }
        });
        for &i in &refs[..n] {
            let v = *field_slot(base, off, i);
            if v != 0 {
                env.work.push(v);
            }
        }
        for &i in &extra {
            let v = *field_slot(base, off, i);
            if v != 0 {
                env.work.push(v);
            }
        }
    }
}

/// 该对象的引用槽里是否还有指向年轻代的
pub(crate) fn has_young_ref(env: &GcEnv, off: u32) -> bool {
    let base = env.base;
    let layout = env.layout;
    unsafe {
        let class_id = object::class_of(base, off);
        let cls = layout.class(class_id);
        if cls.is_array {
            if !cls.ref_elem {
                return false;
            }
            let len = object::length_of(base, off);
            for i in 0..len {
                if env.is_young(*elem_slot(base, off, i)) {
                    return true;
                }
            }
            return false;
        }
        let mut found = false;
        cls.for_each_ref(|i| {
            if !found && env.is_young(*field_slot(base, off, i)) {
                found = true;
            }
        });
        found
    }
}

/// 老年代压缩：把存活对象向 `tail_start` 方向重新紧密排布。
///
/// 会**改变对象偏移**，所以所有外部引用（根表、SATB、对象之间的引用）都要重映射；
/// 老年代内部引用一律按重映射表改写，指向年轻代的引用保持不变。
/// JIT 手里直接存的 handle 在这之后必须从根表重新读 —— 文档里写明了这一点。
pub(crate) fn compact(env: &mut GcEnv) -> MajorReport {
    let t0 = Instant::now();
    let cap = env.cap;
    let base = env.base;
    let tail_start = env.old.tail_start;

    // 存活对象 = 老对象 + LOS 对象。**必须一起搬**：LOS 和老年代共用尾部 arena，
    // 只搬老对象会直接压到不可移动的 LOS 对象身上。
    let mut order: Vec<u32> = Vec::with_capacity(env.old.live.len() + env.old.los_live.len());
    order.extend_from_slice(&env.old.live);
    order.extend_from_slice(&env.old.los_live);
    order.sort_unstable();

    let mut map: Vec<(u32, u32)> = Vec::with_capacity(order.len());
    let mut cursor = tail_start;
    for &old in &order {
        let size = unsafe { size_at(base, old, env.layout) };
        map.push((old, cursor));
        cursor += size;
    }

    // 搬：升序，目标地址永远 <= 源地址（紧密排布），所以 memmove 安全
    for &(old, new) in &map {
        if old == new {
            continue;
        }
        let size = unsafe { size_at(base, old, env.layout) } as usize;
        unsafe {
            std::ptr::copy(base.add(old as usize), base.add(new as usize), size);
        }
    }

    // 改对象内部指向老年代的引用
    let layout = env.layout;
    for &(_, new) in &map {
        unsafe {
            let cls = layout.class(object::class_of(base, new));
            let is_arr = cls.is_array;
            let fix = |i: u32| {
                let sp = if is_arr {
                    elem_slot(base, new, i)
                } else {
                    field_slot(base, new, i)
                };
                let v = *sp;
                if v >= tail_start {
                    *sp = remap(&map, v).unwrap_or(0);
                }
            };
            if cls.is_array {
                if cls.ref_elem {
                    let len = object::length_of(base, new);
                    for i in 0..len {
                        fix(i);
                    }
                }
            } else {
                cls.for_each_ref(fix);
            }
        }
    }

    // 改根 / SATB
    for r in env.roots.iter_mut() {
        if *r >= tail_start {
            *r = remap(&map, *r).unwrap_or(0);
        }
    }
    for r in env.satb.iter_mut() {
        if *r >= tail_start {
            *r = remap(&map, *r).unwrap_or(0);
        }
    }

    // 起点图重建（老的位要清，新的位要置）
    for &(old, new) in &map {
        env.starts.clear(old);
        env.starts.clear(new);
    }
    for &(_, new) in &map {
        env.starts.set(new);
    }

    // 存活表按 FLAG_LOS 重新分桶（flags 随 memcpy 保留）
    let mut new_live: Vec<u32> = Vec::with_capacity(map.len());
    let mut new_los: Vec<u32> = Vec::new();
    for &(_, new) in &map {
        let fl = unsafe { flags_of(base, new) };
        if fl & crate::object::FLAG_LOS != 0 {
            new_los.push(new);
        } else {
            new_live.push(new);
        }
    }
    env.old.live = new_live;
    env.old.los_live = new_los;
    env.old.free.clear();
    env.old.los_free.clear();
    env.old.set_top(cursor.min(cap));
    let kept = env.old.live.len();

    // 卡表重建
    env.cards.clear();
    for i in 0..env.old.live.len() {
        let off = env.old.live[i];
        if has_young_ref(env, off) {
            env.cards.mark(off);
        }
    }
    for i in 0..env.old.los_live.len() {
        let off = env.old.los_live[i];
        if has_young_ref(env, off) {
            env.cards.mark(off);
        }
    }

    MajorReport {
        freed: 0,
        freed_bytes: 0,
        los_freed: 0,
        kept,
        marked: kept,
        pause_ns: t0.elapsed().as_nanos(),
        compacted: true,
    }
}

fn remap(map: &[(u32, u32)], off: u32) -> Option<u32> {
    map.binary_search_by_key(&off, |&(o, _)| o)
        .ok()
        .map(|i| map[i].1)
}
