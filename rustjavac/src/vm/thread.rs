//! Java 线程、对象监视器、停靠点。
//!
//! ## 线程模型（2026-11 去 GIL）
//!
//! 每个 Java 线程 = 一个 OS 线程（`std::thread`），**各自持有一个 `Vm`**
//! （`Vm::fork` 造出来的解释器上下文），各自有 Rust 栈上的帧栈。
//! 线程之间不再串行：需要共享的东西都在 `Shared` 里，且每一条路径都有明确的同步。
//!
//! 旧实现是「一把大锁 + `AtomicPtr<Vm>`」：谁持锁谁独占整个 VM，每 512 条字节码
//! 交还一次。那是 CPython 式 GIL —— 实测 4 线程比串行慢 2.2 倍（票据锁的 condvar
//! 乒乓比干活还贵）。现在**没有全局锁**：
//!
//! | 共享物 | 同步方式 |
//! | --- | --- |
//! | 对象字段 / 数组元素 | `Cell` 上的原子（无锁） |
//! | 对象分配 | 页表 + 原子序号（无锁） |
//! | 类表 | `Arc<ClassStore>` 快照（读不加锁） |
//! | 对象监视器 | 对象头 thin lock（CAS）+ 竞争时膨胀的等待集 |
//! | 静态字段 / 类对象 / indy 状态 | 分组 `Mutex`（都不在字节码热路径上） |
//! | 停靠点 | 全局轮次 + 每线程 park（`gc.rs`） |
//!
//! 阻塞点（sleep / wait / join / monitorenter 竞争）就是普通的 park —— 不再需要
//! 「交还 / 重新获取」锁这一步，因为根本没有锁可交还。
//!
//! ## 监视器语义（JVMS）
//!
//! - `monitorenter` 可重入（同一线程 count+1），被别人持有就阻塞（FIFO 队首）；
//! - `monitorexit` count-1，归零时**移交**给 entry 队首（hand-off，防插队）；
//! - `wait` 必须在持锁时调用，否则 `IllegalMonitorStateException`；它会**完全释放**
//!   监视器（含重入计数）、进条件队列、被 `notify` 搬进 entry 队列后再重新获取，
//!   最后把重入计数恢复成 `wait` 前的值。
//!
//! 状态机与「状态存在哪」（对象头 thin 字 / 膨胀后的等待集）见 [`super::monitors`]。

use super::shared::lk;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::monitors::{thin_count, thin_owner, thin_word, WaitSet, COUNT_MAX, INFLATED};
use super::slot::Slot;
use super::{Vm, VmError};

/// `VMLESS_MON_TRACE=1`：监视器占用账本（句柄 → 当前占着它的线程）。
///
/// 只在这个环境变量开着时才会被初始化；正常跑一个字节码都不多碰。
static MON_OCC: Mutex<Option<HashMap<i32, u64>>> = Mutex::new(None);

// ================================================================ Rust 栈预算
//
// 解释器的每层 Java 调用都是**真的**一层 Rust 栈帧（`execute` → `execute_body`
// → 递归 `execute`），所以「能递归多深」由线程栈大小决定，不是随便拍一个数字。
// 判据直接看**实际栈指针**：`stack_top() - 当前 sp > 预算` 就是快溢出了。
// 这比数深度准 —— 有的帧宽（局部变量多）、有的窄，深度相同的两次调用栈占用不同。

/// 栈顶哨兵：本 OS 线程在入口处记下的 sp（0 = 还没绑定）
thread_local! {
    static STACK_TOP: Cell<usize> = const { Cell::new(0) };
}

/// Java 线程（含主线程）的 Rust 栈大小，`VMLESS_STACK_MB` 可覆盖（默认 24 MB）。
///
/// 为什么要显式给：解释器一层调用比 JIT 重得多（`execute_body` 那个大 match 的
/// 栈帧不小），默认 1 MB 的线程栈只够几十层。HotSpot 默认 `-Xss512k` 能跑到
/// 一万层上下，因为它的解释器帧不是真实 C 栈帧。
///
/// 24 MB 是**实测**定的：`VMLESS_SOE_TRACE=1` 量到每层 ≈ **2128 B**（`R5` 的
/// `deep(0)` 递归），按 3/4 预算 → 约 8800 层可用，落在「8000 层」这个目标上。
/// 这是**虚拟**保留：Windows 按需提交，没递归到那么深就几乎不占物理内存。
pub(crate) fn java_stack_bytes() -> usize {
    static B: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *B.get_or_init(|| {
        std::env::var("VMLESS_STACK_MB")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .map(|mb| mb.clamp(1, 512))
            .unwrap_or(24)
            << 20
    })
}

/// 在**本线程的入口**调一次，记下栈顶。
///
/// 必须在真入口（`thread_start` 的 spawn 闭包 / `run_main` 的大栈线程）调：
/// 绑定得越晚，`top` 越低，算出来的可用空间越保守 —— 保守是安全方向，但会
/// 少掉一大截可用深度。
pub(super) fn bind_stack_top() {
    let anchor = 0u8;
    let sp = std::hint::black_box(&anchor) as *const u8 as usize;
    STACK_TOP.with(|t| t.set(sp));
}

#[inline]
pub(super) fn stack_top() -> usize {
    STACK_TOP.with(|t| t.get())
}

/// 本线程已经用掉多少栈（诊断 / 定 `VMLESS_STACK_MB` 用）
pub(super) fn stack_used() -> usize {
    let anchor = 0u8;
    let sp = std::hint::black_box(&anchor) as *const u8 as usize;
    stack_top().saturating_sub(sp)
}

/// 溢出判据：这一层再压下去还剩多少栈。
#[inline]
pub(super) fn stack_exhausted() -> bool {
    let anchor = 0u8;
    let sp = std::hint::black_box(&anchor) as *const u8 as usize;
    let top = stack_top();
    if top == 0 {
        // 没绑定（不是我们起的线程）→ 按最保守的 1 MB 算，宁可早报错也不要段错误
        return false;
    }
    // 留 1/4 余量：抛 `StackOverflowError` 自己还要走几层（分配异常对象、
    // 抓帧快照、`exc_text` 拼串），临界点上再吃满栈就是真 SIGSEGV，没有 panic。
    top.saturating_sub(sp) > java_stack_bytes() / 4 * 3
}


/// 开关**只在第一次问的时候查环境**。
///
/// 这是 `monitorenter` / `monitorexit` 的热路径：每次锁都做一次 `var_os` 就是每次
/// 锁都拿一遍环境变量表的锁，实测会把无竞争的锁成本翻好几倍。缓存成一个字节的
/// `AtomicU8`（0=没查过，1=开，2=关）之后，热路径上只剩一次 relaxed 读。
static MON_TRACE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

#[inline]
fn mon_trace_on() -> bool {

    match MON_TRACE.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on = std::env::var_os("VMLESS_MON_TRACE").is_some();
            MON_TRACE.store(if on { 1 } else { 2 }, Ordering::Relaxed);
            on
        }
    }
}

/// 进入临界区后核账：已经被**另一个**线程占着就打印现场（互斥坏掉的唯一可靠判据）
fn mon_occ_check(handle: i32, me: u64) {
    let prev = {
        let mut g = MON_OCC.lock().unwrap_or_else(|e| e.into_inner());
        let m = g.get_or_insert_with(HashMap::new);
        m.insert(handle, me)
    };
    if let Some(other) = prev {
        if other != me {
            eprintln!("[mon] DOUBLE ENTER handle={handle} me={me} other={other}");
            for (i, fr) in super::stacktrace::frames_current().iter().take(10).enumerate() {
                eprintln!(
                    "  #{i} {}.{}({}:{})",
                    fr.class.replace('/', "."),
                    fr.method,
                    fr.source.clone().unwrap_or_else(|| "?".into()),
                    fr.line
                );
            }
        }
    }
}

