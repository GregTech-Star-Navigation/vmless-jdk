//! attach 服务端：把 VM 的计数器与线程栈接到 `repo/jdk-attach` 的协议上。
//!
//! 为什么需要这一段：协议侧（`jps` / `jstack` / `jmap` / `jstat` / `jcmd` / `jinfo`）
//! 早就实现完了，但 **VM 侧从来没启动过服务端** —— perfdata 里没有这个进程，
//! 于是所有工具都看不到 `rustjava`。这里把服务挂起来，并如实填它能填的那部分：
//!
//! | 命令 | 数据来源 |
//! | --- | --- |
//! | `jps` | `VmStrings.java_command`（crate 自己从 identity 里填） |
//! | `jstat` | `perf()`：线程数、已加载类数、年轻代/全量 GC 次数与累计停顿 |
//! | `jstack` | `threads()`：直接读 `ThreadRec.帧表`（与死锁报告同一份数据） |
//! | `jmap -histo` / `-dump` | **未实现**（crate 默认实现如实报「VM 未实现」） |
//!
//! 两条硬约束：
//! 1. **失败不影响执行**。attach 只是可观测性，perfdata 目录不可写之类的环境问题
//!    绝不能让 hello world 起不来 —— 只有 `VMLESS_ATTACH_TRACE=1` 才打一行原因。
//! 2. **不给假数据**。做不到的命令落回 crate 的默认实现（返回 `Err("VM 未实现 …")`），
//!    `jcmd` 会把那句话原样打出来 —— 比编一个 0 有用得多。

use super::shared::lk;
use super::thread::{ST_BLOCKED, ST_RUNNABLE, ST_TERMINATED, ST_TIMED_WAITING, ST_WAITING};
use super::{Shared, Vm};
use std::sync::Arc;
use vmless_attach::{GcStat, JThread, PerfSnapshot, ThreadDump, VmProbe};

/// 本进程启动时刻（`uptime_ms` 用；与 crate 的 identity.start_ms 无关）
fn uptime_ms() -> u64 {
    static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    T0.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

fn state_name(s: i32) -> &'static str {
    match s {
        ST_RUNNABLE => "RUNNABLE",
        ST_BLOCKED => "BLOCKED",
        ST_WAITING => "WAITING",
        ST_TIMED_WAITING => "TIMED_WAITING",
        ST_TERMINATED => "TERMINATED",
        _ => "RUNNABLE",
    }
}

/// `VmProbe` 的 VMless 实现。持 `Arc<Shared>`：**只读**共享世界，不碰任何线程私有状态。
struct VmAttachProbe {
    sh: Arc<Shared>,
}

impl VmProbe for VmAttachProbe {
    fn perf(&self) -> PerfSnapshot {
        // 线程表：一次快照，别在下面反复取锁（`lock` 是 std::sync::Mutex，不可重入）
        let (started, live, daemon) = {
            let g = lk(&self.sh.threads);
            let live = g.values().filter(|t| t.alive).count() as i64;
            let daemon = g.values().filter(|t| t.alive && t.daemon).count() as i64;
            (g.len() as i64, live, daemon)
        };
        let h = &self.sh.heap;
        PerfSnapshot {
            uptime_ms: uptime_ms(),
            threads_started: started,
            threads_live: live,
            threads_daemon: daemon,
            threads_peak: started,
            classes_loaded: self.sh.store_snapshot().by_name.len() as i64,
            // 收集器口径按 HotSpot 的命名：jstat 认 `collector.0/1`
            collectors: vec![
                GcStat {
                    name: "G1 Young Generation".into(),
                    invocations: h.minor_count.load(std::sync::atomic::Ordering::Relaxed) as i64,
                    time_ns: h.minor_pause_ns.load(std::sync::atomic::Ordering::Relaxed) as i64,
                    ..GcStat::default()
                },
                GcStat {
                    name: "G1 Old Generation".into(),
                    invocations: h.major_count.load(std::sync::atomic::Ordering::Relaxed) as i64,
                    time_ns: h.major_pause_ns.load(std::sync::atomic::Ordering::Relaxed) as i64,
                    ..GcStat::default()
                },
            ],
            ..PerfSnapshot::default()
        }
    }

