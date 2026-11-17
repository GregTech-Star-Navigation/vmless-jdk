//! attach 服务端：监听回环端口、写 perfdata、执行 jcmd 命令。
//!
//! 线程模型：
//! - **刷新线程**：每 `refresh_interval_ms` 调一次 `probe.perf()`，原地覆写 perfdata 的值区。
//! - **accept 线程**：非阻塞 accept，20ms 轮询一次停止标志（不用自连唤醒那套把戏）。
//! - **每连接一个线程**：读超时 5s 等握手，之后 60s 执行命令。命令执行用一把互斥锁串行化
//!   —— 两个客户端同时 `GC.run` 会把 VM 状态搞乱，串行是这里唯一正确的选择。

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::counters::PerfSnapshot;
use crate::heap::{ClassHisto, LoaderStat};
use crate::perf::{self, PerfFile, PerfValue, Sample};
use crate::protocol::{self, Frame, FrameError, Op, Status, MAX_PAYLOAD};
use crate::registry::{self, Endpoint};
use crate::{ClassStat, VmIdentity, VmProbe, PerfId, VM_NAME, VM_VERSION};


/// 服务端诊断日志：设了 `VMLESS_ATTACH_LOG=<路径>` 才写文件。
///
/// 为什么不用 stderr：Windows 上隐藏窗口/重定向的 stderr 会掺进调用方的输出，
/// 排查协议问题时反而看不清；而且客户端和服务端可能共用一个控制台。
pub fn dlog(msg: &str) {
    use std::io::Write;
    let path = match std::env::var("VMLESS_ATTACH_LOG") {
        Ok(p) if !p.is_empty() => p,
        _ => return,
    };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}", msg);
    }
}

/// 挂载选项。
#[derive(Debug, Clone)]
pub struct AttachOptions {
    /// 计数器刷新周期（毫秒）。
    pub refresh_interval_ms: u64,
    /// 并发连接上限。
    pub max_connections: usize,
    /// 接收命令后的执行超时（毫秒，0 = 不限）。
    pub command_timeout_ms: u64,
    /// 是否在启动时清理死进程残留。
    pub sweep_stale: bool,
}

impl Default for AttachOptions {
    fn default() -> Self {
        AttachOptions {
            refresh_interval_ms: 100,
            max_connections: 64,
            command_timeout_ms: 60_000,
            sweep_stale: true,
        }
    }
}

/// 挂上的 attach 服务。**drop 即注销**。
pub struct AttachHandle {
    pub pid: u32,
    evidence: Evidence,
    stop: Arc<AtomicBool>,
    acceptor: Option<JoinHandle<()>>,
    refresher: Option<JoinHandle<()>>,
    perf: Arc<Mutex<PerfFile>>,
    overlay: Arc<Mutex<Vec<(String, PerfValue)>>>,
    probe: Arc<dyn VmProbe>,
    identity: Arc<VmIdentity>,
    static_samples: Arc<Vec<Sample>>,
    exec: Arc<Mutex<()>>,
}

/// 对外暴露的连接信息（`perf_counter_set` 之类不经过它）。
struct Evidence {
    port: u16,
    token: String,
    start_ms: u64,
}

impl AttachHandle {
    pub fn port(&self) -> u16 {
        self.evidence.port
    }
    pub fn token(&self) -> &str {
        &self.evidence.token
    }
    /// 当前的端点（读 perfdata，保证和工具看到的一致）。
    pub fn endpoint(&self) -> Option<Endpoint> {
        Endpoint::read(self.pid)
    }

    /// 立刻刷一次 perfdata（`GC.run` 之后、或测试里要确定性时用）。
    pub fn refresh_now(&self) -> io::Result<()> {
        let samples = self.build_samples();
        let mut pf = self.perf.lock().unwrap_or_else(|e| e.into_inner());
        pf.update(&samples)
    }

    /// 设置**单个**热计数器。名字不在 perfdata 里则忽略（布局已定死）。
    pub fn perf_counter_set(&self, id: PerfId, value: i64) {
        self.set_named(id.name(), value)
    }

