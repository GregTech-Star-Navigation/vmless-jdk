//! 采样层 —— 把 attach / perfdata 的数据整理成 UI 直接能画的结构。
//!
//! # 线程模型（决定了这里为什么长这样）
//!
//! **UI 线程绝不碰网络。** 目标 VM 卡死时 `TcpStream` 上的 2s 超时会把窗口冻住 2 秒，
//! 拖窗口都是白屏 —— 那是"能用"和"不能用"的差别。所以：
//!
//! ```text
//!   worker 线程                UI 线程
//!   ───────────                ───────
//!   attach / 采样  ──写──▶  Arc<Mutex<Snapshot>>  ◀──读──  定时器
//!          └── PostMessage(WM_APP_REFRESH) ──▶ 重绘 + 更新状态栏
//! ```
//!
//! 用 `Arc<Mutex<_>>` 而不是 `PostMessage` 直接携带 `Box<Snapshot>`：后者的消息一旦
//! 因为窗口先销毁而丢弃，那个 `Box` 就泄漏了；前者由 `Arc` 自己管生命周期，窗口退出
//! 只需 `stop` 置位，worker 自然结束。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vmless_attach::client::Client;
use vmless_attach::heap::ClassHisto;
use vmless_attach::perf::{self, PerfData};
use vmless_attach::protocol::Op;
use vmless_attach::registry;

/// `jstat -gc` 的 19 列，顺序与真 jstat 一致。
pub const GC_COLS: [&str; 19] = [
    "S0C", "S1C", "S0U", "S1U", "EC", "EU", "OC", "OU", "MC", "MU", "CCSC", "CCSU", "YGC", "YGCT", "FGC",
    "FGCT", "CGC", "CGCT", "GCT",
];

/// 每列的取值类型：0 = 字节，1 = 次数，2 = 纳秒（显示成秒）。
pub const GC_KIND: [u8; 19] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 1, 2, 1, 2, 2];

/// 采样间隔（毫秒）。UI 与状态栏共用这一个常量。
pub const INTERVAL_MS: u64 = 1000;
/// 断线后的重连间隔。
pub const RECONNECT_MS: u64 = 3000;

#[derive(Clone, Debug)]
pub struct ThreadInfo {
    pub name: String,
    pub state: String,
    pub java_id: u64,
    pub daemon: bool,
    /// `java.lang.Thread.State: X` 与各栈帧（已按 `jstack` 的缩进整理好）
    pub stack: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct HistoRow {
    pub class_name: String,
    pub instances: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub seq: u64,
    pub pid: u32,
    pub connected: bool,
    pub retry: u64,
    pub error: Option<String>,
    pub vm_name: String,
    pub vm_version: String,
    pub main_class: String,

    // 概览
    pub heap_used: i64,
    pub heap_cap: i64,
    pub heap_max: i64,
    pub nonheap_used: i64,
    pub nonheap_cap: i64,
    pub meta_used: i64,
    pub meta_cap: i64,
    pub ccs_used: i64,
    pub ccs_cap: i64,
    pub threads_live: i64,
    pub threads_daemon: i64,
    pub threads_peak: i64,
    pub threads_started: i64,
    pub classes_loaded: i64,
    pub classes_unloaded: i64,
    pub uptime_ms: i64,
    pub cpu_ns: u64,
    pub cpu_pct: f64,

    // 内存页
    pub gc: [Option<i64>; 19],
    pub collectors: Vec<(String, i64, i64)>,

    // 线程页 / 类页
    pub threads: Option<Arc<Vec<ThreadInfo>>>,
    pub histo: Option<Arc<Vec<HistoRow>>>,
    pub histo_at: u64,
}

impl Snapshot {
    pub fn new(pid: u32) -> Snapshot {
        Snapshot { pid, ..Default::default() }
    }

    /// 19 个计数器的显示文本。`-` = 该计数器缺失（与真 jstat 一致）。
    pub fn gc_text(&self, i: usize) -> String {
        if i >= 19 {
            return "-".into();
        }
        match self.gc[i] {
            None => "-".to_string(),
            Some(v) => match GC_KIND[i] {
                0 => format_kb(v),
                1 => format!("{}", v),
                _ => format!("{:.3}", v as f64 / 1e9),
            },
        }
    }