/// 释放时把账本那一格清掉（归零的只有真正退出临界区的那一次）
fn mon_occ_exit(handle: i32, me: u64) {
    let mut g = MON_OCC.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(m) = g.as_mut() {
        if m.get(&handle) == Some(&me) {
            m.remove(&handle);
        }
    }
}

/// 下一个 Java 线程 id（1 留给 main）
static NEXT_TID: AtomicU64 = AtomicU64::new(2);

thread_local! {
    /// 本 OS 线程代表哪个 Java 线程
    static TID: Cell<u64> = const { Cell::new(0) };
}

/// 当前线程的 Java 线程 id（0 = 还没进 VM）
pub(super) fn cur_tid() -> u64 {
    TID.with(|t| t.get())
}

fn set_tid(t: u64) {
    TID.with(|c| c.set(t));
}

/// 主线程登记为 tid = 1（`run_main` 进 VM 时调）
pub(super) fn set_main_tid() {
    set_tid(1);
}

// ---------------------------------------------------------------- 线程停靠点

/// 卡住/唤醒一个 Java 线程用的单槽信号量（经典 Parker）。
///
/// 有 `flag` 而不是裸 condvar：`unpark` 可能发生在 `park` 之前（hand-off 之后
/// 对方还没 park），那样本次 park 必须立刻返回 —— 信号不会丢。
struct Parker {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl Parker {
    fn new() -> Arc<Parker> {
        Arc::new(Parker { flag: Mutex::new(false), cv: Condvar::new() })
    }

    fn unpark(&self) {
        {
            let mut g = self.flag.lock().unwrap_or_else(|e| e.into_inner());
            *g = true;
        }
        self.cv.notify_all();
    }

    fn park(&self) {
        let mut g = self.flag.lock().unwrap_or_else(|e| e.into_inner());
        while !*g {
            g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
        *g = false;
    }

    /// 返回 true = 拿到信号被唤醒；false = 超时
    fn park_timeout(&self, d: Duration) -> bool {
        let t0 = Instant::now();
        let mut g = self.flag.lock().unwrap_or_else(|e| e.into_inner());
        while !*g {
            let left = match d.checked_sub(t0.elapsed()) {
                Some(l) if !l.is_zero() => l,
                _ => return false,
            };
            let (ng, _) = self.cv.wait_timeout(g, left).unwrap_or_else(|e| e.into_inner());
            g = ng;
        }
        *g = false;
        true
    }
}

// ---------------------------------------------------------------- 线程状态（HotSpot 位）

// `Thread.getState()` 是 java.base 的**真字节码**：
//   getState() → threadState() → holder.threadStatus → VM.toThreadState(int)
// 所以这里写进去的必须是 HotSpot 的 JVMTI 位（`VM.toThreadState` 按位判）。
/// 活着的 + 可运行
pub(super) const ST_RUNNABLE: i32 = 0x0001 | 0x0004; // ALIVE | RUNNABLE = 5
/// 等监视器
pub(super) const ST_BLOCKED: i32 = 0x0001 | 0x0400; // ALIVE | BLOCKED_ON_MONITOR_ENTER
/// 无限期等待（wait() / join()）
pub(super) const ST_WAITING: i32 = 0x0001 | 0x0080 | 0x0010; // ALIVE | WAITING | INDEFINITELY
/// 限时等待（wait(t) / join(t) / sleep）
pub(super) const ST_TIMED_WAITING: i32 = 0x0001 | 0x0080 | 0x0020; // ALIVE | WAITING | WITH_TIMEOUT
/// 已终止
pub(super) const ST_TERMINATED: i32 = 0x0002;

#[derive(Clone)]
pub(super) struct ThreadRec {
    pub tid: u64,
    /// 对应的 `java.lang.Thread` 对象句柄（0 = 还没建 / 没有 java.base）
    pub java: i32,
    pub name: String,
    pub daemon: bool,
    pub alive: bool,
    /// HotSpot 位（同时也是 `holder.threadStatus` 的来源）
    pub state: i32,
    parker: Arc<Parker>,
    /// `Thread.interrupt()` 的标志（对应 Java 侧 `interrupted` 字段的语义镜像）
    pub interrupted: bool,
    /// 正在等哪个监视器（诊断 / 死锁报告用）
    pub waiting_on: i32,
    /// `LockSupport.unpark` 留下的**许可**。
    ///
    /// JDK 语义：`unpark` 在目标线程没 park 时记一个许可，下一次 `park` **立刻返回并清掉**；
    /// 连续多次 `unpark` 只算一次。少了它，`park()` 只能靠中断才返回 ——
    /// 而 AQS（`ReentrantLock`/`Condition`/`LinkedBlockingDeque`）全是无限期 park，
    /// 一挂就是整个程序卡死。
    pub permit: bool,
    /// 这条线程的**帧表**（见 `stacktrace`）：栈轨迹 / 死锁报告 / jstack 都从这里读。
    /// 放在 `ThreadRec` 里是为了「别的线程也能拿到它」——帧槽本身只由本线程写，
    /// 读侧走 seqlock。
    pub frames: Arc<super::stacktrace::FrameStack>,
}

// ---------------------------------------------------------------- 停靠频率

/// 每次 `safepoint` 之间的字节码条数。太小伤性能，太大饿死别的线程。
pub(super) const SAFEPOINT_EVERY: u32 = 512;

impl Vm {
    // ------------------------------------------------------------ 线程表

    fn rec(&self, tid: u64) -> Option<ThreadRec> {
        lk(&self.sh.threads).get(&tid).cloned()
    }

    /// 进 VM 的线程登记（main 与 `Thread.start` 都走它）
    pub(super) fn thread_register(&mut self, tid: u64, java: i32, name: &str, daemon: bool) {
        let frames = Arc::new(super::stacktrace::FrameStack::new_for(tid));
        let rec = ThreadRec {
            tid,
            java,
            name: name.to_string(),
            daemon,
            alive: true,
            state: ST_RUNNABLE,
            parker: Parker::new(),
            interrupted: false,
            waiting_on: 0,
            permit: false,
            frames,
        };
        lk(&self.sh.threads).insert(tid, rec);
        self.sh.nthreads.fetch_add(1, Ordering::AcqRel);
        self.set_state(tid, ST_RUNNABLE);
        // 主线程是「在自己身上登记」的，这里顺手绑上帧表；子线程要等它自己的 OS 线程
        // 起来后再绑（`thread_start` 的 spawn 闭包里做）。
        if cur_tid() == tid {
            super::stacktrace::bind_current(&self.sh, tid);
        }
    }

    pub(super) fn thread_is_alive(&self, tid: u64) -> bool {
        self.rec(tid).map(|r| r.alive).unwrap_or(false)
    }

    /// 改线程状态（同时镜像到 `Thread.holder.threadStatus`，`getState()` 读的就是它）
    pub(super) fn set_state(&mut self, tid: u64, st: i32) {
        let obj = match lk(&self.sh.threads).get_mut(&tid) {
            Some(r) => {
                r.state = st;
                r.java
            }
            None => return,
        };
        if obj > 0 {
            let holder = self.get_named(obj, "java/lang/Thread", "holder").as_i32();
            if holder > 0 {
                self.put_named(holder, "java/lang/Thread$FieldHolder", "threadStatus", Slot::I(st));
            }
        }
    }