    /// 设置单个计数器的当前值（按名字）。
    pub fn set_named(&self, name: &str, value: i64) {
        {
            let mut ov = self.overlay.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(slot) = ov.iter_mut().find(|(n, _)| n == name) {
                slot.1 = PerfValue::Long(value);
            } else {
                ov.push((name.to_string(), PerfValue::Long(value)));
            }
        }
        let mut pf = self.perf.lock().unwrap_or_else(|e| e.into_inner());
        let _ = pf.update(&[Sample {
            name: name.to_string(),
            value: PerfValue::Long(value),
            units: perf::Units::None,
            var: perf::Var::Variable,
            supported: true,
        }]);
    }

    /// 累加计数（计数器常见用法）。
    pub fn perf_counter_add(&self, id: PerfId, delta: i64) {
        let name = id.name();
        // 从文件里读当前值再累加 —— 文件是唯一真相源，多个线程累加不会丢
        let cur = {
            let path = {
                let pf = self.perf.lock().unwrap_or_else(|e| e.into_inner());
                pf.path().to_path_buf()
            };
            perf::read(&path).ok().and_then(|d| d.get_i64(name)).unwrap_or(0)
        };
        let mut pf = self.perf.lock().unwrap_or_else(|e| e.into_inner());
        let _ = pf.update(&[Sample {
            name: name.to_string(),
            value: PerfValue::Long(cur + delta),
            units: perf::Units::None,
            var: perf::Var::Monotonic,
            supported: true,
        }]);
    }

    fn build_samples(&self) -> Vec<Sample> {
        let mut snap: PerfSnapshot = self.probe.perf();
        // 把覆盖值盖上
        let ov = self.overlay.lock().unwrap_or_else(|e| e.into_inner());
        if !ov.is_empty() {
            let mut v = snap.samples();
            for (n, val) in ov.iter() {
                if let Some(s) = v.iter_mut().find(|s| &s.name == n) {
                    s.value = val.clone();
                }
            }
            let mut all = (*self.static_samples).clone();
            all.append(&mut v);
            return all;
        }
        drop(ov);
        let mut all = (*self.static_samples).clone();
        all.append(&mut snap.samples());
        all
    }

    /// 主动注销（Drop 也会做，这里是显式版）。
    pub fn shutdown(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        if std::env::var("VMLESS_ATTACH_DEBUG").is_ok() {
            eprintln!("[vmless-attach] stop_inner: 注销 pid={} （如果这不是退出时打印的，说明 handle 被提前 drop 了）", self.pid);
            let bt = std::backtrace::Backtrace::force_capture();
            eprintln!("{}", bt);
        }
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.acceptor.take() {
            let _ = h.join();
        }
        if let Some(h) = self.refresher.take() {
            let _ = h.join();
        }
        registry::unregister(self.pid);
    }
}

