//! 增量标记：把一轮 full GC 的「标记」拆成按预算步进的若干步。
//!
//! # 为什么需要
//!
//! `major()` 是全停顿：老年代 100 万对象、27ms 一口气跑完，JIT 出来的代码在这
//! 27ms 里一个字都不能动。增量标记把同一个算法切成 `step(budget)`，每次只扫
//! `budget` 个对象，外部在两拍之间想跑多久就跑多久。
//!
//! # 正确性的两条腿
//!
//! 并发（这里是「交错」）标记只有一个经典难题：**在标记过程中被改掉的引用**。
//! 两种错法各有一个屏障兜住，我们两个都上（保守：多标一点，不会漏标）：
//!
//! ```text
//!   ① A 已扫过、A.f = B（B 未标）  → 增量更新屏障：B 进灰队列
//!   ② A 未扫、A.f = null（B 是唯一引用） → SATB 删除屏障：旧值 B 记进 satb
//! ```
//!
//! 两条都不上就有两个著名漏洞：① 会让活对象被扫掉，② 会让活对象被提前回收。
//!
//! # 状态机
//!
//! ```text
//!   Idle ──step()/begin()──► Marking ──灰队列空──► Sweeping ──扫完──► Idle
//! ```
//!
//! `step()` 可重入。**Marking / Sweeping 期间不允许 minor**（复制会搬对象，
//! 灰队列里的偏移立刻变野）—— `Heap::minor()` 会先把这一轮增量标记推完再干活。

use std::time::Instant;

use crate::heap::GcEnv;
use crate::object::{flags_of, set_flags, size_at, FLAG_FINALIZE, FLAG_FINALIZED, FLAG_MARK};
use crate::old::MajorReport;

/// GC 阶段
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum GcPhase {
    /// 没有在跑的回收
    #[default]
    Idle,
    /// 增量标记中（灰队列未空）
    Marking,
    /// 增量清扫中
    Sweeping,
}

impl GcPhase {
    #[inline]
    pub fn is_idle(self) -> bool {
        self == GcPhase::Idle
    }
    #[inline]
    pub fn is_marking(self) -> bool {
        self == GcPhase::Marking
    }
}

/// 一次 `step()` 的结果
#[derive(Clone, Copy, Debug, Default)]
pub struct StepReport {
    /// 这一步结束后处于哪个阶段
    pub phase: GcPhase,
    /// 这一步扫了多少个对象
    pub scanned: usize,
    /// 还剩多少灰对象没扫（Marking 阶段）
    pub work_left: usize,
    /// 这一步花了多久
    pub elapsed_ns: u128,
    /// 这一步是否把整轮回收跑完了
    pub finished: bool,
    /// 跑完时给出的完整报告（没跑完是 `None`）
    pub report: Option<MajorReport>,
}

/// 增量标记的可重入状态。挂在 `Heap` 上，跨 `step()` 调用存活。
pub struct Marker {
    phase: GcPhase,
    /// 灰对象队列（已标但还没扫它的引用槽）
    work: Vec<u32>,
    /// 年轻对象的标记位（周期末统一清）
    young_marked: Vec<u32>,
    /// 标记阶段扫过的老对象数
    marked: usize,
    /// 扫描计数（含年轻对象）
    scanned: usize,

    // ---- 清扫阶段 ----
    /// 清扫开始时从 `OldSpace::live` 取走的快照
    snapshot: Vec<u32>,
    los_snapshot: Vec<u32>,
    cursor: usize,
    los_cursor: usize,
    in_los: bool,
    /// 卡表重建的游标（清扫完还要按预算重建一遍卡表）
    rebuild_cursor: usize,
    /// 清扫列表是否已经收过尾（收尾只能做一次，否则重建游标会被重置）
    lists_done: bool,
    kept: Vec<u32>,
    los_kept: Vec<u32>,
    freed: usize,
    freed_bytes: usize,
    los_freed: usize,
    finalized: usize,
    weak_cleared: usize,

    started_ns: u128,
    /// 整轮跑了几次 step（诊断）
    steps: u32,
    /// 是不是刚刚在这一步跑完的
    just_finished: bool,
}