    fn set_waiting_on(&mut self, tid: u64, handle: i32) {
        if let Some(r) = lk(&self.sh.threads).get_mut(&tid) {
            r.waiting_on = handle;
        }
    }

    /// 当前线程的 `java.lang.Thread` 对象（懒建；没有 java.base 时返回 0）
    pub(super) fn current_thread_obj(&mut self) -> i32 {
        let tid = cur_tid();
        if let Some(r) = lk(&self.sh.threads).get(&tid) {
            if r.java > 0 {
                return r.java;
            }
        }
        if !self.store.has_class("java/lang/Thread") {
            return 0;
        }
        let name = self.rec(tid).map(|r| r.name.clone()).unwrap_or_else(|| "main".to_string());
        let daemon = self.rec(tid).map(|r| r.daemon).unwrap_or(false);
        match self.thread_obj_new(&name, 0, daemon) {
            Ok(h) => {
                if let Some(r) = lk(&self.sh.threads).get_mut(&tid) {
                    r.java = h;
                }
                let st = self.rec(tid).map(|r| r.state).unwrap_or(ST_RUNNABLE);
                self.set_state(tid, st);
                h
            }
            Err(_) => 0,
        }
    }

    // ------------------------------------------------------------ Park / 唤醒

    /// park 一次（超时返回 false）。**不处理 interrupt** —— 由调用方决定。
    ///
    /// 没有锁要交还了（GIL 已经不存在），所以这里就是纯粹的 park。
    pub(super) fn park_block(&mut self, timeout: Option<Duration>) -> bool {
        let tid = cur_tid();
        let parker = match lk(&self.sh.threads).get(&tid) {
            Some(r) => r.parker.clone(),
            None => return false,
        };
        match timeout {
            Some(d) => parker.park_timeout(d),
            // **不能无限期 `park()`**：`LockSupport.park` 的契约本来就允许**伪唤醒**，
            // 而我们的 `unpark` 要靠「Thread 对象句柄 → tid」反查，那条链一旦断掉
            // （对象还没登记、或 tid 表在 GC 期间换过），`park()` 就永远醒不来。
            // AQS（`ReentrantLock`/`Condition`/`LinkedBlockingDeque`）全是无限期 park，
            // 一挂就是整个程序卡死。用 50ms 一片轮转：伪唤醒 → AQS 重查条件 → 该走的就走。
            None => parker.park_timeout(Duration::from_millis(50)),
        }
    }

    pub(super) fn wake(&self, tid: u64) {
        if let Some(r) = lk(&self.sh.threads).get_mut(&tid) {
            // 先记许可再 unpark：`LockSupport.unpark` 在目标还没 park 时也要「记住这一次」，
            // 否则 AQS 的 signal 早于 await 就会丢（典型症状：先 signal 后 await 永不醒）。
            r.permit = true;
            r.parker.unpark();
        }
    }

    /// 停靠点检查：GC 要停世界时线程在这里 park。
    ///
    /// 稳态成本是一条 relaxed 原子读。旧实现在这里交还 / 重抢 GIL，
    /// 那是 4 线程比串行还慢 2.2 倍的直接原因。
    pub(super) fn safepoint(&mut self) {
        self.sh.safepoint.poll();
    }

    // ------------------------------------------------------------ 监视器

    /// 本线程持有的监视器（`count` 0 → 1 时记一笔，线程终止时靠它兜底释放）
    fn held_push(&mut self, handle: i32) {
        if !self.held.contains(&handle) {
            self.held.push(handle);
        }
    }

    fn held_pop(&mut self, handle: i32) {
        if let Some(p) = self.held.iter().position(|h| *h == handle) {
            self.held.remove(p);
        }
    }

    /// 释放本线程还持有的所有锁（线程终止时兜底）
    fn release_held(&mut self) {
        let held: Vec<i32> = self.held.drain(..).collect();
        for h in held {
            let ws = self.inflate(h);
            {
                let mut st = ws.lock();
                st.count = 0;
                st.owner = 0;
            }
            self.mon_grant(&ws);
        }
    }

    /// 把 thin 锁（或空闲状态）**原子地**搬进等待集，返回它。
    ///
    /// 关键在「原子」：`0/(tid,count) → INFLATED` 必须是一次 CAS —— 否则持有者
    /// 正好在这一刻 `monitorexit`（它把字 CAS 回 0），而我们把「0」当成「别人拿着」，
    /// 就会凭空造出一个永不被释放的锁。
    fn inflate(&mut self, handle: i32) -> Arc<WaitSet> {
        let mut g = self.sh.monitors.sets_lock();
        if let Some(ws) = g.get(&handle).cloned() {
            return ws;
        }
        let Some(w) = self.sh.heap.lock_word(handle) else {
            // 字符串句柄（没有对象头）→ 只能直接给个等待集
            let ws = Arc::new(WaitSet::new(0, 0));
            g.insert(handle, ws.clone());
            return ws;
        };
        let (owner, count) = loop {
            let cur = w.load(Ordering::Acquire);
            if cur == INFLATED {
                break (0, 0);
            }
            if w.compare_exchange(cur, INFLATED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                break if cur == 0 { (0, 0) } else { (thin_owner(cur), thin_count(cur)) };
            }
        };
        let ws = Arc::new(WaitSet::new(owner, count));
        g.insert(handle, ws.clone());
        ws
    }

    /// 锁空闲时把所有权**移交**给 entry 队首（不留插队窗口）
    /// 谁持有这个句柄的锁（0 = 没人持有）—— 死锁报告用
    fn monitor_owner(&self, handle: i32) -> Option<u64> {
        if let Some(w) = self.sh.heap.lock_word(handle) {
            let cur = w.load(Ordering::Acquire);
            if cur != INFLATED && cur != 0 {
                return Some(thin_owner(cur));
            }
        }
        self.sh.monitors.lookup(handle).map(|ws| ws.lock().owner).filter(|o| *o != 0)
    }

    fn mon_grant(&mut self, ws: &Arc<WaitSet>) {
        let mut st = ws.lock();
        if st.owner != 0 {
            return;
        }
        if let Some(t) = st.entry.pop_front() {
            st.owner = t;
            st.count = 1;
            drop(st);
            self.wake(t);
        }
    }

    /// 本线程此刻是否持有这个句柄的锁
    fn holds_word(&self, handle: i32, me: u64) -> bool {
        if handle == 0 {
            return false;
        }
        if let Some(w) = self.sh.heap.lock_word(handle) {
            let cur = w.load(Ordering::Acquire);
            if cur != INFLATED && cur != 0 {
                return thin_owner(cur) == me;
            }
        }
        self.sh.monitors.lookup(handle).map(|ws| ws.lock().owner == me).unwrap_or(false)
    }

    /// `monitorenter`
    /// `VMLESS_MON_TRACE=1` 的诊断外壳：拿到锁之后核一遍「这个句柄此刻是不是已经
    /// 被别的线程占着」。互斥坏掉时**没有任何异常**（只是少加了几个数），
    /// 所以只能靠这种主动取证把现场打出来 —— 判据是 `DOUBLE ENTER` 那一行。
    pub(super) fn monitor_enter(&mut self, handle: i32) -> Result<(), VmError> {
        let r = self.monitor_enter_inner(handle);
        if r.is_ok() && mon_trace_on() {
            mon_occ_check(handle, cur_tid());
        }
        r
    }

