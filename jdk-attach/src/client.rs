//! 客户端库 —— 七个工具共用。
//!
//! 设计要点：
//! - **每一次 IO 都有超时**（默认 2s，`-t` 可调）：`connect_timeout` + `set_read_timeout`
//!   + `set_write_timeout`，三处都设。少设一处就会在目标挂死时永久阻塞。
//! - **失败要能区分**：`pid 不存在` / `不是我们的 VM` / `协议版本不符` / `超时` 是四种
//!   完全不同的处境，各自的退出码不同（见 [`AttachError::exit_code`]）。
//! - **不发 panic 路径**：任何解析失败都变成 `Protocol(String)`。

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use crate::protocol::{self, Frame, FrameError, Op, Status};
use crate::registry::Endpoint;

pub const DEFAULT_TIMEOUT_MS: u64 = 2000;

/// 服务端自述信息（握手时带回）。
#[derive(Debug, Clone, Default)]
pub struct ServerInfo {
    pub protocol_version: u8,
    pub vm_name: String,
    pub vm_version: String,
    pub main_class: String,
}

impl ServerInfo {
    fn parse(text: &str) -> ServerInfo {
        let mut info = ServerInfo { protocol_version: protocol::VERSION, ..Default::default() };
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                match k.trim() {
                    "version" => info.protocol_version = v.trim().parse().unwrap_or(protocol::VERSION),
                    "vm" => info.vm_name = v.trim().to_string(),
                    "vmversion" => info.vm_version = v.trim().to_string(),
                    "mainclass" => info.main_class = v.trim().to_string(),
                    _ => {}
                }
            }
        }
        info
    }
}

#[derive(Debug)]
pub enum AttachError {
    /// 目录里连 perfdata 都没有 —— 这个 pid 不是 VM。
    NoSuchVm(u32),
    /// 有 perfdata，但没有我们的控制端点：多半是真 HotSpot，或者是我们的 VM 还没握手。
    NotOurVm(u32),
    /// TCP 连不上（端口关了 / 被防火墙拦了）。
    Connect(String, io::Error),
    /// 目标进程已经不在了。
    VmGone(u32),
    /// 超时。参数是哪一步。
    Timeout(&'static str),
    /// 帧/协议层错误（含版本不符）。
    Protocol(String),
    /// 服务端明确拒绝（令牌/命令/执行失败）。
    Remote(Status, String),
    Io(io::Error),
}

impl AttachError {
    /// 进程级退出码。**整张表见 docs/cli-exit-codes.md**。
    pub fn exit_code(&self) -> i32 {
        match self {
            AttachError::NoSuchVm(_) | AttachError::NotOurVm(_) | AttachError::VmGone(_) => 2,
            AttachError::Protocol(_) => 3,
            AttachError::Timeout(_) => 4,
            AttachError::Connect(_, _) | AttachError::Io(_) => 5,
            AttachError::Remote(_, _) => 6,
        }
    }
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttachError::NoSuchVm(pid) => write!(f, "目标 VM {} 不存在（没有 perfdata）", pid),
            AttachError::NotOurVm(pid) => write!(
                f,
                "目标 VM {} 不是 VMless VM（有 perfdata，但没有 attach 端点）",
                pid
            ),
            AttachError::Connect(a, e) => write!(f, "无法连接 {}: {}", a, e),
            AttachError::VmGone(pid) => write!(f, "目标 VM {} 已退出", pid),
            AttachError::Timeout(what) => write!(f, "操作超时（{}）", what),
            AttachError::Protocol(m) => write!(f, "协议错误: {}", m),
            AttachError::Remote(s, m) => write!(f, "目标拒绝({:?}): {}", s, m),
            AttachError::Io(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for AttachError {}

impl From<io::Error> for AttachError {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::TimedOut || e.kind() == io::ErrorKind::WouldBlock {
            AttachError::Timeout("io")
        } else {
            AttachError::Io(e)
        }
    }
}

/// 已建立的 attach 连接。
pub struct Client {
    stream: TcpStream,
    pub pid: u32,
    pub info: ServerInfo,
    /// 握手耗时，便于 `-v` 观察。
    pub handshake: Duration,
}

impl Client {
    /// 连接并握手。
    pub fn attach(pid: u32, timeout_ms: u64) -> Result<Client, AttachError> {
        let t = Duration::from_millis(timeout_ms.max(1));
        // ① 端点
        let ep = match Endpoint::read(pid) {
            Some(ep) => ep,
            None => {
                return Err(if crate::registry::perf_path(pid).exists() {
                    AttachError::NotOurVm(pid)
                } else {
                    AttachError::NoSuchVm(pid)
                });
            }
        };
        // ② 进程还活着吗（端点文件可能是残留的）
        if !crate::registry::pid_alive(pid) {
            return Err(AttachError::VmGone(pid));
        }
        // ③ 连（只连回环地址 —— 服务端也只会 bind 回环）
        let addr = SocketAddr::from(([127, 0, 0, 1], ep.port));
        let started = Instant::now();
        let stream = TcpStream::connect_timeout(&addr, t).map_err(|e| {
            if e.kind() == io::ErrorKind::TimedOut {
                AttachError::Timeout("connect")
            } else {
                AttachError::Connect(format!("127.0.0.1:{}", ep.port), e)
            }
        })?;
        let _ = stream.set_nodelay(true);
        stream.set_read_timeout(Some(t)).map_err(AttachError::Io)?;
        stream.set_write_timeout(Some(t)).map_err(AttachError::Io)?;

        let mut c = Client {
            stream,
            pid,
            info: ServerInfo { protocol_version: ep.version, vm_name: String::new(), vm_version: String::new(), main_class: String::new() },
            handshake: Duration::ZERO,
        };
        // ④ 握手
        if std::env::var("VMLESS_ATTACH_DEBUG").is_ok() {
            eprintln!("[client] 连上 127.0.0.1:{} token={} 长度={}", ep.port, ep.token, ep.token.len());
        }
        let reply = c.request(Frame::request(Op::Hello, ep.token.clone().into_bytes()))?;
        if std::env::var("VMLESS_ATTACH_DEBUG").is_ok() {
            eprintln!("[client] 握手回复 status={:?} body={:?}", reply.status(), reply.body_string());
        }
        match reply.status() {
            Status::Ok => {}
            Status::BadToken => {
                return Err(AttachError::Protocol("令牌校验失败（可能不是同一用户）".into()))
            }
            Status::BadVersion => {
                return Err(AttachError::Protocol(format!(
                    "协议版本不符（服务端支持 {}，本端 {}）",
                    reply.body_string().trim(),
                    protocol::VERSION
                )))
            }
            s => return Err(AttachError::Remote(s, reply.body_string())),
        }
        c.info = ServerInfo::parse(&reply.body_string());
        c.handshake = started.elapsed();
        Ok(c)
    }