impl Default for Marker {
    fn default() -> Self {
        Self::new()
    }
}

impl Marker {
    pub fn new() -> Marker {
        Marker {
            phase: GcPhase::Idle,
            work: Vec::new(),
            young_marked: Vec::new(),
            marked: 0,
            scanned: 0,
            snapshot: Vec::new(),
            los_snapshot: Vec::new(),
            cursor: 0,
            los_cursor: 0,
            in_los: false,
            rebuild_cursor: 0,
            lists_done: false,
            kept: Vec::new(),
            los_kept: Vec::new(),
            freed: 0,
            freed_bytes: 0,
            los_freed: 0,
            finalized: 0,
            weak_cleared: 0,
            started_ns: 0,
            steps: 0,
            just_finished: false,
        }
    }

    #[inline]
    pub fn phase(&self) -> GcPhase {
        self.phase
    }

    #[inline]
    pub fn is_active(&self) -> bool {
        self.phase != GcPhase::Idle
    }

    /// 还剩多少灰对象（诊断 / 测试）
    #[inline]
    pub fn work_left(&self) -> usize {
        self.work.len()
    }

    #[inline]
    pub fn marked(&self) -> usize {
        self.marked
    }

    /// 上一轮跑完用了几步
    #[inline]
    pub fn last_steps(&self) -> u32 {
        self.steps
    }

    /// 开始一轮增量标记：种子 = 显式根 + SATB + 屏障着色的新值
    pub(crate) fn begin(&mut self, env: &mut GcEnv, grey: &mut Vec<u32>) {
        debug_assert!(!self.is_active(), "上一轮增量回收还没跑完就重新开始了");
        self.phase = GcPhase::Marking;
        self.work.clear();
        self.young_marked.clear();
        self.marked = 0;
        self.scanned = 0;
        self.kept.clear();
        self.los_kept.clear();
        self.snapshot.clear();
        self.los_snapshot.clear();
        self.cursor = 0;
        self.los_cursor = 0;
        self.in_los = false;
        self.rebuild_cursor = 0;
        self.lists_done = false;
        self.freed = 0;
        self.freed_bytes = 0;
        self.los_freed = 0;
        self.finalized = 0;
        self.weak_cleared = 0;
        self.steps = 0;
        self.just_finished = false;
        self.started_ns = uptime_ns();

        self.work.extend(env.roots.iter().copied().filter(|&o| o != 0));
        // 指针式帧根区（增量标记的开始：只读，不搬动）
        for k in 0..env.regions.len() {
            let r = env.regions[k];
            for i in 0..r.n as usize {
                let v = unsafe { *r.ptr.add(i) };
                if v != 0 {
                    self.work.push(v);
                }
            }
        }
        self.work.extend(env.satb.drain(..).filter(|&o| o != 0));
        self.work.extend(grey.drain(..).filter(|&o| o != 0));
        env.weak.cycle_begin();
    }
}

/// 进程内单调时钟的零点（`Instant` 不能凭空构造，得有个基准）
static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

#[inline]
fn uptime_ns() -> u128 {
    START.get_or_init(Instant::now).elapsed().as_nanos()
}

/// 按预算推进一步。`budget` = 这一步最多处理多少个对象（下限 1）。
pub(crate) fn step(
    env: &mut GcEnv,
    m: &mut Marker,
    budget: usize,
    grey: &mut Vec<u32>,
) -> StepReport {
    let t0 = Instant::now();
    let budget = budget.max(1);
    m.just_finished = false;
    if m.phase == GcPhase::Idle {
        m.begin(env, grey);
    }
    let mut done = 0usize;

    if m.phase == GcPhase::Marking {
        done = mark_step(env, m, budget, grey);
        if m.work.is_empty() {
            begin_sweep(env, m);
        }
    }
    if m.phase == GcPhase::Sweeping && done < budget {
        done += sweep_step(env, m, budget - done);
    } else if m.phase == GcPhase::Sweeping {
        // 预算刚好用完但还在清扫阶段：什么也不做，等下一拍
    }

    m.steps = m.steps.wrapping_add(1);
    let finished = m.just_finished;
    StepReport {
        phase: m.phase,
        scanned: done,
        work_left: m.work.len(),
        elapsed_ns: t0.elapsed().as_nanos(),
        finished,
        report: if finished { Some(finish_report(env, m)) } else { None },
    }
}