    fn monitor_enter_inner(&mut self, handle: i32) -> Result<(), VmError> {
        if handle == 0 {
            return Err(self.java_exception(
                "java/lang/NullPointerException",
                "monitorenter 的对象是 null",
            ));
        }
        let me = cur_tid();
        // 1) thin 快路径：一次 CAS（未竞争）/ 一次自增（重入）
        if let Some(w) = self.sh.heap.lock_word(handle) {
            loop {
                let cur = w.load(Ordering::Acquire);
                if cur == 0 {
                    if w.compare_exchange(0, thin_word(me, 1), Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        self.held_push(handle);
                        return Ok(());
                    }
                    continue;
                }
                if cur != INFLATED && thin_count(cur) < COUNT_MAX && thin_owner(cur) == me {
                    if w.compare_exchange(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                        return Ok(());
                    }
                    continue;
                }
                break;
            }
        }
        // 2) 慢路径：膨胀成等待集，排队等移交
        let ws = self.inflate(handle);
        loop {
            {
                let mut st = ws.lock();
                if st.owner == 0 {
                    st.owner = me;
                    st.count = 1;
                    drop(st);
                    self.held_push(handle);
                    return Ok(());
                }
                if st.owner == me {
                    if self.held.contains(&handle) {
                        st.count += 1; // 真重入
                    } else {
                        self.held_push(handle); // 移交所得：count 已经是 1
                    }
                    return Ok(());
                }
                if !st.entry.contains(&me) && !st.cond.contains(&me) {
                    st.entry.push_back(me);
                }
            }
            self.set_state(me, ST_BLOCKED);
            self.set_waiting_on(me, handle);
            // monitorenter 不响应 interrupt（JVMS：synchronized 阻塞不可中断）
            self.park_block(None);
            self.set_state(me, ST_RUNNABLE);
            self.set_waiting_on(me, 0);
        }
    }

    /// `monitorexit`
    pub(super) fn monitor_exit(&mut self, handle: i32) -> Result<(), VmError> {
        if handle == 0 {
            return Err(self.java_exception(
                "java/lang/NullPointerException",
                "monitorexit 的对象是 null",
            ));
        }
        let me = cur_tid();
        if let Some(w) = self.sh.heap.lock_word(handle) {
            loop {
                let cur = w.load(Ordering::Acquire);
                if cur == 0 || cur == INFLATED {
                    break;
                }
                if thin_owner(cur) != me {
                    return Err(self.java_exception(
                        "java/lang/IllegalMonitorStateException",
                        "current thread is not owner",
                    ));
                }
                if thin_count(cur) <= 1 {
                    if w.compare_exchange(cur, 0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                        self.held_pop(handle);
                        if mon_trace_on() {
                            mon_occ_exit(handle, me);
                        }
                        return Ok(());
                    }
                    continue;
                }
                if w.compare_exchange(cur, cur - 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                    return Ok(());
                }
            }
        }
        let ws = self.inflate(handle);
        let mut st = ws.lock();
        if st.owner != me || st.count == 0 {
            return Err(self.java_exception(
                "java/lang/IllegalMonitorStateException",
                "current thread is not owner",
            ));
        }
        st.count -= 1;
        if st.count > 0 {
            return Ok(());
        }
        st.owner = 0;
        drop(st);
        self.held_pop(handle);
        if mon_trace_on() {
            mon_occ_exit(handle, me);
        }
        self.mon_grant(&ws);
        Ok(())
    }

    /// 当前线程是否持有这把锁（`Thread.holdsLock`）
    pub(super) fn monitor_holds(&self, handle: i32) -> bool {
        self.holds_word(handle, cur_tid())
    }

    /// `Object.wait` 的 VM 实现（`wait0(J)V` 落到这里）
    pub(super) fn monitor_wait(
        &mut self,
        handle: i32,
        timeout: Option<Duration>,
    ) -> Result<(), VmError> {
        let me = cur_tid();
        // wait 必须膨胀（条件队列只在等待集里）
        let ws = self.inflate(handle);
        let saved = {
            let st = ws.lock();
            if st.owner != me || st.count == 0 {
                return Err(self.java_exception(
                    "java/lang/IllegalMonitorStateException",
                    "current thread is not owner",
                ));
            }
            st.count
        };
        // 1) 完全释放监视器（重入计数会记下来，醒来后恢复），并移交出去
        {
            let mut st = ws.lock();
            st.owner = 0;
            st.count = 0;
            st.cond.push_back(me);
        }
        self.held_pop(handle);
        self.mon_grant(&ws);
        self.set_state(me, if timeout.is_some() { ST_TIMED_WAITING } else { ST_WAITING });
        self.set_waiting_on(me, handle);

        let t0 = Instant::now();
        let mut interrupted = false;
        let mut timed_out = false;
        loop {
            let left = timeout.map(|d| d.saturating_sub(t0.elapsed()));
            if let Some(l) = left {
                if l.is_zero() {
                    timed_out = true;
                }
            }
            if !timed_out {
                let woke = self.park_block(left);
                if !woke {
                    timed_out = true;
                }
            }
            let (owns, in_cond) = {
                let st = ws.lock();
                (st.owner == me, st.cond.contains(&me))
            };
            if owns {
                break;
            }
            let intr = lk(&self.sh.threads).get(&me).map(|r| r.interrupted).unwrap_or(false);
            if interrupted {
                // 已经在收尾：只等拿到锁
                self.park_block(None);
                continue;
            }
            if intr {
                interrupted = true;
            }
            if interrupted || timed_out {
                // 退出条件队列 → 排到 entry 队尾等锁（拿不到锁不能返回）
                {
                    let mut st = ws.lock();
                    if in_cond {
                        st.cond.retain(|t| *t != me);
                        st.entry.push_back(me);
                    }
                }
                self.set_state(me, ST_BLOCKED);
                self.mon_grant(&ws);
                self.park_block(None);
                continue;
            }
            // 虚假唤醒：还在条件队列里就继续等
            self.set_state(me, if timeout.is_some() { ST_TIMED_WAITING } else { ST_WAITING });
        }
        // 2) 重新拿到监视器：把重入计数恢复成 wait 之前的值
        {
            let mut st = ws.lock();
            st.count = saved;
        }
        self.held_push(handle);
        self.set_state(me, ST_RUNNABLE);
        self.set_waiting_on(me, 0);
        if interrupted {
            self.clear_interrupt(me);
            return Err(self.java_exception("java/lang/InterruptedException", ""));
        }
        Ok(())
    }

    /// `Object.notify` / `notifyAll`（**要求持锁**，Java 侧语义）
    pub(super) fn monitor_notify(&mut self, handle: i32, all: bool) -> Result<(), VmError> {
        let me = cur_tid();
        if !self.holds_word(handle, me) {
            return Err(self.java_exception(
                "java/lang/IllegalMonitorStateException",
                "current thread is not owner",
            ));
        }
        self.notify_unchecked(handle, all);
        Ok(())
    }

    /// 唤醒条件队列（**不检查持锁**）：线程终止时 join 的 notifyAll 走这条
    fn notify_unchecked(&mut self, handle: i32, all: bool) {
        let ws = self.inflate(handle);
        let mut moved: Vec<u64> = Vec::new();
        {
            let mut st = ws.lock();
            if all {
                moved.extend(st.cond.drain(..));
            } else if let Some(t) = st.cond.pop_front() {
                moved.push(t);
            }
            for t in &moved {
                st.entry.push_back(*t);
            }
        }
        if moved.is_empty() {
            return;
        }
        self.mon_grant(&ws);
        let owner = ws.lock().owner;
        for t in moved {
            // 被唤醒的线程要醒过来去「等锁」那一步（否则它还在 wait 里 park 着）
            if owner != t {
                self.wake(t);
            }
        }
    }