    /// 发一个请求并收完整回复（自动拼装 `FLAG_MORE` 分块）。
    fn request(&mut self, f: Frame) -> Result<Frame, AttachError> {
        protocol::write_frame(&mut self.stream, &f).map_err(map_io)?;
        let mut first: Option<Frame> = None;
        let mut body: Vec<u8> = Vec::new();
        loop {
            let fr = protocol::read_frame(&mut self.stream).map_err(map_frame_err)?;
            if !fr.is_reply() {
                return Err(AttachError::Protocol(format!(
                    "期望回复帧，收到 op=0x{:02x}",
                    fr.op
                )));
            }
            if first.is_none() {
                let status = fr.status();
                if !status.is_ok() {
                    return Ok(fr);
                }
                first = Some(fr.clone());
            }
            body.extend_from_slice(fr.body());
            if !fr.more() {
                break;
            }
        }
        let mut f0 = first.unwrap_or_else(|| Frame::reply(Op::Ping, Status::Ok, Vec::new()));
        // reply 的 payload = status + body
        let mut payload = Vec::with_capacity(body.len() + 1);
        payload.push(f0.payload.first().copied().unwrap_or(0));
        payload.extend_from_slice(&body);
        f0.payload = payload;
        Ok(f0)
    }

    /// 执行一条 jcmd 风格命令。
    pub fn command(&mut self, line: &str) -> Result<String, AttachError> {
        let r = self.request(Frame::request(Op::Command, line.as_bytes().to_vec()))?;
        if !r.status().is_ok() {
            return Err(AttachError::Remote(r.status(), r.body_string()));
        }
        Ok(r.body_string())
    }

    /// 二进制结构化请求。
    pub fn request_bin(&mut self, op: Op, arg: &[u8]) -> Result<Vec<u8>, AttachError> {
        let r = self.request(Frame::request(op, arg.to_vec()))?;
        if !r.status().is_ok() {
            return Err(AttachError::Remote(r.status(), r.body_string()));
        }
        Ok(r.body().to_vec())
    }

    /// 流式请求：边收边写，用于 hprof 这种大载荷。
    pub fn request_stream(
        &mut self,
        op: Op,
        arg: &[u8],
        mut sink: impl Write,
    ) -> Result<u64, AttachError> {
        protocol::write_frame(&mut self.stream, &Frame::request(op, arg.to_vec())).map_err(map_io)?;
        let mut total = 0u64;
        let mut first = true;
        loop {
            let fr = protocol::read_frame(&mut self.stream).map_err(map_frame_err)?;
            if !fr.is_reply() {
                return Err(AttachError::Protocol(format!("期望回复帧，收到 op=0x{:02x}", fr.op)));
            }
            if first {
                first = false;
                if !fr.status().is_ok() {
                    return Err(AttachError::Remote(fr.status(), fr.body_string()));
                }
            }
            let b = fr.body();
            sink.write_all(b).map_err(|e| AttachError::Io(e))?;
            total += b.len() as u64;
            if !fr.more() {
                break;
            }
        }
        sink.flush().map_err(AttachError::Io)?;
        Ok(total)
    }

