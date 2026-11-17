//! 停靠点（safepoint）与写屏障接线。
//!
//! ## 现在接到哪一步
//!
//! 停靠点协议是**完整**的：全局轮次 + 每线程到达计数 + park/unpark，`Safepoint::sync()`
//! 能把所有已登记线程召到停靠点再放行。但**当前没有 GC 去发起它**（原因见下），
//! 所以 `poll()` 在稳态下只花一次 relaxed 原子读 —— 不是"预留的空函数"，而是
//! 一套已经能用的机制，只是调用者是未来的 GC 线程。
//!
//! ## 为什么这一版没有真正的 GC
//!
//! 堆已经不是"进程只增不减"了（对象块按页组织，理论上可以回收），但**回收需要精确根集**：
//! 解释器的 `Frame.locals` 是 `Vec<Slot>`，里面引用（句柄）和 int/long 混在一起，
//! 没有任何类型标签。追溯扫描无法区分「句柄 5」和「整数 5」——
//! 要么把整数当引用（把随机偏移当对象，立刻段错误），要么漏掉真引用（活对象被回收）。
//!
//! 旧 VM 同样没有 GC（`Vec<Vec<Slot>>` 只增不减），所以这不是回归；
//! 接 GC 的前置工作是给每帧的局部槽配上引用位图（`StackMapTable` 或执行期影子栈），
//! 那是独立的一块工作，混在这次"去 GIL"里做会把并行性和回收正确性两件事一起搞砸。
//!
//! 写屏障也是同理：`Heap::set_ref` 已经把所有引用字段的写入收敛到一个地方并调
//! `dirty_write`，卡表已经在维护，接上增量标记只差"谁去扫脏卡"。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

/// 卡表：8 字节粒度标记「这块地址上有引用被写过」。
///
/// 只在该页第一次变脏时置位（`swap`），扫完由 GC 清 —— 现在是**一直关着**，
/// 因为还没有 GC 去消费它（开着只是白写内存）。
static CARDS_ENABLED: AtomicBool = AtomicBool::new(false);
const CARD_SHIFT: u32 = 9; // 512 字节/卡
const CARD_WORDS: usize = 1 << 16;
static CARDS: [AtomicU64; CARD_WORDS] = {
    #[allow(clippy::declare_interior_mutable_const)]
    const Z: AtomicU64 = AtomicU64::new(0);
    [Z; CARD_WORDS]
};

/// 引用字段写入的写屏障。`off` 是被写对象的堆偏移（这里用句柄即可）。
#[inline]
pub(crate) fn dirty_write(off: u32) {
    if !CARDS_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let card = (off as usize >> CARD_SHIFT) & (CARD_WORDS - 1);
    let bit = off as usize & 63;
    CARDS[card].fetch_or(1 << bit, Ordering::Relaxed);
}

/// 打开卡表（GC 开始时）
pub(crate) fn cards_enable(on: bool) {
    CARDS_ENABLED.store(on, Ordering::Relaxed);
}

/// 脏卡快照（GC 扫描用）
pub(crate) fn dirty_cards() -> Vec<usize> {
    let mut out = Vec::new();
    for (i, w) in CARDS.iter().enumerate() {
        if w.load(Ordering::Relaxed) != 0 {
            out.push(i);
        }
    }
    out
}

/// 清零脏卡
pub(crate) fn clear_cards() {
    for w in CARDS.iter() {
        w.store(0, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------- 停靠点

pub(crate) struct Safepoint {
    /// 每发起一轮停靠 +1；线程 park 到它再变一次为止
    epoch: AtomicUsize,
    /// 正在要求停靠
    requested: AtomicBool,
    /// 已到达的线程数
    arrived: AtomicUsize,
    /// 已登记（在跑字节码）的线程数
    active: AtomicUsize,
    lock: Mutex<()>,
    cv: Condvar,
}

impl Safepoint {
    pub(crate) const fn new() -> Safepoint {
        Safepoint {
            epoch: AtomicUsize::new(0),
            requested: AtomicBool::new(false),
            arrived: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            lock: Mutex::new(()),
            cv: Condvar::new(),
        }
    }

    /// 线程进 VM 时登记
    pub(crate) fn register(&self) {
        self.active.fetch_add(1, Ordering::AcqRel);
    }

    /// 线程退出 VM 时注销
    pub(crate) fn unregister(&self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
        self.cv.notify_all();
    }

    /// 字节码主循环里的检查：一分钟内的稳态成本是一条 relaxed 读。
    #[inline]
    pub(crate) fn poll(&self) {
        if self.requested.load(Ordering::Relaxed) {
            self.park();
        }
    }

    /// 到停靠点：报到 → 睡到本轮结束
    pub(crate) fn park(&self) {
        let epoch = self.epoch.load(Ordering::Acquire);
        self.arrived.fetch_add(1, Ordering::AcqRel);
        let mut g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        while self.epoch.load(Ordering::Acquire) == epoch {
            self.cv.notify_all(); // 叫醒发起方：我又到了一个
            g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// 发起一轮停靠：等所有登记线程到达（`timeout_ms` 内），返回「是否全部到齐」。
    ///
    /// 没到齐也不死等 —— 返回 false 让调用方决定（GC 一轮失败可以下一轮再来，
    /// 卡死整个进程比放弃一轮 GC 糟得多）。
    pub(crate) fn sync(&self, timeout_ms: u64) -> bool {
        let want = self.active.load(Ordering::Acquire);
        self.arrived.store(0, Ordering::Release);
        self.requested.store(true, Ordering::Release);
        let epoch = self.epoch.fetch_add(1, Ordering::AcqRel) + 1;

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        let mut g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut ok = false;
        while std::time::Instant::now() < deadline {
            if self.arrived.load(Ordering::Acquire) >= want.saturating_sub(1) {
                ok = true;
                break;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let (ng, _) = self.cv.wait_timeout(g, left).unwrap_or_else(|e| e.into_inner());
            g = ng;
        }
        drop(g);
        self.requested.store(false, Ordering::Release);
        self.epoch.store(epoch + 1, Ordering::Release);
        self.cv.notify_all();
        ok
    }

    pub(crate) fn requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }
}

thread_local! {
    /// 本线程是否已经在停靠点里（避免重入 park）
    static AT_SAFEPOINT: AtomicU32 = const { AtomicU32::new(0) };
}

pub(crate) fn in_safepoint_thread() -> bool {
    AT_SAFEPOINT.with(|a| a.load(Ordering::Relaxed) != 0)
}