/// 标记阶段：最多扫 `budget` 个对象。返回实际扫了多少。
fn mark_step(env: &mut GcEnv, m: &mut Marker, budget: usize, grey: &mut Vec<u32>) -> usize {
    if !grey.is_empty() {
        m.work.extend(grey.drain(..).filter(|&o| o != 0));
    }
    if !env.satb.is_empty() {
        m.work.extend(env.satb.drain(..).filter(|&o| o != 0));
    }

    // 把工作队列借给 env：`scan_refs` 往 `env.work` 里推子对象
    std::mem::swap(&mut env.work, &mut m.work);
    let mut n = 0usize;
    while n < budget {
        let off = match env.work.pop() {
            Some(o) => o,
            None => break,
        };
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
                m.young_marked.push(off);
            } else {
                m.marked += 1;
            }
        }
        n += 1;
        crate::old::scan_refs(env, off);
    }
    std::mem::swap(&mut env.work, &mut m.work);
    m.scanned += n;
    n
}

/// 标记完 → 复活带 finalizer 的死对象 → 取清扫快照
fn begin_sweep(env: &mut GcEnv, m: &mut Marker) {
    if env.finalize_armed {
        let mut revived: Vec<u32> = Vec::new();
        for i in 0..env.old.live.len() {
            let off = env.old.live[i];
            unsafe {
                let fl = flags_of(env.base, off);
                if fl & FLAG_MARK == 0
                    && fl & FLAG_FINALIZE != 0
                    && fl & FLAG_FINALIZED == 0
                {
                    set_flags(env.base, off, fl | FLAG_FINALIZED);
                    env.finalize.push(off);
                    m.finalized += 1;
                    revived.push(off);
                }
            }
        }
        if !revived.is_empty() {
            // 复活对象连同子图重新标一遍。复活的通常是极少数，这一步不按预算切。
            std::mem::swap(&mut env.work, &mut m.work);
            env.work.extend(revived);
            let mut guard = 0u64;
            while let Some(off) = env.work.pop() {
                guard += 1;
                assert!(guard < 1 << 32, "复活标记没有收敛（堆坏了？）");
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
                        m.young_marked.push(off);
                    } else {
                        m.marked += 1;
                    }
                }
                crate::old::scan_refs(env, off);
            }
            std::mem::swap(&mut env.work, &mut m.work);
        }
    }

    m.phase = GcPhase::Sweeping;
    m.snapshot = std::mem::take(&mut env.old.live);
    m.los_snapshot = std::mem::take(&mut env.old.los_live);
    m.kept.clear();
    m.los_kept.clear();
    m.cursor = 0;
    m.los_cursor = 0;
    m.in_los = false;
    m.rebuild_cursor = 0;
    m.lists_done = false;
}