impl Drop for AttachHandle {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

/// 挂载。见 [`crate::start`]。
pub fn start(identity: VmIdentity, probe: Arc<dyn VmProbe>, opts: AttachOptions) -> io::Result<AttachHandle> {
    let pid = if identity.pid == 0 { std::process::id() } else { identity.pid };
    let identity = Arc::new(VmIdentity { pid, ..identity });

    if opts.sweep_stale {
        let _ = registry::sweep_stale();
    }
    registry::ensure_dir(&registry::perf_dir())?;

    // ① 先 bind：端口要写进 perfdata，所以必须在建文件之前定下来。
    //    **只 bind 回环** —— 绑 0.0.0.0 会让同网段任何人都能 attach。
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    debug_assert!(port != 0);

    // ② perfdata：端点的 port/token 就是两个计数器（**不另建控制文件**）
    let token = registry::random_token();
    let mut strings = identity.strings();
    strings.attach_port = port;
    strings.attach_token = token.clone();
    let static_samples = Arc::new(strings.samples());
    let mut initial = (*static_samples).clone();
    initial.append(&mut probe.perf().samples());
    let perf_file = PerfFile::create(&registry::perf_path(pid), &initial, identity.start_ms)?;
    let perf = Arc::new(Mutex::new(perf_file));

    let stop = Arc::new(AtomicBool::new(false));
    let exec = Arc::new(Mutex::new(()));
    let overlay: Arc<Mutex<Vec<(String, PerfValue)>>> = Arc::new(Mutex::new(Vec::new()));

    // ④ 刷新线程
    let refresher = {
        let stop = stop.clone();
        let perf = perf.clone();
        let probe = probe.clone();
        let identity = identity.clone();
        let static_samples = static_samples.clone();
        let overlay = overlay.clone();
        let interval = opts.refresh_interval_ms.max(10);
        std::thread::Builder::new().name("vmless-perf".into()).spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let mut snap = probe.perf();
                let mut all = (*static_samples).clone();
                let mut nums = snap.samples();
                {
                    let ov = overlay.lock().unwrap_or_else(|e| e.into_inner());
                    for (n, v) in ov.iter() {
                        if let Some(s) = nums.iter_mut().find(|s| &s.name == n) {
                            s.value = v.clone();
                        }
                    }
                }
                all.append(&mut nums);
                {
                    let mut pf = perf.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = pf.update(&all);
                }
                let _ = identity;
                std::thread::sleep(Duration::from_millis(interval));
            }
        })?
    };

    // ⑤ accept 线程
    let conns = Arc::new(AtomicUsize::new(0));
    let acceptor = {
        let stop = stop.clone();
        let probe = probe.clone();
        let identity = identity.clone();
        let exec = exec.clone();
        let perf = perf.clone();
        let token_expect = token.clone();
        let max_conns = opts.max_connections;
        let cmd_timeout = Duration::from_millis(opts.command_timeout_ms.max(1000));
        std::thread::Builder::new().name("vmless-accept".into()).spawn(move || {
            let ctx = Ctx {
                pid,
                probe,
                identity,
                exec,
                perf,
                token: token_expect,
                cmd_timeout,
            };
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((s, peer)) => {
                        if !peer.ip().is_loopback() {
                            // 不该发生（只 bind 了回环），但真发生了就拒绝
                            drop(s);
                            continue;
                        }
                        if conns.fetch_add(1, Ordering::SeqCst) + 1 > max_conns {
                            conns.fetch_sub(1, Ordering::SeqCst);
                            let mut s = s;
                            let _ = s.set_write_timeout(Some(Duration::from_millis(500)));
                            let _ = protocol::write_frame(
                                &mut s,
                                &Frame::reply(Op::Ping, Status::Failed, b"too many concurrent attaches".to_vec()),
                            );
                            continue;
                        }
                        let ctx = ctx.clone();
                        let conns2 = conns.clone();
                        dlog(&format!("[accept] 收到连接 {:?}", peer));
                        let _ = std::thread::Builder::new().name("vmless-conn".into()).spawn(move || {
                            let _ = serve(ctx, s);
                            conns2.fetch_sub(1, Ordering::SeqCst);
                        });
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
        })?
    };

    Ok(AttachHandle {
        pid,
        evidence: Evidence { port, token, start_ms: identity.start_ms },
        stop,
        acceptor: Some(acceptor),
        refresher: Some(refresher),
        perf,
        overlay,
        probe,
        identity,
        static_samples,
        exec,
    })
}

#[derive(Clone)]
struct Ctx {
    pid: u32,
    probe: Arc<dyn VmProbe>,
    identity: Arc<VmIdentity>,
    exec: Arc<Mutex<()>>,
    perf: Arc<Mutex<PerfFile>>,
    token: String,
    cmd_timeout: Duration,
}

