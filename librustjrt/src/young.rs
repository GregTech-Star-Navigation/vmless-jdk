//! 年轻代：复制式 minor GC（eden + survivor → to-space，超龄晋升老年代）。
//!
//! 根集 = **显式根（影子栈）** + **SATB 缓冲** + **所有脏卡里的对象**。
//! 最后一项就是缺陷 3 的修复点：老年代里 `old.a = young` 的边靠卡表把 `old`
//! 拉进根集，minor 才会顺着它找到 young。
//!
//! 卡表维护是**就地**的：扫描一个老对象时，只要修好后的槽里还有指向年轻代的
//! 引用，立刻把它所在的卡重新标脏。这样就不需要 GC 结束后再遍历一遍幸存老对象
//! （实测那是 1M 对象级别的一次额外全扫，几毫秒）。

use std::time::Instant;

use crate::cards::CARD_BYTES;
use crate::heap::GcEnv;
use crate::object::{
    age_of, class_of, copy_object, elem_slot, field_slot, flags_of, length_of, set_flags, size_at,
    FLAG_FORWARDED, FLAG_MARK, AGE_MASK,
};

/// 每处理这么多对象算一个「步」，用来量最大单步耗时
const CHUNK_OBJECTS: usize = 4096;

#[derive(Clone, Copy, Debug, Default)]
pub struct MinorReport {
    pub copied: usize,
    pub promoted: usize,
    pub promoted_bytes: usize,
    pub survived: usize,
    pub survived_bytes: usize,
    pub collected: usize,
    pub young_bytes: u32,
    /// 本轮扫描过的老对象数（脏卡 + 晋升 + 根里的）
    pub scanned_old: usize,
    pub max_step_ns: u128,
    pub pause_ns: u128,
    /// to-space 装不下、被迫提前晋升的次数（正常策略，不是错误）
    pub early_promotions: usize,
}

pub(crate) fn minor(env: &mut GcEnv) -> MinorReport {
    let t0 = Instant::now();
    env.work.clear();
    env.scanned.clear();
    env.copied = 0;
    env.promoted = 0;
    env.promoted_bytes = 0;
    env.survived = 0;
    env.survived_bytes = 0;
    env.max_step_ns = 0;
    env.failures = 0;

    let young_bytes = env.young_bytes();
    let to_start = env.semi[(1 - env.semi_idx) as usize].0;
    env.to_top.set(to_start);
    // 队列大小上界就是活对象数：先留好，省掉复制过程中的扩容搬迁
    env.work.reserve(env.young_objects.min(1 << 20));

    // ---- 1) 显式根（值式影子栈 + 指针式帧根区）----
    for i in 0..env.roots.len() {
        let v = env.roots[i];
        if v != 0 {
            env.roots[i] = fix(env, v);
        }
    }
    // 指针式帧根区：**就地**读、**就地**写回（JIT 帧的引用住在自己的栈槽里）
    for k in 0..env.regions.len() {
        let r = env.regions[k];
        for i in 0..r.n as usize {
            let p = unsafe { r.ptr.add(i) };
            let v = unsafe { *p };
            if v != 0 {
                unsafe { *p = fix(env, v) };
            }
        }
    }

    // ---- 2) SATB 缓冲（保守保留）----
    if !env.satb.is_empty() {
        let satb = std::mem::take(env.satb);
        for v in satb {
            if v != 0 {
                let nv = fix(env, v);
                env.satb.push(nv);
            }
        }
    }

    // ---- 3) 脏卡 → 老对象（这是「老 → 年轻」唯一漏不掉的入口）----
    let dirty = env.cards.drain();
    let mut found: Vec<u32> = Vec::new();
    for card in dirty {
        let lo = env.cards.card_lo(card);
        found.clear();
        env.starts.objects_in(lo, lo + CARD_BYTES, &mut found);
        for k in 0..found.len() {
            push_old(env, found[k]);
        }
    }

    // ---- 4) 复制 + 扫描（Cheney 队列）----
    let mut n = 0usize;
    let mut chunk = Instant::now();
    while let Some(off) = env.work.pop() {
        scan_slots(env, off);
        n += 1;
        if n % CHUNK_OBJECTS == 0 {
            let ns = chunk.elapsed().as_nanos();
            if ns > env.max_step_ns {
                env.max_step_ns = ns;
            }
            chunk = Instant::now();
        }
    }

    let survivors = env.survived + env.promoted;
    MinorReport {
        copied: env.copied,
        promoted: env.promoted,
        promoted_bytes: env.promoted_bytes,
        survived: env.survived,
        survived_bytes: env.survived_bytes,
        collected: env.young_objects.saturating_sub(survivors),
        young_bytes,
        scanned_old: env.marked,
        max_step_ns: env.max_step_ns,
        pause_ns: t0.elapsed().as_nanos(),
        early_promotions: env.failures,
    }
}

