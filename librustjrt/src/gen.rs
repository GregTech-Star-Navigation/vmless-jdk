//! 分代回收：年轻代 bump 分配 + 复制式清理，存活对象晋升到老年代。
//!
//! 布局：一整块内存，年轻代从低地址向上长，老年代从高地址向下长，
//! 两边都用相对 base 的 u32 偏移表示引用，所以复制后只需改偏移。
//!
//! 一轮 minor 的工作量正比于 **存活对象数**（不是分配次数），
//! 这是它比全堆标记清扫快一个量级的原因。
//!
//! 转发指针直接写在年轻对象的头里（class_id 存新偏移，flags 置 FORWARDED），
//! 不需要为存活对象建哈希表。

use crate::{CompactMeta, CompactObject, COMPACT_HEADER};

const FORWARDED: u32 = 0x8000_0000;

pub struct GenerationalHeap {
    base: *mut u8,
    cap: usize,
    /// 年轻代已用（从 0 向上）
    young_top: usize,
    /// 老年代起点：老年代占据 [old_start, cap)
    old_start: usize,
    /// 老年代里存活对象的偏移
    old_live: Vec<usize>,
    /// 老年代空闲槽（对象大小相同的前提下可直接复用）
    old_free: Vec<usize>,
    pub promoted: usize,
    pub collected: usize,
}

unsafe impl Send for GenerationalHeap {}

#[derive(Default, Debug)]
pub struct MinorStats {
    pub copied: usize,
    pub young_bytes: usize,
    pub steps: usize,
    pub max_step_ns: u128,
}

impl GenerationalHeap {
    pub fn new(cap: usize) -> Self {
        let layout = std::alloc::Layout::from_size_align(cap, 8).expect("layout");
        let base = unsafe { std::alloc::alloc(layout) };
        assert!(!base.is_null(), "heap alloc failed");
        GenerationalHeap {
            base,
            cap,
            // 偏移 0 保留为 null，第一个对象从 16 开始
            young_top: 16,
            old_start: cap,
            old_live: Vec::new(),
            old_free: Vec::new(),
            promoted: 0,
            collected: 0,
        }
    }

    pub fn young_used(&self) -> usize {
        self.young_top
    }

    pub fn old_used(&self) -> usize {
        self.cap - self.old_start
    }

    pub fn old_live_count(&self) -> usize {
        self.old_live.len()
    }

    /// 年轻代分配：一次加法。
    pub fn alloc(&mut self, meta: &CompactMeta) -> u32 {
        let size = meta.size();
        let off = self.young_top;
        assert!(off + size < self.old_start, "heap full");
        self.young_top += size;
        unsafe {
            let p = self.base.add(off) as *mut CompactObject;
            (*p).class_id = 0;
            (*p).flags = 0;
            let f = (p as *mut u8).add(COMPACT_HEADER) as *mut u32;
            for i in 0..meta.field_len() as usize {
                *f.add(i) = 0;
            }
        }
        off as u32
    }

    fn ptr(&self, off: u32) -> *mut CompactObject {
        unsafe { self.base.add(off as usize) as *mut CompactObject }
    }

    pub fn set_slot(&mut self, off: u32, idx: usize, v: u32) {
        unsafe {
            let f = (self.ptr(off) as *mut u8).add(COMPACT_HEADER) as *mut u32;
            *f.add(idx) = v;
        }
    }

    pub fn get_slot(&self, off: u32, idx: usize) -> u32 {
        unsafe {
            let f = (self.ptr(off) as *const u8).add(COMPACT_HEADER) as *const u32;
            *f.add(idx)
        }
    }