fn serve(ctx: Ctx, mut s: TcpStream) -> io::Result<()> {
    dlog(&format!("[serve] 新连接 peer={:?}", s.peer_addr()));
    let _ = s.set_nodelay(true);
    // **必须显式设回阻塞**：Windows 上 accept() 出来的 socket 会继承监听套接字的
    // 非阻塞模式（监听器为了 accept 轮询被设成了 nonblocking）。如果不改回来，
    // 服务端第一次 read 就会拿到 WouldBlock 然后直接关连接 —— 客户端看到的是
    // "连接被对端关闭"，而且只在"客户端发得比服务端读得慢"时才复现（典型竞态）。
    if let Err(e) = s.set_nonblocking(false) {
        dlog(&format!("[serve] set_nonblocking(false) 失败: {}", e));
        return Err(e);
    }
    s.set_read_timeout(Some(Duration::from_millis(5000)))?;
    s.set_write_timeout(Some(Duration::from_millis(30_000)))?;

    // ① 第一帧必须是 Hello，且令牌要对
    let first = match protocol::read_frame(&mut s) {
        Ok(f) => f,
        Err(FrameError::Eof) => return Ok(()),
        Err(FrameError::BadVersion { op, .. }) => {
            let _ = protocol::write_frame(
                &mut s,
                &Frame::reply(
                    Op::from_u8(op & !protocol::FLAG_REPLY).unwrap_or(Op::Ping),
                    Status::BadVersion,
                    format!("{}", protocol::VERSION).into_bytes(),
                ),
            );
            return Ok(());
        }
        Err(e) => {
            dlog(&format!("[serve] 读第一帧失败: {:?}", e));
            return Ok(());
        }
    };
    dlog(&format!("[serve] 第一帧 op={:?} payload_len={}", first.req_op(), first.payload.len()));
    if first.req_op() != Some(Op::Hello) {
        let _ = protocol::write_frame(
            &mut s,
            &Frame::reply(Op::Ping, Status::Failed, b"first frame must be Hello".to_vec()),
        );
        return Ok(());
    }
    dlog(&format!(
        "[serve] token 比对: match={} 收到长度={} 期望长度={}",
        first.payload == ctx.token.as_bytes(),
        first.payload.len(),
        ctx.token.len()
    ));
    if first.payload != ctx.token.as_bytes() {
        let _ = protocol::write_frame(
            &mut s,
            &Frame::reply(Op::Hello, Status::BadToken, b"bad token".to_vec()),
        );
        return Ok(());
    }
    let hello = format!(
        "version={}\nvm={}\nvmversion={}\nmainclass={}\npid={}\n",
        protocol::VERSION,
        VM_NAME,
        VM_VERSION,
        ctx.identity.main_class,
        ctx.pid
    );
    protocol::write_frame(&mut s, &Frame::reply(Op::Hello, Status::Ok, hello.into_bytes()))?;
    match s.set_read_timeout(Some(ctx.cmd_timeout)) {
        Ok(()) => dlog(&format!("[serve] 握手完成，读超时改为 {:?}", ctx.cmd_timeout)),
        Err(e) => {
            dlog(&format!("[serve] 设置读超时失败: {}", e));
            return Err(e);
        }
    }

    // ② 命令循环
    loop {
        dlog("[serve] 等待下一帧");
        let f = match protocol::read_frame(&mut s) {
            Ok(f) => f,
            Err(FrameError::Eof) => return Ok(()),
            Err(FrameError::Io(e))
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                // 读超时：告诉对端一声再断
                let _ = protocol::write_frame(
                    &mut s,
                    &Frame::reply(Op::Ping, Status::Failed, b"idle timeout".to_vec()),
                );
                return Ok(());
            }
            Err(e) => {
                let _ = protocol::write_frame(
                    &mut s,
                    &Frame::reply(Op::Ping, Status::BadOp, format!("{}", e).into_bytes()),
                );
                return Ok(());
            }
        };
        dlog(&format!("[serve] 收到帧 op={:?} len={}", f.req_op(), f.payload.len()));
        let op = match f.req_op() {
            Some(op) => op,
            None => {
                let _ = protocol::write_frame(
                    &mut s,
                    &Frame::reply(Op::Ping, Status::BadOp, b"unknown op".to_vec()),
                );
                continue;
            }
        };
        match op {
            Op::Hello => {
                protocol::write_frame(&mut s, &Frame::reply(Op::Hello, Status::BadToken, b"already hello".to_vec()))?;
            }
            Op::Ping => {
                protocol::write_frame(
                    &mut s,
                    &Frame::reply(Op::Ping, Status::Ok, format!("pong {} {}", VM_NAME, VM_VERSION).into_bytes()),
                )?;
            }
            Op::Command => {
                let line = String::from_utf8_lossy(&f.payload).into_owned();
                let perf_path = {
                    let pf = ctx.perf.lock().unwrap_or_else(|e| e.into_inner());
                    pf.path().to_path_buf()
                };
                let (status, body) =
                    crate::dispatch::run_command(ctx.pid, &ctx.identity, &ctx.probe, &perf_path, &line);
                protocol::write_frame(&mut s, &Frame::reply(Op::Command, status, body.into_bytes()))?;
            }
            Op::HeapHisto => {
                let r = {
                    let _g = ctx.exec.lock().unwrap_or_else(|e| e.into_inner());
                    ctx.probe.heap_histogram()
                };
                match r {
                    Ok(mut v) => {
                        ClassHisto::sort_by_bytes(&mut v);
                        let body = ClassHisto::to_wire(&v);
                        protocol::write_frame(&mut s, &Frame::reply(Op::HeapHisto, Status::Ok, body))?;
                    }
                    Err(e) => {
                        protocol::write_frame(&mut s, &Frame::reply(Op::HeapHisto, Status::Unsupported, e.into_bytes()))?;
                    }
                }
            }
            Op::ClassLoaders => {
                let r = {
                    let _g = ctx.exec.lock().unwrap_or_else(|e| e.into_inner());
                    ctx.probe.class_loaders()
                };
                match r {
                    Ok(v) => {
                        let body = LoaderStat::to_wire(&v);
                        protocol::write_frame(&mut s, &Frame::reply(Op::ClassLoaders, Status::Ok, body))?;
                    }
                    Err(e) => {
                        protocol::write_frame(&mut s, &Frame::reply(Op::ClassLoaders, Status::Unsupported, e.into_bytes()))?;
                    }
                }
            }
            Op::HeapDump => {
                // 流式：VM 直接往 socket 写，不经过内存攒整个文件
                let mut sink = FrameSink::new(&mut s, Op::HeapDump);
                let r = {
                    let _g = ctx.exec.lock().unwrap_or_else(|e| e.into_inner());
                    ctx.probe.heap_model().and_then(|model| {
                        crate::heap::write_hprof(&model, &mut sink, registry::now_ms())
                            .map(|_| ())
                            .map_err(|e| format!("写 hprof 失败: {}", e))
                    })
                };
                match r {
                    Ok(()) => {
                        sink.finish()?;
                    }
                    Err(e) => {
                        // 已经流出去一半了：用最后一帧带错误状态
                        sink.finish_error(Status::Unsupported, &e)?;
                    }
                }
            }
        }
    }
}