/// 清扫阶段：最多处理 `budget` 个对象。返回实际处理了多少。
fn sweep_step(env: &mut GcEnv, m: &mut Marker, budget: usize) -> usize {
    let base = env.base;
    let mut n = 0usize;
    while n < budget {
        if !m.in_los {
            if m.cursor >= m.snapshot.len() {
                m.in_los = true;
                continue;
            }
            let off = m.snapshot[m.cursor];
            m.cursor += 1;
            n += 1;
            unsafe {
                let fl = flags_of(base, off);
                if fl & FLAG_MARK != 0 {
                    set_flags(base, off, fl & !FLAG_MARK);
                    m.kept.push(off);
                } else if fl & FLAG_FINALIZE != 0 && fl & FLAG_FINALIZED == 0 {
                    set_flags(base, off, fl | FLAG_FINALIZED);
                    env.finalize.push(off);
                    m.finalized += 1;
                    m.kept.push(off);
                } else {
                    let size = size_at(base, off, env.layout);
                    env.old.free.push(size, off);
                    env.old.note_free(size, false);
                    env.starts.clear(off);
                    env.weak.on_death(off);
                    m.weak_cleared += 1;
                    m.freed += 1;
                    m.freed_bytes += size as usize;
                }
            }
        } else {
            if m.los_cursor >= m.los_snapshot.len() {
                break;
            }
            let off = m.los_snapshot[m.los_cursor];
            m.los_cursor += 1;
            n += 1;
            unsafe {
                let fl = flags_of(base, off);
                if fl & FLAG_MARK != 0 {
                    set_flags(base, off, fl & !FLAG_MARK);
                    m.los_kept.push(off);
                } else if fl & FLAG_FINALIZE != 0 && fl & FLAG_FINALIZED == 0 {
                    set_flags(base, off, fl | FLAG_FINALIZED);
                    env.finalize.push(off);
                    m.finalized += 1;
                    m.los_kept.push(off);
                } else {
                    let size = size_at(base, off, env.layout);
                    env.old.los_free.push(size, off);
                    env.old.note_free(size, true);
                    env.starts.clear(off);
                    env.weak.on_death(off);
                    m.weak_cleared += 1;
                    m.los_freed += 1;
                    m.freed_bytes += size as usize;
                }
            }
        }
    }

    if m.in_los && m.los_cursor >= m.los_snapshot.len() {
        if !m.lists_done {
            end_sweep_lists(env, m);
            m.lists_done = true;
        }
        // 收尾之后还有一遍 O(live) 的卡表重建 —— 它同样按预算切，
        // 否则「最大单步」会被这一步整个吃掉（1M 老对象 ≈ 8ms）。
        n += rebuild_step(env, m, budget - n);
    }
    n
}

/// 装存活表、清年轻标记位、把卡表清空（重建交给 `rebuild_step`）
fn end_sweep_lists(env: &mut GcEnv, m: &mut Marker) {
    // `env.old.live` 在清扫期间被新分配追加过，要接在幸存者后面
    let mut live = std::mem::take(&mut m.kept);
    live.append(&mut env.old.live);
    env.old.live = live;
    let mut los = std::mem::take(&mut m.los_kept);
    los.append(&mut env.old.los_live);
    env.old.los_live = los;

    let young = std::mem::take(&mut m.young_marked);
    for &off in &young {
        unsafe {
            let fl = flags_of(env.base, off);
            set_flags(env.base, off, fl & !FLAG_MARK);
        }
    }
    m.young_marked = young;
    m.young_marked.clear();

    env.weak.cycle_end();
    env.cards.clear();
    m.snapshot.clear();
    m.los_snapshot.clear();
    m.rebuild_cursor = 0;
}

/// 卡表重建：按预算遍历存活老对象，把「还指向年轻代」的卡重新标脏。
///
/// 重建是必须的：下一轮 minor 的根集就是脏卡，不标脏就会漏掉老→年轻的边。
fn rebuild_step(env: &mut GcEnv, m: &mut Marker, budget: usize) -> usize {
    let live = env.old.live.len();
    let los = env.old.los_live.len();
    let mut n = 0usize;
    while n < budget {
        let i = m.rebuild_cursor;
        if i >= live + los {
            m.phase = GcPhase::Idle;
            m.just_finished = true;
            break;
        }
        let off = if i < live { env.old.live[i] } else { env.old.los_live[i - live] };
        if crate::old::has_young_ref(env, off) {
            env.cards.mark(off);
        }
        m.rebuild_cursor = i + 1;
        n += 1;
    }
    n
}

fn finish_report(env: &mut GcEnv, m: &Marker) -> MajorReport {
    MajorReport {
        freed: m.freed,
        freed_bytes: m.freed_bytes,
        los_freed: m.los_freed,
        kept: env.old.live.len() + env.old.los_live.len(),
        marked: m.marked,
        pause_ns: uptime_ns().saturating_sub(m.started_ns),
        compacted: false,
        finalized: m.finalized,
        weak_cleared: m.weak_cleared,
        moved: 0,
        fragmentation: env.old.fragmentation(),
    }
}