    /// 只 ping 一下（不会执行任何 VM 侧代码）。
    pub fn ping(&mut self) -> Result<String, AttachError> {
        let r = self.request(Frame::request(Op::Ping, Vec::new()))?;
        if !r.status().is_ok() {
            return Err(AttachError::Remote(r.status(), r.body_string()));
        }
        Ok(r.body_string())
    }
}

fn map_io(e: io::Error) -> AttachError {
    if e.kind() == io::ErrorKind::TimedOut || e.kind() == io::ErrorKind::WouldBlock {
        AttachError::Timeout("write")
    } else if e.kind() == io::ErrorKind::ConnectionReset || e.kind() == io::ErrorKind::BrokenPipe {
        AttachError::Protocol("连接被对端关闭（目标 VM 可能已退出）".into())
    } else {
        AttachError::Io(e)
    }
}

fn map_frame_err(e: FrameError) -> AttachError {
    match e {
        FrameError::Eof => AttachError::Protocol("目标关闭了连接（VM 可能已退出）".into()),
        FrameError::Io(io) => map_io(io),
        FrameError::BadVersion { version, .. } => {
            AttachError::Protocol(format!("协议版本不符（对端 {}，本端 {}）", version, protocol::VERSION))
        }
        FrameError::BadMagic(m) => AttachError::Protocol(format!("魔数不符 0x{:02x}", m)),
        FrameError::TooLarge(n) => AttachError::Protocol(format!("帧过大 {}", n)),
        FrameError::BadOp(o) => AttachError::Protocol(format!("未知操作码 0x{:02x}", o)),
        FrameError::Truncated(w) => AttachError::Protocol(format!("读取{}时中断", w)),
    }
}

/// 从 argv 里解析 `-t <ms>`，返回 (剩余参数, 超时毫秒)。
///
/// 所有工具共用，保证 `-t` 的语义只有一处定义。
pub fn take_timeout(args: &[String]) -> (Vec<String>, u64) {
    let mut out = Vec::new();
    let mut timeout = DEFAULT_TIMEOUT_MS;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "-t" || a == "--timeout" {
            if let Some(v) = args.get(i + 1) {
                if let Ok(n) = v.parse::<u64>() {
                    timeout = n.max(1);
                    i += 2;
                    continue;
                }
            }
            // 值坏了就忽略，保持默认（不 panic）
            i += 1;
            continue;
        }
        if let Some(v) = a.strip_prefix("-t") {
            if let Ok(n) = v.parse::<u64>() {
                timeout = n.max(1);
                i += 1;
                continue;
            }
        }
        out.push(a.clone());
        i += 1;
    }
    (out, timeout)
}

/// `Read::read_exact` 的带超时版本（用于 jstatd 之类拿裸 socket 的地方）。
pub fn read_exact_timeout(s: &mut TcpStream, buf: &mut [u8]) -> io::Result<()> {
    let mut got = 0;
    while got < buf.len() {
        match s.read(&mut buf[got..]) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof")),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_flag_parsing() {
        let a: Vec<String> = ["-t", "500", "1234"].iter().map(|s| s.to_string()).collect();
        let (rest, t) = take_timeout(&a);
        assert_eq!(t, 500);
        assert_eq!(rest, vec!["1234".to_string()]);

        let a: Vec<String> = ["-t900", "9"].iter().map(|s| s.to_string()).collect();
        let (rest, t) = take_timeout(&a);
        assert_eq!(t, 900);
        assert_eq!(rest, vec!["9".to_string()]);

        // 坏值 / 缺值不 panic
        let a: Vec<String> = ["-t", "abc"].iter().map(|s| s.to_string()).collect();
        let (_, t) = take_timeout(&a);
        assert_eq!(t, DEFAULT_TIMEOUT_MS);
        let a: Vec<String> = ["-t"].iter().map(|s| s.to_string()).collect();
        let (_, t) = take_timeout(&a);
        assert_eq!(t, DEFAULT_TIMEOUT_MS);
    }

    #[test]
    fn exit_codes_are_distinct() {
        assert_eq!(AttachError::NoSuchVm(1).exit_code(), 2);
        assert_eq!(AttachError::Protocol("x".into()).exit_code(), 3);
        assert_eq!(AttachError::Timeout("x").exit_code(), 4);
        assert_eq!(AttachError::Io(io::Error::other("x")).exit_code(), 5);
        assert_eq!(AttachError::Remote(Status::Failed, "x".into()).exit_code(), 6);
    }

    #[test]
    fn server_info_parse() {
        let i = ServerInfo::parse("version=1\nvm=VMless Server VM\nvmversion=0.1.0\nmainclass=A\n");
        assert_eq!(i.protocol_version, 1);
        assert_eq!(i.vm_name, "VMless Server VM");
        assert_eq!(i.main_class, "A");
    }
}
