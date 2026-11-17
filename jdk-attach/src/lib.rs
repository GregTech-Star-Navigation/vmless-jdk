//! VMless JVM 的 attach 协议与性能计数器。
//!
//! # 一句话
//!
//! VM 侧调 [`start`] 挂上 [`VmProbe`]，工具侧用 [`client::Client`] 连上来；
//! 计数器写成 **HotSpot v2.0 二进制格式**，所以真 `jps` / 真 `jstat` 也能读。
//!
//! # 为什么这么选（与 HotSpot 对齐的部分）
//!
//! | 能力 | HotSpot 的做法 | 这里的做法 |
//! | --- | --- | --- |
//! | 进程发现 / 计数器 | `%TEMP%/hsperfdata_<user>/<pid>` 共享内存文件 | **同路径同格式**，逐字节兼容 |
//! | 命令行控制 | 本地 socket + `.attach_pid` 触发文件 | `%TEMP%/hsperfdata_<user>/<pid>.attach` 里写 port+token，直接连回环 TCP |
//! | 同用户限制 | `%TEMP%` 的目录 ACL | 同上，再加 128 位令牌 |
//!
//! 没走"触发文件 + 信号"的原因：跨平台要处理信号/命名管道两套，而且**我们控制的
//! 是 VM 自己**，不需要靠外部文件把睡眠中的进程叫醒 —— 监听线程一直在。
//!
//! # VM 侧需要做什么（只有 3 步）
//!
//! ```no_run
//! use vmless_attach::{start, AttachOptions, VmIdentity, VmProbe, PerfSnapshot};
//! # struct MyVm;
//! # impl VmProbe for MyVm { fn perf(&self) -> PerfSnapshot { PerfSnapshot::default() } }
//! # fn main() -> std::io::Result<()> {
//! let mut id = VmIdentity::new("com.example.Main");
//! id.java_command = "com.example.Main --flag".into();
//! let h = start(id, std::sync::Arc::new(MyVm), AttachOptions::default())?;
//! // 之后随时更新单个计数器（可选，比整份快照便宜）
//! h.perf_counter_set(vmless_attach::PerfId::ThreadsLive, 12);
//! # Ok(()) }
//! ```

pub mod client;
pub mod dispatch;
pub mod fmt;
pub mod counters;
pub mod heap;
pub mod hprof;
pub mod perf;
pub mod protocol;
pub mod registry;
pub mod server;
pub mod sys;
pub mod threads;
pub mod tool;

pub use client::{AttachError, Client};
pub use counters::{GcStat, GenStat, PerfSnapshot, SpaceStat, TlabStat, VmStrings};
pub use heap::{ClassDef, ClassHisto, FieldDef, HeapModel, HeapObject, LoaderStat};
pub use perf::{PerfValue, Sample, Units, Var};
pub use server::{AttachHandle, AttachOptions};
pub use threads::{Frame, JThread, LockRef, ThreadDump};

use std::sync::Arc;

/// 去掉回复里开头的 `<pid>:` 行。
///
/// 服务端每条命令都会带 pid 头（`jcmd` 的版式要求），但 `jstack` / `jinfo` 的输出里
/// **没有**这一行 —— 由工具自己剥掉，比让服务端按客户端分版式更省事。
pub fn strip_pid_header(t: &str) -> String {
    // 用 char::from(10) 而不是字符字面量，避免源码里的换行转义踩坑
    let nl = char::from(10u8);
    match t.split_once(nl) {
        Some((first, rest)) => {
            let f = first.trim_end();
            let digits = f.strip_suffix(':').unwrap_or("");
            if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                rest.to_string()
            } else {
                t.to_string()
            }
        }
        None => t.to_string(),
    }
}

/// VM 名（出现在 `jstack` 的 `Full thread dump ...` 与 `VM.version` 里）。
pub const VM_NAME: &str = "VMless Server VM";
/// VM 版本。
pub const VM_VERSION: &str = env!("CARGO_PKG_VERSION");

/// VM 身份与启动参数。`jps` / `jinfo` / `jcmd VM.command_line` 都从这里取。
#[derive(Debug, Clone)]
pub struct VmIdentity {
    pub pid: u32,
    /// 主类全名（`-l` 用）
    pub main_class: String,
    /// 主类 + main 参数（`sun.rt.javaCommand`，jps 直接显示它）
    pub java_command: String,
    /// `-X` / `-D` / `--add-opens` 之类
    pub jvm_args: Vec<String>,
    /// `-XX:` 部分
    pub vm_flags: Vec<String>,
    pub class_path: String,
    pub java_home: String,
    /// 有序（输出顺序即插入顺序）
    pub system_properties: Vec<(String, String)>,
    /// 启动时刻（Unix 毫秒）
    pub start_ms: u64,
    /// GC 策略名（`sun.gc.policy.name`）
    pub gc_policy_name: String,
    /// 版本三元组，用于 `sun.rt.jvmVersion`
    pub version_major: u8,
    pub version_minor: u8,
    pub version_micro: u8,
    pub version_patch: u8,
}

