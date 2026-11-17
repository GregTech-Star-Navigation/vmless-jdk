//! 监视器：对象头里的 thin lock（CAS 快路径）+ 竞争时膨胀出的等待集。
//!
//! ## 状态机（对象头的 `lock: AtomicU32`）
//!
//! ```text
//!   0                    未锁，也没膨胀过
//!   tid << 8 | count     thin 持有者与重入计数（count 1..=255，tid 从 1 起）
//!   INFLATED (1)         已膨胀：拥有者/计数/两个队列全在 WaitSet 里
//! ```
//!
//! 膨胀是**单向**的（和 HotSpot 一样不做 deflate）：一旦某个对象真竞争过，它就一直在
//! 等待集里管，省掉「两条真相」对账的错。未竞争过的锁（Java 绝大多数）永远只付
//! 一次 CAS —— 这正是 `synchronized` 计数（验收 T1，4 线程 × 10 万次）能真正并行的原因，
//! 旧实现里每次 `monitorenter` 都要拿 GIL。
//!
//! ## 为什么要 `Arc<WaitSet>`
//!
//! 等待中的线程必须先**原子地**放弃锁并进入条件队列，否则会丢唤醒（T3 的判据就是
//! 「notifyAll 不丢」）。所以队列的修改与 `park` 之间不能有窗口：
//! 拿 `Arc<WaitSet>` 出来（表锁只护 map 本身）→ 在 `m` 上做状态变更 → 释放 `m` → park。
//! 队列表的锁从不被长时间持有，也不与 `park` 嵌套。

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

/// 膨胀哨兵：对象头里的值（tid 从 1 起，所以 1 不会被当成合法持有者）
pub(crate) const INFLATED: u32 = 1;

/// thin 字里重入计数占的位数
pub(crate) const COUNT_BITS: u32 = 8;
pub(crate) const COUNT_MAX: u32 = (1 << COUNT_BITS) - 1;

#[inline]
pub(crate) fn thin_word(tid: u64, count: u32) -> u32 {
    ((tid as u32) << COUNT_BITS) | (count & COUNT_MAX)
}

#[inline]
pub(crate) fn thin_owner(word: u32) -> u64 {
    (word >> COUNT_BITS) as u64
}

#[inline]
pub(crate) fn thin_count(word: u32) -> u32 {
    word & COUNT_MAX
}

/// 膨胀后的监视器状态
#[derive(Default)]
pub(crate) struct MonState {
    /// 拥有者线程 id（0 = 空闲）
    pub(crate) owner: u64,
    /// 重入计数
    pub(crate) count: u32,
    /// 等锁队列（monitorenter 竞争 / 被 notify 唤醒后等重新获取）
    pub(crate) entry: VecDeque<u64>,
    /// 条件队列（`Object.wait`）
    pub(crate) cond: VecDeque<u64>,
}

pub(crate) struct WaitSet {
    pub(crate) m: Mutex<MonState>,
}

impl WaitSet {
    pub(crate) fn new(owner: u64, count: u32) -> WaitSet {
        WaitSet { m: Mutex::new(MonState { owner, count, ..Default::default() }) }
    }

    #[inline]
    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, MonState> {
        self.m.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 句柄 → 膨胀后的监视器。未膨胀的对象不在这张表里（它们的锁在对象头上）。
#[derive(Default)]
pub(crate) struct MonitorTable {
    sets: Mutex<HashMap<i32, Arc<WaitSet>>>,
}

impl MonitorTable {
    pub(crate) fn new() -> MonitorTable {
        MonitorTable { sets: Mutex::new(HashMap::new()) }
    }

    /// 取（必要时建）一个监视器的等待集。
    ///
    /// 调用者必须在**已经持有该对象锁**的前提下调它（把锁的状态搬进来）：
    /// `owner`/`count` 是 thin 字里读到的值。
    pub(crate) fn inflate(&self, handle: i32, owner: u64, count: u32) -> Arc<WaitSet> {
        let mut g = self.sets.lock().unwrap_or_else(|e| e.into_inner());
        g.entry(handle).or_insert_with(|| Arc::new(WaitSet::new(owner, count))).clone()
    }

    /// 直接拿表锁（膨胀时要在持锁期间做 thin → INFLATED 的 CAS）
    pub(crate) fn sets_lock(&self) -> std::sync::MutexGuard<'_, HashMap<i32, Arc<WaitSet>>> {
        self.sets.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn lookup(&self, handle: i32) -> Option<Arc<WaitSet>> {
        self.sets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&handle)
            .cloned()
    }

    /// 诊断快照：`(句柄, 拥有者, 计数, entry 队列, cond 队列)`
    pub(crate) fn snapshot(&self) -> Vec<(i32, u64, u32, Vec<u64>, Vec<u64>)> {
        let g = self.sets.lock().unwrap_or_else(|e| e.into_inner());
        g.iter()
            .map(|(h, ws)| {
                let st = ws.lock();
                (*h, st.owner, st.count, st.entry.iter().copied().collect(), st.cond.iter().copied().collect())
            })
            .collect()
    }

    /// 丢弃一个句柄的等待集（线程终止时把还挂在上面的队列清干净）
    pub(crate) fn forget(&self, handle: i32) {
        self.sets.lock().unwrap_or_else(|e| e.into_inner()).remove(&handle);
    }
}