    /// 一条曲线的纵坐标（0..1 比例）：`num/den`，分母为 0 时返回 0。
    pub fn ratio(num: Option<i64>, den: Option<i64>) -> f64 {
        match (num, den) {
            (Some(n), Some(d)) if d > 0 => (n as f64 / d as f64).clamp(0.0, 1.0),
            _ => 0.0,
        }
    }
}

pub fn format_kb(bytes: i64) -> String {
    format!("{:.1}", bytes as f64 / 1024.0)
}

pub fn format_size(bytes: i64) -> String {
    let b = bytes as f64;
    if b >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GB", b / (1024.0 * 1024.0 * 1024.0))
    } else if b >= 1024.0 * 1024.0 {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    } else if b >= 1024.0 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

/// 毫秒 → `1d 02:03:04` / `02:03:04` / `3.2s`
pub fn format_duration(ms: i64) -> String {
    if ms < 0 {
        return "-".into();
    }
    let total = ms / 1000;
    let d = total / 86400;
    let h = (total % 86400) / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if d > 0 {
        format!("{}d {:02}:{:02}:{:02}", d, h, m, s)
    } else if h > 0 {
        format!("{:02}:{:02}:{:02}", h, m, s)
    } else {
        format!("{:02}:{:02}", m, s)
    }
}

// ---------------------------------------------------------------------------
// 共享状态
// ---------------------------------------------------------------------------

pub struct Shared {
    pub snap: Mutex<Snapshot>,
    pub stop: AtomicBool,
    /// 位 0 = 立刻重取直方图，位 1 = 立刻重取线程栈
    pub req: AtomicU32,
    /// 重连次数（状态栏用）
    pub retries: AtomicU64,
}

pub const REQ_HISTO: u32 = 1;
pub const REQ_THREADS: u32 = 2;

impl Shared {
    pub fn new(pid: u32) -> Arc<Shared> {
        Arc::new(Shared {
            snap: Mutex::new(Snapshot::new(pid)),
            stop: AtomicBool::new(false),
            req: AtomicU32::new(0),
            retries: AtomicU64::new(0),
        })
    }

    pub fn read<R>(&self, f: impl FnOnce(&Snapshot) -> R) -> R {
        let g = self.snap.lock().unwrap_or_else(|e| e.into_inner());
        f(&g)
    }