/// 把一个偏移「修好」：from-space 里的对象就地复制（或晋升），返回新偏移。
///
/// 三种情况原样返回：老对象、**已经在 to-space 里的对象**（否则会重复复制）、
/// 已经带转发指针的对象（返回转发地址）。
#[inline]
fn fix(env: &mut GcEnv, off: u32) -> u32 {
    if off == 0 {
        return 0;
    }
    if !env.is_evacuable(off) {
        return off;
    }
    let base = env.base;
    unsafe {
        let fl = flags_of(base, off);
        if fl & FLAG_FORWARDED != 0 {
            // 已经搬过：class_id 字段里存的是新偏移
            return class_of(base, off);
        }
        let size = size_at(base, off, env.layout);
        let age = age_of(fl) + 1;
        let promote = age >= env.tenuring_threshold;
        let mut dst = if promote { None } else { env.alloc_to(size) };
        if dst.is_none() && !promote {
            // to-space 装不下：提前晋升（标准策略）
            env.failures += 1;
        }
        let dst = match dst.take() {
            Some(d) => d,
            None => match env.alloc_tail(size) {
                Some(d) => d,
                None => panic!(
                    "minor: 尾部空间不足（off={off} size={size}）：prepare_minor 预检失效"
                ),
            },
        };
        copy_object(base.add(off as usize), base.add(dst as usize), size as usize);
        let np = crate::object::hdr(base, dst);
        // 目标对象：class_id / length / size 由副本带过来，只改 flags（年龄 +1）
        (*np).flags =
            (fl & !AGE_MASK & !FLAG_FORWARDED & !FLAG_MARK) | (age << crate::object::AGE_SHIFT);
        // 源对象：写转发地址
        let op = crate::object::hdr(base, off);
        (*op).class_id = dst;
        (*op).flags = fl | FLAG_FORWARDED;
        env.work.push(dst);
        if promote {
            env.promoted += 1;
            env.promoted_bytes += size as usize;
            // 晋升对象现在是老对象：登记起点，脏卡才能顺着卡找到它
            env.starts.set(dst);
        } else {
            env.survived += 1;
            env.survived_bytes += size as usize;
        }
        env.copied += 1;
        dst
    }
}

/// 老对象入根集（MARK 去重）。年轻对象不需要去重：转发指针就是去重。
fn push_old(env: &mut GcEnv, off: u32) {
    unsafe {
        let fl = flags_of(env.base, off);
        if fl & FLAG_MARK != 0 {
            return;
        }
        set_flags(env.base, off, fl | FLAG_MARK);
    }
    env.marked += 1;
    env.work.push(off);
}

/// 扫描一个对象（可能刚被复制）的引用槽，并修正其中的年轻引用。
///
/// 老对象扫完就地清 MARK，并且**如果它还指向年轻代就把卡标脏** ——
/// 这代替了「GC 完再全扫一遍幸存老对象」的后处理。
#[inline]
fn scan_slots(env: &mut GcEnv, off: u32) {
    let base = env.base;
    let layout = env.layout;
    let is_old = !env.is_young(off);
    let mut has_young = false;
    unsafe {
        let cls = layout.class(class_of(base, off));
        if cls.is_array {
            if cls.ref_elem {
                let len = length_of(base, off);
                for i in 0..len {
                    let sp = elem_slot(base, off, i);
                    let v = *sp;
                    if v != 0 {
                        let nv = fix(env, v);
                        if nv != v {
                            *sp = nv;
                        }
                        if env.is_young(nv) {
                            has_young = true;
                        }
                    }
                }
            }
        } else {
            cls.for_each_ref(|i| {
                let sp = field_slot(base, off, i);
                let v = *sp;
                if v != 0 {
                    let nv = fix(env, v);
                    if nv != v {
                        *sp = nv;
                    }
                    if env.is_young(nv) {
                        has_young = true;
                    }
                }
            });
        }
        if is_old {
            let fl = flags_of(base, off);
            set_flags(base, off, fl & !FLAG_MARK);
            if has_young {
                env.cards.mark(off);
            }
        }
    }
}