    // ------------------------------------------------------------ 线程生命周期

    /// `Thread.start()`：起一个 OS 线程去跑 `target.run()`
    pub(super) fn thread_start(&mut self, thread_obj: i32) -> Result<(), VmError> {
        let status = self.thread_status(thread_obj);
        if status != 0 {
            return Err(self.java_exception(
                "java/lang/IllegalThreadStateException",
                "thread already started",
            ));
        }
        let holder = self.get_named(thread_obj, "java/lang/Thread", "holder").as_i32();
        // `daemon` / `priority` 挂在 `holder`（FieldHolder）上，不在 Thread 对象上 ——
        // 直接拿 Thread 去读会读到同名下标的别的字段（这里曾经踩过）
        let daemon = self.get_named(holder, "java/lang/Thread$FieldHolder", "daemon").as_i32() != 0;
        let name = {
            let h = self.get_named(thread_obj, "java/lang/Thread", "name").as_i32();
            if h != 0 {
                self.obj_to_string(h)
            } else {
                "Thread".to_string()
            }
        };
        let tid = NEXT_TID.fetch_add(1, Ordering::SeqCst);
        // eetop != 0 是 `Thread.isAlive()` 的判据（真字节码 `alive()` 读的就是它）
        self.put_named(thread_obj, "java/lang/Thread", "eetop", Slot::L(0x1000 + tid as i64));
        self.thread_register(tid, thread_obj, &name, daemon);
        self.set_state(tid, ST_RUNNABLE);

        ensure_deadlock_watchdog();
        // 子线程入口：**自己的解释器上下文**（共享 `Shared`，执行状态全新）
        let mut child = self.fork();
        // 解释器每层 Java 调用是一层真实 Rust 栈帧：默认 1~2 MB 的线程栈只够跑几十层，
        // 任何正常递归（快排 / 树遍历 / 解析器）都会撞上限。这里按 `VMLESS_STACK_MB`
        // 给足（默认 8 MB），并在闭包第一行记下栈顶供 `stack_exhausted()` 用。
        let spawned = std::thread::Builder::new()
            .name(format!("java-{tid}"))
            .stack_size(java_stack_bytes())
            .spawn(move || {
                bind_stack_top();
                set_tid(tid);
                // 帧表绑到**这个** OS 线程上（登记是在父线程做的，TLS 只能在子线程自己这里设）
                super::stacktrace::bind_current(&child.sh, tid);
                child.sh.safepoint.register();
                let r = child.invoke_thread_run(thread_obj);
                let exc = child.thread_finish(tid, r);
                child.sh.safepoint.unregister();
                if let Some(text) = exc {
                    eprintln!("{text}");
                }
            });
        if spawned.is_err() {
            // 起不来线程：按 JVM 语义，`Thread.start()` 失败 → OutOfMemoryError
            return Err(self.java_exception(
                "java/lang/OutOfMemoryError",
                "unable to create native thread",
            ));
        }
        Ok(())
    }

    /// 子线程里跑 `Thread.run()`（虚分派到子类的 run）
    fn invoke_thread_run(&mut self, thread_obj: i32) -> Result<Slot, VmError> {
        let desc = "()V";
        self.ensure_init("java/lang/Thread")?;
        // 虚分派：必须从**接收者的运行时类**开始沿继承链找 `run`，
        // 而不是写死 `java/lang/Thread`。否则 `class MyT extends Thread { void run(){…} }`
        // 会落到 `Thread.run()` 的默认实现（里面 target 为 null，什么都不做）——
        // 线程体静默不执行，且不报错。
        let start = self
            .obj_class_name(thread_obj)
            .unwrap_or_else(|| "java/lang/Thread".to_string());
        let target = self
            .store
            .resolve_method(&start, "run", desc)
            .or_else(|| self.store.resolve_method("java/lang/Thread", "run", desc));
        if let Some((ci, mi)) = target {
            self.depth += 1;
            let r = self.execute(ci, mi, desc, &[Slot::I(thread_obj)], true);
            self.depth -= 1;
            r
        } else {
            Ok(Slot::I(0))
        }
    }

    /// 线程收尾：释放它仍持有的锁、置 TERMINATED、叫醒 `join` 的人。
    /// 返回要打到 stderr 的「未捕获异常」文本（`Exception in thread "x" …`）。
    pub(super) fn thread_finish(&mut self, tid: u64, r: Result<Slot, VmError>) -> Option<String> {
        let exc = match r {
            Ok(_) => None,
            Err(e) => match e.exc {
                Some(h) => {
                    let name = self.rec(tid).map(|t| t.name.clone()).unwrap_or_default();
                    Some(format!("Exception in thread \"{name}\" {}", self.exc_text(h)))
                }
                None => {
                    let name = self.rec(tid).map(|t| t.name.clone()).unwrap_or_default();
                    Some(format!("Exception in thread \"{name}\" {}", e.msg))
                }
            },
        };
        // 1) 把还挂在这个线程名下的锁全部释放（Java 代码不该这样，但锁不能永久泄漏）
        self.release_held();
        // 2) 置终止 + eetop=0（isAlive 立刻变 false）
        let obj = self.rec(tid).map(|t| t.java).unwrap_or(0);
        self.set_state(tid, ST_TERMINATED);
        if obj > 0 {
            self.put_named(obj, "java/lang/Thread", "eetop", Slot::L(0));
        }
        if let Some(t) = lk(&self.sh.threads).get_mut(&tid) {
            t.alive = false;
            t.waiting_on = 0;
        }
        self.sh.nthreads.fetch_sub(1, Ordering::AcqRel);
        // 3) 叫醒所有 join 在这个 Thread 对象上的线程（HotSpot 在 ensure_join 里 do_notify_all）
        if obj > 0 {
            self.notify_unchecked(obj, true);
        }
        if std::env::var_os("VMLESS_WDN_TRACE").is_some() {
            let name = self.rec(tid).map(|t| t.name.clone()).unwrap_or_default();
            eprintln!("[fin] tid={tid} name={name} exc={:?}", exc.as_deref());
        }
        exc
    }

    /// 主线程收尾（它的未捕获异常由 `run_main` 自己格式化）
    pub(super) fn thread_terminate_main(&mut self) {
        self.release_held();
        self.set_state(1, ST_TERMINATED);
        if let Some(t) = lk(&self.sh.threads).get_mut(&1) {
            t.alive = false;
        }
        self.sh.nthreads.fetch_sub(1, Ordering::AcqRel);
    }