    /// 复制一个对象到老年代（若已转发则直接返回新偏移）。
    fn copy_to_old(&mut self, old_off: u32, meta: &CompactMeta, queue: &mut Vec<u32>) -> u32 {
        let p = self.ptr(old_off);
        let flags = unsafe { (*p).flags };
        if flags & FORWARDED != 0 {
            return unsafe { (*p).class_id };
        }
        let size = meta.size();
        let new_off = if let Some(slot) = self.old_free.pop() {
            slot as u32
        } else {
            self.old_start -= size;
            self.old_start as u32
        };
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.base.add(old_off as usize),
                self.base.add(new_off as usize),
                size,
            );
            let np = self.base.add(new_off as usize) as *mut CompactObject;
            (*np).flags = 0;
            let op = self.base.add(old_off as usize) as *mut CompactObject;
            (*op).class_id = new_off;
            (*op).flags = flags | FORWARDED;
        }
        self.old_live.push(new_off as usize);
        self.promoted += 1;
        queue.push(new_off);
        new_off
    }

    /// 一轮年轻代回收。
    pub fn minor(&mut self, roots: &mut [u32], meta: &CompactMeta) -> MinorStats {
        let young_bytes = self.young_top;
        let mut queue: Vec<u32> = Vec::with_capacity(4096);
        self.old_live.reserve(4096);

        let mut copied = 0usize;
        let mut steps = 0usize;
        let mut max_step_ns = 0u128;

        for r in roots.iter_mut() {
            if *r == 0 {
                continue;
            }
            *r = self.copy_to_old(*r, meta, &mut queue);
        }

        let mut chunk_start = std::time::Instant::now();
        let mut in_chunk = 0usize;
        while let Some(new_off) = queue.pop() {
            for i in 0..meta.field_len() as usize {
                if meta.is_ref(i) {
                    let v = self.get_slot(new_off, i);
                    if v == 0 {
                        continue;
                    }
                    let mapped = if (v as usize) < young_bytes {
                        self.copy_to_old(v, meta, &mut queue)
                    } else {
                        v
                    };
                    self.set_slot(new_off, i, mapped);
                }
            }
            copied += 1;
            steps += 1;
            // 每 1024 个对象量一次，避免 Instant::now() 本身成为开销
            in_chunk += 1;
            if in_chunk == 1024 {
                let ns = chunk_start.elapsed().as_nanos();
                if ns > max_step_ns {
                    max_step_ns = ns;
                }
                chunk_start = std::time::Instant::now();
                in_chunk = 0;
            }
        }

        let objects = young_bytes / meta.size().max(1);
        if objects > copied {
            self.collected += objects - copied;
        }
        self.young_top = 0;

        MinorStats { copied, young_bytes, steps, max_step_ns }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FieldKind;

    fn node() -> CompactMeta {
        CompactMeta { name: "Node", fields: vec![FieldKind::Prim, FieldKind::Ref] }
    }

    #[test]
    fn minor_promotes_live_objects() {
        let meta = node();
        let mut h = GenerationalHeap::new(1 << 20);
        let a = h.alloc(&meta);
        let b = h.alloc(&meta);
        let c = h.alloc(&meta);
        h.set_slot(a, 0, 11);
        h.set_slot(b, 0, 22);
        h.set_slot(c, 0, 33);
        h.set_slot(a, 1, b); // a -> b，c 是垃圾
        let mut roots = [a];
        let st = h.minor(&mut roots, &meta);
        assert_eq!(st.copied, 2, "a 与 b 存活并被复制");
        assert_eq!(h.old_live_count(), 2);
        assert!(h.collected >= 1, "c 被回收");
        // 根被更新到新位置，且字段值仍然正确
        let r = roots[0];
        assert_eq!(h.get_slot(r, 0), 11);
        let child = h.get_slot(r, 1);
        assert_eq!(h.get_slot(child, 0), 22);
    }

    #[test]
    fn young_is_reset_after_minor() {
        let meta = node();
        let mut h = GenerationalHeap::new(1 << 20);
        for _ in 0..100 {
            h.alloc(&meta);
        }
        let before = h.young_used();
        assert!(before > 0);
        let mut roots: [u32; 0] = [];
        h.minor(&mut roots, &meta);
        assert_eq!(h.young_used(), 0, "年轻代整体作废");
    }

    #[test]
    fn promotes_count() {
        let meta = node();
        let mut h = GenerationalHeap::new(1 << 20);
        let a = h.alloc(&meta);
        let mut roots = [a];
        h.minor(&mut roots, &meta);
        assert_eq!(h.promoted, 1);
    }
}

// ===================== 老年代回收（标记清扫） =====================

impl GenerationalHeap {
    /// 老年代标记清扫：从 roots 出发标记，未标记的进空闲表。
    /// roots 里的偏移可能指向年轻代（不在老年代范围内），会被忽略。
    pub fn major(&mut self, roots: &[u32], meta: &CompactMeta) -> usize {
        const MARK: u32 = 1;
        let old_lo = self.old_start as u32;
        let mut work: Vec<u32> = Vec::new();
        for r in roots {
            if *r as usize >= self.old_start {
                work.push(*r);
            }
        }
        while let Some(off) = work.pop() {
            let p = self.ptr(off);
            unsafe {
                if (*p).flags & MARK != 0 {
                    continue;
                }
                (*p).flags |= MARK;
            }
            for i in 0..meta.field_len() as usize {
                if meta.is_ref(i) {
                    let v = self.get_slot(off, i);
                    if v >= old_lo {
                        work.push(v);
                    }
                }
            }
        }
        let mut kept = Vec::with_capacity(self.old_live.len());
        let mut freed = 0usize;
        for &off in &self.old_live {
            let p = self.ptr(off as u32);
            let marked = unsafe { (*p).flags & MARK != 0 };
            if marked {
                unsafe { (*p).flags &= !MARK };
                kept.push(off);
            } else {
                self.old_free.push(off);
                freed += 1;
            }
        }
        self.old_live = kept;
        self.collected += freed;
        freed
    }
}