/// 把 `Write` 切成协议帧的适配器（hprof 流式输出用）。
struct FrameSink<'a> {
    s: &'a mut TcpStream,
    op: Op,
    buf: Vec<u8>,
    wrote_any: bool,
    finished: bool,
}

impl<'a> FrameSink<'a> {
    fn new(s: &'a mut TcpStream, op: Op) -> FrameSink<'a> {
        FrameSink { s, op, buf: Vec::with_capacity(MAX_PAYLOAD as usize), wrote_any: false, finished: false }
    }
    fn flush_chunk(&mut self, more: bool) -> io::Result<()> {
        let mut payload = Vec::with_capacity(self.buf.len() + 1);
        payload.push(Status::Ok as u8);
        payload.append(&mut self.buf);
        let f = Frame {
            op: (self.op as u8) | protocol::FLAG_REPLY,
            flags: if more { protocol::FLAG_MORE } else { 0 },
            payload,
        };
        protocol::write_frame(self.s, &f)?;
        self.wrote_any = true;
        Ok(())
    }
    fn finish(mut self) -> io::Result<()> {
        // 先带 FLAG_MORE 发最后一块（如果还有），再发一个空帧收尾
        if !self.buf.is_empty() {
            self.flush_chunk(true)?;
        }
        self.finished = true;
        self.flush_chunk(false)
    }
    fn finish_error(mut self, st: Status, msg: &str) -> io::Result<()> {
        if !self.wrote_any {
            let f = Frame::reply(self.op, st, msg.as_bytes().to_vec());
            protocol::write_frame(self.s, &f)?;
            return Ok(());
        }
        self.buf.clear();
        self.flush_chunk(false)
    }
}

impl<'a> Write for FrameSink<'a> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(b);
        while self.buf.len() >= MAX_PAYLOAD as usize {
            let rest = self.buf.split_off(MAX_PAYLOAD as usize);
            self.flush_chunk(true)?;
            self.buf = rest;
        }
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> Drop for FrameSink<'a> {
    fn drop(&mut self) {
        // 没走 finish 就 drop（panic 路径）—— 不能在这里写，socket 可能已坏
        let _ = self.finished;
    }
}