impl VmIdentity {
    pub fn new(main_class: impl Into<String>) -> VmIdentity {
        let main_class = main_class.into();
        VmIdentity {
            pid: std::process::id(),
            java_command: main_class.clone(),
            main_class,
            jvm_args: Vec::new(),
            vm_flags: Vec::new(),
            class_path: String::new(),
            java_home: String::new(),
            system_properties: Vec::new(),
            start_ms: registry::now_ms(),
            gc_policy_name: "VMless Mark-Sweep".to_string(),
            version_major: 0,
            version_minor: 1,
            version_micro: 0,
            version_patch: 0,
        }
    }

    /// 转成 `VmStrings`（perfdata 里的常量/字符串计数器）。
    pub fn strings(&self) -> VmStrings {
        let mut props = vec![
            ("java.version".to_string(), format!("{}.{}", 0, 1)),
            ("java.vm.name".to_string(), VM_NAME.to_string()),
            ("java.vm.version".to_string(), VM_VERSION.to_string()),
            ("java.vm.vendor".to_string(), "VMless".to_string()),
            ("java.vm.info".to_string(), "mixed mode".to_string()),
            ("java.home".to_string(), self.java_home.clone()),
            ("java.class.path".to_string(), self.class_path.clone()),
            ("sun.java.command".to_string(), self.java_command.clone()),
        ];
        props.extend(self.system_properties.iter().cloned());
        let jvm_version = ((self.version_major as i64) << 24)
            | ((self.version_minor as i64) << 16)
            | ((self.version_micro as i64) << 8)
            | (self.version_patch as i64);
        VmStrings {
            java_command: self.java_command.clone(),
            vm_args: self.jvm_args.join(" "),
            vm_flags: self.vm_flags.join(" "),
            created_ms: self.start_ms,
            vm_init_done_ms: self.start_ms,
            jvm_version,
            jvm_capabilities: "1".to_string(),
            policy_name: self.gc_policy_name.clone(),
            system_properties: props,
            // 由 server::start 填（要等端口定下来）
            attach_port: 0,
            attach_token: String::new(),
        }
    }
}

/// 计数器 id —— 给 [`AttachHandle::perf_counter_set`] 用。
///
/// 与整份 [`PerfSnapshot`] 的分工：快照是"一次全量"（周期刷新），这里是"单个热计数器"
/// （解释器循环里想更新就更新，不复制整个结构）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerfId {
    ThreadsLive,
    ThreadsDaemon,
    ThreadsStarted,
    ThreadsPeak,
    ClassesLoaded,
    ClassesUnloaded,
    ClassBytes,
    Safepoints,
    SafepointTimeNs,
    ApplicationTimeNs,
    JitCompiled,
    JitTimeNs,
    YoungGcCount,
    YoungGcTimeNs,
    FullGcCount,
    FullGcTimeNs,
}

impl PerfId {
    /// 对应的 perfdata 计数器名。
    pub fn name(self) -> &'static str {
        match self {
            PerfId::ThreadsLive => "java.threads.live",
            PerfId::ThreadsDaemon => "java.threads.daemon",
            PerfId::ThreadsStarted => "java.threads.started",
            PerfId::ThreadsPeak => "java.threads.livePeak",
            PerfId::ClassesLoaded => "java.cls.loadedClasses",
            PerfId::ClassesUnloaded => "java.cls.unloadedClasses",
            PerfId::ClassBytes => "sun.cls.loadedBytes",
            PerfId::Safepoints => "sun.rt.safepoints",
            PerfId::SafepointTimeNs => "sun.rt.safepointTime",
            PerfId::ApplicationTimeNs => "sun.rt.applicationTime",
            PerfId::JitCompiled => "sun.ci.totalCompiles",
            PerfId::JitTimeNs => "java.ci.totalTime",
            PerfId::YoungGcCount => "sun.gc.collector.0.invocations",
            PerfId::YoungGcTimeNs => "sun.gc.collector.0.time",
            PerfId::FullGcCount => "sun.gc.collector.1.invocations",
            PerfId::FullGcTimeNs => "sun.gc.collector.1.time",
        }
    }
}