    /// `jstack` / `Thread.print`：从每个 `ThreadRec` 的帧表直接读。
    ///
    /// 这与死锁报告用的是**同一份**数据（`ThreadRec.frames`），所以两条路径上看到的
    /// 栈一定一致 —— 不会出现「jstack 说在 A、死锁报告说在 B」。
    fn threads(&self) -> Result<ThreadDump, String> {
        // 先造一个只读上下文：`frames_of` 需要 `&Vm` 才能把帧表翻成 `FrameInfo`
        let vm = Vm::from_shared(Arc::clone(&self.sh));
        let recs: Vec<(u64, String, bool, bool, i32)> = {
            let g = lk(&self.sh.threads);
            g.values().map(|t| (t.tid, t.name.clone(), t.daemon, t.alive, t.state)).collect()
        };
        let mut out = Vec::with_capacity(recs.len());
        for (tid, name, daemon, alive, state) in recs {
            let mut t = JThread::new(name, tid, if alive { state_name(state) } else { "TERMINATED" });
            t.daemon = daemon;
            t.tid = tid;
            t.nid = tid;
            // 帧表按「调用次序」存；jstack 要的是「最内层在最上」，所以反过来
            let mut frames: Vec<_> = super::stacktrace::frames_of(&vm, tid)
                .into_iter()
                .map(|f| {
                    vmless_attach::Frame::java(
                        f.class.replace('/', "."),
                        f.method,
                        f.source.clone().unwrap_or_default(),
                        f.line as i32,
                    )
                })
                .collect();
            frames.reverse();
            t.frames = frames;
            out.push(t);
        }
        // 固定顺序（tid 升序）：jstack 每次输出的线程顺序必须稳定，否则没法 diff
        out.sort_by_key(|t| t.java_id);
        Ok(ThreadDump { threads: out })
    }
}

/// `jps` 的进程登记表：`<TMP>/vmless_jps/<pid>.txt`。
///
/// **不是 hsperfdata**：`jps`（`repo/jdk-tools`）读的是这张纯文本登记表，因为
/// hsperfdata 的二进制布局随 JDK 版本变。登记表原本只由 launcher（`javaw`/`jshell`/`jdb`）
/// 写，于是**裸跑 `run_jdk.exe` 的进程 jps 看不见** —— 这里把同一份格式写一遍，
/// 让直接跑的 VM 也出现在 `jps` 里。
///
/// 失败一律忽略：没有这张表 `jstack`/`jstat`/`jcmd` 照样能用（它们走 perfdata + 端点），
/// 只有 `jps` 列不出名字。过期条目由 `jps` 自己剔除已死 pid，不需要我们清理。
fn register_jps(pid: u32, main: &str) {
    let dir = std::env::temp_dir().join("vmless_jps");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|x| x.as_secs())
        .unwrap_or(0);
    let text = format!("pid={pid}\nmain={main}\nargs=\nlauncher=rustjava\nstarted={started}\n");
    let _ = std::fs::write(dir.join(format!("{pid}.txt")), text);
}

/// 起一次 attach 服务端。**同一进程只起一次**；句柄存进静态量活到进程结束
/// （`AttachHandle` 一 drop 就注销 perfdata）。
pub(super) fn start(main_class: &str, sh: Arc<Shared>) {
    static HANDLE: std::sync::Mutex<Option<vmless_attach::AttachHandle>> = std::sync::Mutex::new(None);
    {
        // 已经起过就别重复起（重复 start 会在同一 perfdata 目录上打架）
        if lk(&HANDLE).is_some() {
            return;
        }
    }
    let mut id = vmless_attach::VmIdentity::new(&main_class.replace('/', "."));
    id.pid = std::process::id();
    register_jps(id.pid, &id.main_class);
    let opts = vmless_attach::AttachOptions::default();
    match vmless_attach::start(id, Arc::new(VmAttachProbe { sh }), opts) {
        Ok(h) => {
            let pid = h.pid;
            *lk(&HANDLE) = Some(h);
            if std::env::var_os("VMLESS_ATTACH_TRACE").is_some() {
                eprintln!("[attach] 已挂上 pid={pid} main={main_class}");
            }
        }
        // 静默：attach 失败不能挡住执行
        Err(e) => {
            if std::env::var_os("VMLESS_ATTACH_TRACE").is_some() {
                eprintln!("[attach] 启动失败（已忽略）: {e}");
            }
        }
    }
}