    pub fn request(&self, bits: u32) {
        self.req.fetch_or(bits, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// 计数器提取
// ---------------------------------------------------------------------------

fn sum_spaces(p: &PerfData) -> (i64, i64, i64) {
    // (used, capacity, maxCapacity) —— 逐代逐空间累加；`sun.gc.generation.{g}.spaces` 是空间数
    let mut used = 0i64;
    let mut cap = 0i64;
    let mut max = 0i64;
    let gens = p.get_i64("sun.gc.policy.generations").unwrap_or(2).clamp(0, 8);
    for g in 0..gens {
        let spaces = p.get_i64(&format!("sun.gc.generation.{}.spaces", g)).unwrap_or(0).clamp(0, 8);
        for s in 0..spaces {
            let b = format!("sun.gc.generation.{}.space.{}", g, s);
            used += p.get_i64(&format!("{}.used", b)).unwrap_or(0);
            cap += p.get_i64(&format!("{}.capacity", b)).unwrap_or(0);
            max += p.get_i64(&format!("{}.maxCapacity", b)).unwrap_or(0);
        }
    }
    (used, cap, max)
}

fn gc19(p: &PerfData) -> [Option<i64>; 19] {
    let g = |n: &str| p.get_i64(n);
    let hz = g("sun.os.hrt.frequency").filter(|v| *v > 0).unwrap_or(1_000_000_000);
    let t = |i: usize| g(&format!("sun.gc.collector.{}.time", i)).map(|v| v);
    let tsum = {
        let a = t(0).unwrap_or(0);
        let b = t(1).unwrap_or(0);
        let c = t(2).unwrap_or(0);
        if t(0).is_none() && t(1).is_none() && t(2).is_none() {
            None
        } else {
            Some(a + b + c)
        }
    };
    let _ = hz; // ns 计数器的原始单位就是纳秒，不需要再按 hz 归一
    [
        g("sun.gc.generation.0.space.1.capacity"),
        g("sun.gc.generation.0.space.2.capacity"),
        g("sun.gc.generation.0.space.1.used"),
        g("sun.gc.generation.0.space.2.used"),
        g("sun.gc.generation.0.space.0.capacity"),
        g("sun.gc.generation.0.space.0.used"),
        g("sun.gc.generation.1.space.0.capacity"),
        g("sun.gc.generation.1.space.0.used"),
        g("sun.gc.metaspace.capacity"),
        g("sun.gc.metaspace.used"),
        g("sun.gc.compressedclassspace.capacity"),
        g("sun.gc.compressedclassspace.used"),
        g("sun.gc.collector.0.invocations"),
        t(0),
        g("sun.gc.collector.1.invocations"),
        t(1),
        g("sun.gc.collector.2.invocations"),
        t(2),
        tsum,
    ]
}

fn collectors(p: &PerfData) -> Vec<(String, i64, i64)> {
    let n = p.get_i64("sun.gc.policy.collectors").unwrap_or(0).clamp(0, 8);
    let mut v = Vec::new();
    for i in 0..n {
        let name = p.get_str(&format!("sun.gc.collector.{}.name", i)).unwrap_or("?").to_string();
        let inv = p.get_i64(&format!("sun.gc.collector.{}.invocations", i)).unwrap_or(0);
        let time = p.get_i64(&format!("sun.gc.collector.{}.time", i)).unwrap_or(0);
        v.push((name, inv, time));
    }
    v
}

// ---------------------------------------------------------------------------
// 线程栈文本解析
// ---------------------------------------------------------------------------

/// 解析 `Thread.print` 的文本输出。
///
/// 格式见 `vmless_attach::threads::ThreadDump::render`：线程头以 `"` 开头，
/// 栈帧以 `\tat ` 开头，`- locked` / `- waiting on` 缩进一级。
pub fn parse_thread_dump(text: &str) -> Vec<ThreadInfo> {
    let mut out: Vec<ThreadInfo> = Vec::new();
    let mut i = 0usize;
    let lines: Vec<&str> = text.lines().collect();
    while i < lines.len() {
        let line = lines[i].trim_end();
        i += 1;
        let Some(rest) = line.strip_prefix('"') else { continue };
        let (name, tail) = match rest.find('"') {
            Some(p) => (&rest[..p], &rest[p + 1..]),
            None => (rest, ""),
        };
        let toks: Vec<&str> = tail.split_whitespace().collect();
        let mut java_id = 0u64;
        let mut daemon = false;
        for t in &toks {
            if let Some(n) = t.strip_prefix('#') {
                java_id = n.parse().unwrap_or(0);
            }
            if *t == "daemon" {
                daemon = true;
            }
        }
        // 状态：最后一个 `[...]` 之前那个 token（`runnable` / `waiting on condition` …）
        let mut state = String::from("UNKNOWN");
        if let Some(pos) = toks.iter().position(|t| t.starts_with('[')) {
            if pos > 0 {
                state = toks[pos - 1].to_uppercase();
            }
        }
        let mut stack: Vec<String> = Vec::new();
        while i < lines.len() {
            let l = lines[i].trim_end();
            if l.starts_with('"') || (l.starts_with("Full thread dump") || l.starts_with("Found ")) {
                break;
            }
            let lt = l.trim();
            if let Some(r) = lt.strip_prefix("java.lang.Thread.State: ") {
                // 只取值给列表用，不要再压一行到栈里 —— 头部已经打印了状态
                state = r.trim().to_string();
            } else if let Some(r) = lt.strip_prefix("at ") {
                stack.push(format!("  at {}", r));
            } else if let Some(r) = lt.strip_prefix("- locked ") {
                stack.push(format!("    locked {}", r));
            } else if let Some(r) = lt.strip_prefix("- waiting on ") {
                stack.push(format!("    waiting on {}", r));
            } else if lt.starts_with("- None") || lt.starts_with("- parking") {
                stack.push(format!("    {}", lt.trim_start_matches("- ")));
            }
            i += 1;
        }
        out.push(ThreadInfo { name: name.to_string(), state, java_id, daemon, stack });
    }
    out
}

// ---------------------------------------------------------------------------
// worker
// ---------------------------------------------------------------------------

/// 起采样线程。`pre` = 主线程已经建好的连接（有就省一次握手）。
pub fn spawn_worker(
    pid: u32,
    pre: Option<Client>,
    shared: Arc<Shared>,
    hwnd: crate::win::HWND,
    notify: u32,
    interval_ms: u64,
) {
    // HWND 是裸指针，不实现 Send；这里转成 isize 传过去再转回来。
    // 窗口句柄在进程内有效，worker 只在窗口存活期间 PostMessage。
    let target = hwnd as isize;
    std::thread::Builder::new()
        .name(format!("sample-{}", pid))
        .spawn(move || worker_loop(pid, pre, shared, target as crate::win::HWND, notify, interval_ms))
        .ok();
}

fn sleep_abortable(shared: &Shared, ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn publish(shared: &Shared, hwnd: crate::win::HWND, notify: u32, snap: Snapshot) {
    {
        let mut g = shared.snap.lock().unwrap_or_else(|e| e.into_inner());
        *g = snap;
    }
    if !hwnd.is_null() && !shared.stop.load(Ordering::SeqCst) {
        unsafe {
            crate::win::PostMessageW(hwnd, notify, 0, 0);
        }
    }
}

fn worker_loop(
    pid: u32,
    pre: Option<Client>,
    shared: Arc<Shared>,
    hwnd: crate::win::HWND,
    notify: u32,
    interval_ms: u64,
) {
    let mut client: Option<Client> = pre;
    let mut last_cpu: Option<(u64, Instant)> = None;
    let mut tick: u64 = 0;
    // 每 N 个 tick 取一次较贵的数据：线程栈每 2s，直方图每 5s
    const THREADS_EVERY: u64 = 2;
    const HISTO_EVERY: u64 = 5;

    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        if client.is_none() {
            match Client::attach(pid, 1500) {
                Ok(c) => {
                    // 握手信息（VM 名 / 版本 / 主类）只在这里拿得到，抄进快照给界面用
                    let mut s2 = Snapshot::new(pid);
                    s2.vm_name = c.info.vm_name.clone();
                    s2.vm_version = c.info.vm_version.clone();
                    s2.main_class = c.info.main_class.clone();
                    client = Some(c);
                    let mut g = shared.snap.lock().unwrap_or_else(|e| e.into_inner());
                    g.vm_name = s2.vm_name;
                    g.vm_version = s2.vm_version;
                    g.main_class = s2.main_class;
                    drop(g);
                }
                Err(e) => {
                    let n = shared.retries.fetch_add(1, Ordering::SeqCst) + 1;
                    let mut s = Snapshot::new(pid);
                    s.connected = false;
                    s.retry = n;
                    s.error = Some(format!("{}", e));
                    publish(&shared, hwnd, notify, s);
                    sleep_abortable(&shared, RECONNECT_MS);
                    continue;
                }
            }
        }

        let started = Instant::now();
        tick += 1;
        let req = shared.req.swap(0, Ordering::SeqCst);

        let prev: Snapshot = shared.read(|s| s.clone());
        let mut next = prev.clone();
        next.seq += 1;
        next.pid = pid;
        next.retry = 0;

        // ① 计数器（直接读 perfdata 文件 —— 与真 jstat 同一条路）
        let path = registry::perf_path(pid);
        match perf::read(&path) {
            Ok(p) => {
                let (used, cap, max) = sum_spaces(&p);
                next.heap_used = used;
                next.heap_cap = cap;
                next.heap_max = if max > 0 { max } else { cap };
                next.meta_used = p.get_i64("sun.gc.metaspace.used").unwrap_or(0);
                next.meta_cap = p.get_i64("sun.gc.metaspace.capacity").unwrap_or(0);
                next.ccs_used = p.get_i64("sun.gc.compressedclassspace.used").unwrap_or(0);
                next.ccs_cap = p.get_i64("sun.gc.compressedclassspace.capacity").unwrap_or(0);
                next.nonheap_used = next.meta_used + next.ccs_used;
                next.nonheap_cap = next.meta_cap + next.ccs_cap;
                next.threads_live = p.get_i64("java.threads.live").unwrap_or(0);
                next.threads_daemon = p.get_i64("java.threads.daemon").unwrap_or(0);
                next.threads_peak = p.get_i64("java.threads.livePeak").unwrap_or(0);
                next.threads_started = p.get_i64("java.threads.started").unwrap_or(0);
                next.classes_loaded = p.get_i64("java.cls.loadedClasses").unwrap_or(0)
                    + p.get_i64("java.cls.sharedLoadedClasses").unwrap_or(0);
                next.classes_unloaded = p.get_i64("java.cls.unloadedClasses").unwrap_or(0);
                next.uptime_ms = p.get_i64("sun.rt.applicationTime").map(|v| v / 1_000_000).unwrap_or(0);
                next.gc = gc19(&p);
                next.collectors = collectors(&p);
                next.error = None;
                next.connected = true;
            }
            Err(e) => {
                // perfdata 读不到不代表连接断了（可能是恰好写一半），connectivity 由 ping 判
                if next.error.is_none() {
                    next.error = Some(format!("读 perfdata 失败: {}", e));
                }
            }
        }

        // ② CPU 时间（来自操作系统，不依赖 VM 侧计数器）
        if let Some((u, k)) = crate::win::process_cpu_ticks(pid) {
            let total_ns = (u + k) * 100; // FILETIME 单位 100ns
            next.cpu_ns = total_ns;
            if let Some((prev_cpu, prev_at)) = last_cpu {
                let dt = started.duration_since(prev_at).as_secs_f64();
                if dt > 0.05 && total_ns >= prev_cpu {
                    let pct = ((total_ns - prev_cpu) as f64 / 1e9) / dt * 100.0;
                    next.cpu_pct = if pct.is_finite() { pct } else { 0.0 };
                }
            }
            last_cpu = Some((total_ns, started));
        }

        // ③ ping：判活。失败 = 连接断开，丢掉 client 下一轮重连。
        let mut disconnected: Option<String> = None;
        if let Some(c) = client.as_mut() {
            match c.ping() {
                Ok(_) => {}
                Err(e) => disconnected = Some(format!("{}", e)),
            }
        }
        match disconnected {
            Some(msg) => {
                client = None;
                let n = shared.retries.fetch_add(1, Ordering::SeqCst) + 1;
                next.connected = false;
                next.retry = n;
                next.error = Some(msg);
            }
            None => next.connected = true,
        }

        // ④ 线程栈 / ⑤ 类直方图
        if next.connected {
            let want_threads = req & REQ_THREADS != 0 || tick % THREADS_EVERY == 0 || next.threads.is_none();
            let want_histo = req & REQ_HISTO != 0 || tick % HISTO_EVERY == 0 || next.histo.is_none();
            if want_threads || want_histo {
                if let Some(c) = client.as_mut() {
                    if want_threads {
                        match c.command("Thread.print") {
                            Ok(t) => {
                                let body = vmless_attach::strip_pid_header(&t);
                                let v = parse_thread_dump(&body);
                                if !v.is_empty() {
                                    next.threads = Some(Arc::new(v));
                                }
                            }
                            Err(e) => {
                                if next.error.is_none() {
                                    next.error = Some(format!("Thread.print 失败: {}", e));
                                }
                            }
                        }
                    }
                    if want_histo {
                        match c.request_bin(Op::HeapHisto, &[]) {
                            Ok(body) => {
                                if let Some(mut hs) = ClassHisto::from_wire(&body) {
                                    hs.sort_by(|a, b| b.instances.cmp(&a.instances));
                                    let rows: Vec<HistoRow> = hs
                                        .into_iter()
                                        .map(|h| HistoRow {
                                            class_name: h.class_name,
                                            instances: h.instances,
                                            bytes: h.bytes,
                                        })
                                        .collect();
                                    next.histo = Some(Arc::new(rows));
                                    next.histo_at = tick;
                                }
                            }
                            Err(e) => {
                                if next.error.is_none() {
                                    next.error = Some(format!("GC.class_histogram 失败: {}", e));
                                }
                            }
                        }
                    }
                }
            }
        }

        publish(&shared, hwnd, notify, next);

        let spent = started.elapsed().as_millis() as u64;
        sleep_abortable(&shared, interval_ms.saturating_sub(spent).max(50));
    }
}

/// 取本机所有可 attach 的 VM：`(pid, 命令行)`。读 perfdata，不建连接。
pub fn list_vms() -> Vec<(u32, String, i64)> {
    let mut v = Vec::new();
    for pid in vmless_attach::tool::list_local_vms() {
        let path = registry::perf_path(pid);
        let Ok(p) = perf::read(&path) else { continue };
        let cmd = p
            .get_str("java.property.sun.java.command")
            .or_else(|| p.get_str("java.property.sun.java.command.line"))
            .unwrap_or("?")
            .to_string();
        let up = p.get_i64("sun.rt.applicationTime").map(|v| v / 1_000_000).unwrap_or(0);
        // 有 perfdata 但没有 attach 端点的（真 HotSpot）连不上，别列出来
        if registry::Endpoint::read(pid).is_none() {
            continue;
        }
        v.push((pid, cmd, up));
    }
    v.sort_by_key(|x| x.0);
    v
}