/// VM 必须实现的东西。
///
/// **每个方法都有默认实现**，默认返回"未实现" —— 这样 VM 可以一块一块接：
/// 先只接 [`VmProbe::perf`]（jps/jstat 就能用），再逐步接线程栈/堆转储。
/// 没实现的命令在 `jcmd` 里返回 `不支持`，而不是崩掉或者给假数据。
pub trait VmProbe: Send + Sync + 'static {
    /// 计数器快照。**这是最小必需项**。
    fn perf(&self) -> PerfSnapshot;

    /// `GC.heap_info` 的正文（人类可读）。
    fn heap_info(&self) -> Result<String, String> {
        Err("VM 未实现 GC.heap_info".into())
    }
    /// `jmap -histo` / `GC.class_histogram`。必须**真扫堆**。
    fn heap_histogram(&self) -> Result<Vec<ClassHisto>, String> {
        Err("VM 未实现类直方图".into())
    }
    /// `jmap -clstats`。
    fn class_loaders(&self) -> Result<Vec<LoaderStat>, String> {
        Err("VM 未实现类加载器统计".into())
    }
    /// `GC.class_stats`。
    fn class_stats(&self) -> Result<Vec<ClassStat>, String> {
        Err("VM 未实现类统计".into())
    }
    /// `GC.run`：**真的**跑一次 GC（stop-the-world 由 VM 自己保证）。
    fn run_gc(&self) -> Result<String, String> {
        Err("VM 未实现 GC.run".into())
    }
    /// `jstack` / `Thread.print` 的数据。
    fn threads(&self) -> Result<ThreadDump, String> {
        Err("VM 未实现线程栈".into())
    }
    /// `jmap -dump` / `GC.heap_dump` 的模型。
    fn heap_model(&self) -> Result<HeapModel, String> {
        Err("VM 未实现堆转储".into())
    }
    /// `VM.native_memory summary` 的正文（VM 自己的分配器统计）。
    fn native_memory(&self) -> Result<String, String> {
        Err("VM 未实现 --native-memory 跟踪".into())
    }
    /// 读一个 `-XX:` 标志。
    fn get_flag(&self, name: &str) -> Option<String> {
        let _ = name;
        None
    }
    /// 写一个 `-XX:` 标志。改不了就返回 `Err`（jinfo 会如实报出来）。
    fn set_flag(&self, name: &str, value: &str) -> Result<(), String> {
        let _ = (name, value);
        Err("VM 未实现标志写入".into())
    }
    /// VM 自己的 `-XX:` 标志表（`jinfo -flags` / `VM.flags`）。
    fn flags(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// 一条类统计（`GC.class_stats`）。
#[derive(Debug, Clone, Default)]
pub struct ClassStat {
    pub name: String,
    pub class_loader: String,
    pub instances: u64,
    pub bytes: u64,
    pub methods: u64,
    pub constant_pool_bytes: u64,
}

/// 什么都不实现的最小 probe：只提供计数器骨架。
///
/// 用来走最短的接入路径 —— [`vmless_attach_start`] 就靠它。VM 侧可以把
/// [`AttachHandle::perf_counter_set`] 当作全部接口先跑通，之后再逐块换成
/// 完整实现 [`VmProbe`]。
pub struct DefaultProbe {
    /// 只要填了这几个数，`jstat -gc/-class` 立刻就有意义
    pub perf: std::sync::Mutex<PerfSnapshot>,
}

impl Default for DefaultProbe {
    fn default() -> Self {
        DefaultProbe { perf: std::sync::Mutex::new(PerfSnapshot::default()) }
    }
}

impl VmProbe for DefaultProbe {
    fn perf(&self) -> PerfSnapshot {
        self.perf.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// 最短接入路径：`vmless_attach_start("com.example.Main", pid)`。
///
/// 等价于 `start(VmIdentity::new(name) with pid, Arc::new(DefaultProbe), default)`。
/// 之后所有计数器都靠 [`AttachHandle::perf_counter_set`] 推进去。
pub fn vmless_attach_start(name: &str, pid: u32) -> std::io::Result<AttachHandle> {
    let mut id = VmIdentity::new(name);
    id.pid = if pid == 0 { std::process::id() } else { pid };
    start(id, Arc::new(DefaultProbe::default()), AttachOptions::default())
}

/// 挂载 attach 服务。
///
/// 成功返回的 [`AttachHandle`] 一旦 drop 就会注销（删 perfdata + 端点文件、停线程）。
pub fn start(
    identity: VmIdentity,
    probe: Arc<dyn VmProbe>,
    opts: AttachOptions,
) -> std::io::Result<AttachHandle> {
    server::start(identity, probe, opts)
}