    /// 主线程返回后：等所有**非 daemon** 线程跑完（JVM 语义），再让进程退出
    pub(super) fn wait_non_daemon(&mut self) {
        // `VMLESS_WDN_TRACE=1`：每秒打一次「还在等谁」。`wait_non_daemon` 是 1ms 轮询的
        // 死等循环 —— 进程「挂在这儿」时外部看不出在等哪条线程，这个开关直接点名。
        let trace = std::env::var_os("VMLESS_WDN_TRACE").is_some();
        let mut last = Instant::now();
        loop {
            let pending: Vec<u64> = lk(&self.sh.threads)
                .values()
                .filter(|t| t.alive && !t.daemon && t.tid != cur_tid())
                .map(|t| t.tid)
                .collect();
            if pending.is_empty() {
                if trace {
                    eprintln!("[wdn] 全部非 daemon 线程已结束，退出");
                }
                return;
            }
            if trace && last.elapsed() >= Duration::from_secs(1) {
                last = Instant::now();
                let names: Vec<String> = lk(&self.sh.threads)
                    .values()
                    .filter(|t| pending.contains(&t.tid))
                    .map(|t| format!("{}#{} state={:#x}", t.name, t.tid, t.state))
                    .collect();
                eprintln!("[wdn] 仍在等: {names:?}");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// `Thread.interrupt()`
    pub(super) fn thread_interrupt(&mut self, target: i32) {
        let tid = lk(&self.sh.threads).values().find(|t| t.java == target).map(|t| t.tid);
        if let Some(tid) = tid {
            if let Some(r) = lk(&self.sh.threads).get_mut(&tid) {
                r.interrupted = true;
            }
            if target > 0 {
                self.put_named(target, "java/lang/Thread", "interrupted", Slot::I(1));
            }
            let st = self.rec(tid).map(|r| r.state).unwrap_or(0);
            // 睡眠/等待中的线程：叫醒它，让它自己抛 InterruptedException
            if st == ST_WAITING || st == ST_TIMED_WAITING {
                self.wake(tid);
            }
        }
    }

    /// 清掉中断标志：Rust 侧镜像与 Java 侧 `Thread.interrupted` 字段**必须一起清**
    /// （`Thread.isInterrupted()` 读的是字段；异常投递时 HotSpot 也清字段）
    fn clear_interrupt(&mut self, tid: u64) {
        if let Some(r) = lk(&self.sh.threads).get_mut(&tid) {
            r.interrupted = false;
        }
        let obj = self.rec(tid).map(|r| r.java).unwrap_or(0);
        if obj > 0 {
            self.put_named(obj, "java/lang/Thread", "interrupted", Slot::I(0));
        }
    }

    /// `Unsafe.park(isAbsolute, time)`：`LockSupport` 的底座（AQS / 并发容器全靠它）
    ///
    /// 允许**伪唤醒**（`LockSupport.park` 的契约允许「无缘无故返回」），所以无限期 park
    /// 用有限时间片轮转：万一某处 unpark 丢了，线程也不会永久挂死。
    pub(super) fn thread_park(&mut self, timeout: Option<Duration>) -> Result<(), VmError> {
        let me = cur_tid();
        const CAP: Duration = Duration::from_millis(100);
        // 许可：`unpark` 在没 park 时留下的那一个。有许可就**立刻返回并清掉**。
        if self.take_permit(me) {
            return Ok(());
        }
        let deadline = timeout.map(|d| Instant::now() + d);
        loop {
            if lk(&self.sh.threads).get(&me).map(|r| r.interrupted).unwrap_or(false) {
                return Ok(()); // 中断：LockSupport 的语义是「返回」，标志留给调用方看
            }
            let slice = match deadline {
                Some(dl) => {
                    let now = Instant::now();
                    if now >= dl {
                        return Ok(());
                    }
                    (dl - now).min(CAP)
                }
                None => CAP,
            };
            self.set_state(me, if timeout.is_some() { ST_TIMED_WAITING } else { ST_WAITING });
            self.park_block(Some(slice));
            self.set_state(me, ST_RUNNABLE);
            // unpark 到达（或分片超时的伪唤醒）→ 返回。`LockSupport.park` 允许伪唤醒，
            // 调用方（AQS）会重查条件再决定要不要继续 park。
            if self.take_permit(me) || timeout.is_none() {
                return Ok(());
            }
        }
    }

    /// 取走一个 park 许可（有则清掉并返回 true）
    fn take_permit(&self, tid: u64) -> bool {
        let mut t = lk(&self.sh.threads);
        match t.get_mut(&tid) {
            Some(r) if r.permit => {
                r.permit = false;
                true
            }
            _ => false,
        }
    }

    /// `Unsafe.unpark(thread)`（null = 当前线程）
    pub(super) fn thread_unpark(&mut self, target: i32) {
        if target == 0 {
            self.wake(cur_tid());
            return;
        }
        // 锁纪律：先把 tid 取出来（分号处 guard 就 drop），再调 `wake()` ——
        // `wake()` 内部还要 take `sh.threads`，写在 `if let` 里会把同一把锁再取一次，
        // 自己锁自己（不可重入 Mutex，表现为永久挂死、无 panic）。
        let tid = lk(&self.sh.threads).values().find(|t| t.java == target).map(|t| t.tid);
        if let Some(tid) = tid {
            self.wake(tid);
        }
    }

    /// `Thread.sleep`
    pub(super) fn thread_sleep(&mut self, d: Duration) -> Result<(), VmError> {
        let me = cur_tid();
        let mut slept = Duration::ZERO;
        loop {
            let intr = lk(&self.sh.threads).get(&me).map(|r| r.interrupted).unwrap_or(false);
            if intr {
                self.clear_interrupt(me);
                return Err(self.java_exception("java/lang/InterruptedException", "sleep interrupted"));
            }
            if slept >= d {
                return Ok(());
            }
            let left = d - slept;
            self.set_state(me, ST_TIMED_WAITING);
            self.set_waiting_on(me, 0);
            let t0 = Instant::now();
            self.park_block(Some(left));
            slept += t0.elapsed();
            self.set_state(me, ST_RUNNABLE);
        }
    }

    /// `Thread.join` 的 VM 兜底（真字节码 join 走 wait/notify，用不上它）
    pub(super) fn thread_join(&mut self, target: i32, timeout: Option<Duration>) -> Result<(), VmError> {
        let me = cur_tid();
        let tid = lk(&self.sh.threads).values().find(|t| t.java == target).map(|t| t.tid);
        let t0 = Instant::now();
        loop {
            let alive = match tid {
                Some(t) => self.thread_is_alive(t),
                None => false,
            };
            if !alive {
                return Ok(());
            }
            let intr = lk(&self.sh.threads).get(&me).map(|r| r.interrupted).unwrap_or(false);
            if intr {
                self.clear_interrupt(me);
                return Err(self.java_exception("java/lang/InterruptedException", ""));
            }
            if let Some(d) = timeout {
                if t0.elapsed() >= d {
                    return Ok(());
                }
            }
            self.set_state(me, if timeout.is_some() { ST_TIMED_WAITING } else { ST_WAITING });
            self.set_waiting_on(me, target);
            let left = timeout.map(|d| d.saturating_sub(t0.elapsed()));
            self.park_block(Some(left.unwrap_or(Duration::from_millis(20))));
            self.set_state(me, ST_RUNNABLE);
            self.set_waiting_on(me, 0);
        }
    }

    // ------------------------------------------------------------ Thread 对象上的字段

    pub(super) fn thread_status(&self, thread_obj: i32) -> i32 {
        let holder = self.get_named(thread_obj, "java/lang/Thread", "holder").as_i32();
        if holder > 0 {
            self.get_named(holder, "java/lang/Thread$FieldHolder", "threadStatus").as_i32()
        } else {
            0
        }
    }

    /// 按「类 + 字段名」取一个实例字段（跨继承链）
    pub(super) fn get_named(&self, obj: i32, class: &str, name: &str) -> Slot {
        if obj <= 0 {
            return Slot::I(0);
        }
        let Some(i) = self.store.field_index(class, name) else { return Slot::I(0) };
        self.sh.heap.get(obj, i).unwrap_or(Slot::I(0))
    }

    pub(super) fn put_named(&mut self, obj: i32, class: &str, name: &str, v: Slot) {
        if obj <= 0 {
            return;
        }
        let Some(i) = self.store.field_index(class, name) else { return };
        self.sh.heap.set(obj, i, v);
    }

    /// 造一个 `java.lang.Thread`（`Thread.<init>` 由 VM 接管：真构造器要拖进
    /// ThreadGroup / ClassLoader 一整条链，而这里只关心线程语义）
    pub(super) fn thread_obj_new(&mut self, name: &str, task: i32, daemon: bool) -> Result<i32, VmError> {
        let h = self.alloc_vmobj("java/lang/Thread");
        self.thread_fill_impl(h, name, task, daemon)?;
        Ok(h)
    }

    /// 自动起名：`Thread-0`、`Thread-1`……
    pub(super) fn next_thread_name(&mut self) -> String {
        let n = self.sh.thread_seq.load(Ordering::Acquire);
        self.sh.thread_seq.fetch_add(1, Ordering::AcqRel);
        format!("Thread-{n}")
    }

    /// 把 `Thread` 对象的字段填好（构造器接管体走这里）。
    /// daemon/priority 从**当前线程**继承 —— 这是 Java 的语义（`FieldHolder(..., parent.isDaemon())`）。
    pub(super) fn thread_fill(&mut self, h: i32, name: &str, task: i32) -> Result<(), VmError> {
        let daemon = self.rec(cur_tid()).map(|r| r.daemon).unwrap_or(false);
        self.thread_fill_impl(h, name, task, daemon)
    }

    /// 全局共享的「根」`ThreadGroup` 对象句柄（首次使用时创建，缓存在静态字段表里）。
    ///
    /// 真 JDK 里每个线程都挂在一个 `ThreadGroup` 上；我们不分组建，
    /// 但**不能给 null** —— `Thread.getThreadGroup()` 会把 null 直接交给调用方，
    /// 而 `InnocuousThread` / `Cleaner` 会拿它去 `synchronized`。
    fn root_thread_group(&mut self) -> i32 {
        let key = ("java/lang/ThreadGroup".to_string(), "$vmless_root".to_string());
        if let Some(Slot::I(h)) = lk(&self.sh.statics).get(&key).copied() {
            if h > 0 {
                return h;
            }
        }
        if !self.store.has_class("java/lang/ThreadGroup") {
            return 0;
        }
        let h = self.alloc_vmobj("java/lang/ThreadGroup");
        if h <= 0 {
            return 0;
        }
        let nm = self.str_new("main".to_string());
        let _ = self.put_named(h, "java/lang/ThreadGroup", "name", Slot::I(nm));
        // `parent` **必须是 null**（根组没有父组）。
        // 千万不要填成自己：`InnocuousThread.<clinit>` 是
        // `while (tg != null) { parent = UNSAFE.getReference(tg, PARENT); if (parent == null) break; tg = parent; }`
        // —— 自指会让这个「爬到根组」的循环永远出不来，症状是整个程序卡死。
        let _ = self.put_named(h, "java/lang/ThreadGroup", "parent", Slot::I(0));
        lk(&self.sh.statics).insert(key, Slot::I(h));
        h
    }

    fn thread_fill_impl(&mut self, h: i32, name: &str, task: i32, daemon: bool) -> Result<(), VmError> {
        self.ensure_init("java/lang/Thread")?;
        let fh = self.alloc_vmobj("java/lang/Thread$FieldHolder");
        // `Thread$FieldHolder.group` **必须**是个真对象：
        // `Thread.getThreadGroup()` 直接返回它，而 `InnocuousThread.<clinit>` /
        // `Cleaner.create()` 会拿它去 `synchronized` —— 为 null 就是 NPE，
        // 后果是 `FileCleanable.register` 炸、**每一个 FileInputStream/FileOutputStream 构造器都失败**。
        let grp = self.root_thread_group();
        if self.store.has_class("java/lang/ThreadGroup") {
            self.put_named(fh, "java/lang/Thread$FieldHolder", "group", Slot::I(grp));
        }
        self.put_named(fh, "java/lang/Thread$FieldHolder", "task", Slot::I(task));
        self.put_named(fh, "java/lang/Thread$FieldHolder", "stackSize", Slot::L(0));
        self.put_named(fh, "java/lang/Thread$FieldHolder", "priority", Slot::I(5));
        self.put_named(fh, "java/lang/Thread$FieldHolder", "daemon", Slot::I(i32::from(daemon)));
        self.put_named(fh, "java/lang/Thread$FieldHolder", "threadStatus", Slot::I(0));
        let nh = self.str_new(name.to_string());
        // `interrupt()` 会 `synchronized (interruptLock)`，必须是个真对象
        let lock = self.alloc_vmobj("java/lang/Object");
        self.put_named(h, "java/lang/Thread", "holder", Slot::I(fh));
        self.put_named(h, "java/lang/Thread", "name", Slot::I(nh));
        self.put_named(h, "java/lang/Thread", "eetop", Slot::L(0));
        self.put_named(h, "java/lang/Thread", "interruptLock", Slot::I(lock));
        self.put_named(h, "java/lang/Thread", "tid", Slot::L(0));
        Ok(())
    }

    // ------------------------------------------------------------ 诊断：线程转储 / 死锁

    /// 线程清单（名字/状态/持有的锁/在等谁）—— `jstack` 那套的输出源。
    pub fn debug_threads(&self) -> Vec<(u64, String, i32, bool, Vec<i32>, i32)> {
        // 持有的锁清单只包含**膨胀过**的监视器（thin 持有的锁在对象头里，没有全局索引）。
        // 死锁判定不靠这张表：它按「等待的句柄 → 那个句柄的持有者」现算（见 debug_deadlocks）。
        let inflated = self.sh.monitors.snapshot();
        let mut v: Vec<_> = lk(&self.sh.threads)
            .values()
            .map(|t| {
                let mut locks: Vec<i32> = inflated
                    .iter()
                    .filter(|(_, owner, count, _, _)| *owner == t.tid && *count > 0)
                    .map(|(h, _, _, _, _)| *h)
                    .collect();
                locks.sort_unstable();
                (t.tid, t.name.clone(), t.state, t.daemon, locks, t.waiting_on)
            })
            .collect();
        v.sort_by_key(|x| x.0);
        v
    }

    /// 在「持有 / 等待」图上找环（死锁）。返回环上所有线程 id。
    pub fn debug_deadlocks(&self) -> Vec<Vec<u64>> {
        let threads = self.debug_threads();
        let mut owner_of: HashMap<i32, u64> = HashMap::new();
        for (tid, _, _, _, locks, _) in &threads {
            for h in locks {
                owner_of.insert(*h, *tid);
            }
        }
        // thin 持有的锁不在快照里：对每个「有人正在等」的句柄，直接从对象头/等待集读持有者
        for (_, _, _, _, _, waiting) in &threads {
            if *waiting != 0 && !owner_of.contains_key(waiting) {
                if let Some(o) = self.monitor_owner(*waiting) {
                    owner_of.insert(*waiting, o);
                }
            }
        }
        // waiter 图：tid → 它在等谁
        let mut edges: HashMap<u64, u64> = HashMap::new();
        for (tid, _, _, _, _, waiting) in &threads {
            if *waiting != 0 {
                if let Some(o) = owner_of.get(waiting) {
                    // 自环不算死锁（`join` 期间线程自己持有目标 Thread 的监视器是正常的）
                    if *o != *tid {
                        edges.insert(*tid, *o);
                    }
                }
            }
        }
        let mut out: Vec<Vec<u64>> = Vec::new();
        let mut seen: HashMap<u64, usize> = HashMap::new();
        for start in edges.keys().copied().collect::<Vec<_>>() {
            if seen.contains_key(&start) {
                continue;
            }
            let mut path: Vec<u64> = Vec::new();
            let mut cur = start;
            loop {
                if seen.contains_key(&cur) {
                    if let Some(pos) = path.iter().position(|x| *x == cur) {
                        out.push(path[pos..].to_vec());
                    }
                    break;
                }
                seen.insert(cur, 1);
                path.push(cur);
                match edges.get(&cur) {
                    Some(n) => cur = *n,
                    None => break,
                }
            }
        }
        out
    }

    /// 死锁自检（watchdog 每 tick 调一次）：连续若干次「所有存活线程都不在 RUNNABLE」
    /// **并且**等待图里有环，才认定是死锁 —— 单纯卡住（比如全在 sleep）不报。
    pub(super) fn deadlock_watchdog_tick(&mut self) -> Option<String> {
        let (n_alive, all_blocked) = {
            let g = lk(&self.sh.threads);
            let mut n = 0usize;
            let mut blocked = true;
            for t in g.values().filter(|t| t.alive) {
                n += 1;
                if t.state == ST_RUNNABLE {
                    blocked = false;
                }
            }
            (n, blocked)
        };
        let stalled = n_alive > 0 && all_blocked;
        if stalled {
            self.sh.stalled.fetch_add(1, Ordering::AcqRel);
        } else {
            self.sh.stalled.store(0, Ordering::Release);
        }
        if self.sh.stalled.load(Ordering::Acquire) < 4 {
            return None;
        }
        let cycles = self.debug_deadlocks();
        if cycles.is_empty() {
            return None;
        }
        Some(self.deadlock_text(&cycles))
    }

    /// `jstack` 风格的**全线程**转储（含真实帧栈）。`VMLESS_STACK_DUMP=<秒>` 时由
    /// 看门狗周期打印；也可以从 `run_main` 的错误路径直接调。
    pub fn debug_stacks(&self) -> String {
        let threads = self.debug_threads();
        let mut out = String::new();
        for (tid, name, state, daemon, locks, waiting) in &threads {
            out.push_str(&format!(
                "\"{}\" tid={} state={:#x}{}{}\n",
                name,
                tid,
                *state,
                if *daemon { " daemon" } else { "" },
                if *waiting != 0 { format!(" waiting_on={}", waiting) } else { String::new() }
            ));
            if !locks.is_empty() {
                out.push_str(&format!("  holds: {:?}\n", locks));
            }
            for line in super::stacktrace::stack_text(self, *tid, 32) {
                out.push_str(&line);
                out.push('\n');
            }
            if super::stacktrace::frames_of(self, *tid).is_empty() {
                out.push_str("\t<empty stack>\n");
            }
            out.push('\n');
        }
        out
    }

    /// `jstack -l` 风格的死锁报告（进程级诊断输出的拼装在这里）
    fn deadlock_text(&self, cycles: &[Vec<u64>]) -> String {
        let threads = self.debug_threads();
        let name_of = |h: i32| -> String {
            threads
                .iter()
                .find(|t| t.4.contains(&h))
                .map(|t| t.1.clone())
                .unwrap_or_default()
        };
        let class_of = |h: i32| -> String {
            self.class_of(h).map(|c| c.replace('/', ".")).unwrap_or_else(|| "java.lang.Object".into())
        };
        let mut out = String::from("Found one Java-level deadlock:\n");
        out.push_str("=============================\n");
        for cyc in cycles {
            for tid in cyc {
                if let Some(t) = threads.iter().find(|t| t.0 == *tid) {
                    out.push_str(&format!(
                        "\"{}\":\n  waiting to lock monitor <0x{:x}> (a {}),\n  which is held by \"{}\"\n\n",
                        t.1,
                        t.5 as u64,
                        class_of(t.5),
                        name_of(t.5)
                    ));
                }
            }
        }
        out.push_str("Java stack information for the threads listed above:\n");
        out.push_str("===================================================\n");
        // 每一环上的线程给出**真实帧栈**（栈顶若干帧，jstack 风格）：
        // 「谁在等谁」有了，「为什么走到那里」也有了。
        for cyc in cycles {
            for tid in cyc {
                if let Some(t) = threads.iter().find(|t| t.0 == *tid) {
                    out.push_str(&format!("\"{}\":\n", t.1));
                    for line in super::stacktrace::stack_text(self, *tid, 16) {
                        out.push_str(&line);
                        out.push('\n');
                    }
                    out.push('\n');
                }
            }
        }
        out.push_str(&format!("Found {} deadlock.\n", cycles.len()));
        out
    }
}

/// 进程里唯一的共享世界（看门狗线程要拿它造自己的 `Vm`）。
///
/// 只存一份：一个进程只有一个 VM（`run_main` 也只有一个）。
static SHARED_REG: Mutex<Option<Arc<super::shared::Shared>>> = Mutex::new(None);

/// VM 建好时登记（`Vm::from_store` 里调）
pub(super) fn register_shared(sh: &Arc<super::shared::Shared>) {
    if let Ok(mut g) = SHARED_REG.lock() {
        *g = Some(Arc::clone(sh));
    }
}

fn shared_reg() -> Option<Arc<super::shared::Shared>> {
    SHARED_REG.lock().ok().and_then(|g| g.clone())
}

/// 死锁 watchdog：所有线程都阻塞且等待图有环时，打一份 jstack 风格的报告并退出。
///
/// 只在 `Thread.start` 第一次发生时启动；`VMLESS_NO_DEADLOCK_REPORT=1` 可关掉
/// （关掉后行为与 HotSpot 一致：进程一直挂着等人来 attach）。
fn spawn_deadlock_watchdog() {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if std::env::var_os("VMLESS_NO_DEADLOCK_REPORT").is_some() {
        return;
    }
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        // `VMLESS_STACK_DUMP=<秒>`：每隔这么久往 stderr 打一份全线程栈（jstack 的替身，
        // 排查 park/锁挂死用）。`VMLESS_THREAD_DUMP_MS` 只看线程表（老开关，保留）。
        let stack_iv = std::env::var("VMLESS_STACK_DUMP")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .map(Duration::from_secs_f64);
        let mut last = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(250));
            // 看门狗是一个独立线程：给它自己的解释器上下文（只需要 `Shared`，不跑字节码）
            let Some(sh) = shared_reg() else { return };
            let mut vm = Vm::from_shared(sh);
            let report = vm.deadlock_watchdog_tick();
            if std::env::var_os("VMLESS_THREAD_DUMP_MS").is_some() {
                eprintln!("[dump] threads={:?} cycles={:?}", vm.debug_threads(), vm.debug_deadlocks());
            }
            if let Some(iv) = stack_iv {
                if last.elapsed() >= iv {
                    last = Instant::now();
                    eprint!("{}", vm.debug_stacks());
                }
            }
            if let Some(text) = report {
                eprint!("{text}");
                std::process::exit(1);
            }
        }
    });
}

/// 让 `thread_start` 起一次 watchdog（第一次起线程时）
pub(super) fn ensure_deadlock_watchdog() {
    spawn_deadlock_watchdog();
}

