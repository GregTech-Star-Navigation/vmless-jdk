//! 各工具共用的小工具（参数解析、错误退出、目标选择）。
//!
//! 单独一个模块的原因：**退出码和错误文案只能有一处定义**。七个工具各自写一遍
//! `process::exit` 一定会漂移。

use std::io::Write;

use crate::client::{AttachError, Client};
use crate::registry;

/// 退出码表（与 `docs/cli.md` 一致）。
pub mod exit {
    pub const OK: i32 = 0;
    /// 命令行用错
    pub const USAGE: i32 = 1;
    /// 目标 VM 不存在 / 不是 VMless VM
    pub const NO_VM: i32 = 2;
    /// 协议错误（含版本不符）
    pub const PROTOCOL: i32 = 3;
    /// 超时
    pub const TIMEOUT: i32 = 4;
    /// 连接/IO 失败
    pub const IO: i32 = 5;
    /// 目标明确拒绝了命令
    pub const REMOTE: i32 = 6;
}

/// 打印错误并按 [`AttachError::exit_code`] 退出。
pub fn die(e: &AttachError) -> ! {
    let _ = writeln!(std::io::stderr(), "错误: {}", e);
    std::process::exit(e.exit_code())
}

/// 打印错误信息并退出（自定义码）。
pub fn die_msg(code: i32, msg: &str) -> ! {
    let _ = writeln!(std::io::stderr(), "错误: {}", msg);
    std::process::exit(code)
}

/// 用法错误。
pub fn usage(msg: &str) -> ! {
    let _ = writeln!(std::io::stderr(), "{}", msg);
    std::process::exit(exit::USAGE)
}

/// 打印一行到 stdout（忽略 EPIPE —— 被 `head` 截断时不该 panic）。
pub fn out(s: &str) {
    let mut so = std::io::stdout();
    let _ = so.write_all(s.as_bytes());
    let _ = so.flush();
}

/// 解析 `<vmid>`，支持 `pid` 与 `pid@host:port`。
pub fn parse_vmid(s: &str) -> Result<(u32, Option<String>), String> {
    let (pid_part, host) = match s.split_once('@') {
        Some((p, h)) => (p, Some(h.to_string())),
        None => (s, None),
    };
    let pid: u32 = pid_part
        .trim()
        .parse()
        .map_err(|_| format!("不是合法的 pid: {:?}（只接受 `pid` 或 `pid@host:port`）", s))?;
    if pid == 0 {
        return Err("pid 不能是 0".to_string());
    }
    Ok((pid, host))
}

/// 连上目标 VM。
pub fn attach(pid: u32, timeout_ms: u64) -> Client {
    match Client::attach(pid, timeout_ms) {
        Ok(c) => c,
        Err(e) => die(&e),
    }
}

/// 列出所有"能 attach 的"我们的 VM（有 perfdata 且进程还在）。
pub fn list_local_vms() -> Vec<u32> {
    let mut v = Vec::new();
    for pid in registry::list_pids() {
        if !registry::perf_path(pid).exists() {
            continue;
        }
        if !crate::perf::looks_like_perf_file(&registry::perf_path(pid)) {
            continue;
        }
        if pid != registry::current_pid() && !registry::pid_alive(pid) {
            continue;
        }
        v.push(pid);
    }
    v
}

/// 把带 `@host` 的 vmid 变成纯 pid（远程模式在 `--remote` 里另行处理）。
pub fn require_local(pid: u32, host: &Option<String>) -> u32 {
    if let Some(h) = host {
        usage(&format!(
            "本工具只支持本地 attach；远程请用 `jstatd`（收到 host={}）",
            h
        ));
    }
    pid
}

/// 读取 pid 的 perfdata（jps/jstat/jstatd 走文件，**不 attach** —— 与 HotSpot 一致）。
pub fn read_perf(pid: u32) -> Result<crate::perf::PerfData, String> {
    let path = registry::perf_path(pid);
    if !path.exists() {
        return Err(format!("目标 VM {} 不存在（没有 perfdata）", pid));
    }
    crate::perf::read(&path).map_err(|e| format!("读 perfdata 失败: {}", e))
}

/// 从 `jstatd` 取远端 perfdata（`jstat -gc <pid>@<host>:<port>`）。
///
/// 与本地路径的差别只有"字节从哪来"：拿回来还是同一份 perfdata，所以列定义、
/// 格式、缺失列的 `-` 完全一致。
pub fn read_perf_remote(hostport: &str, pid: u32, timeout_ms: u64) -> Result<crate::perf::PerfData, String> {
    use crate::protocol::{self, Frame, Op, Status};
    use std::io::Read;
    use std::net::TcpStream;
    use std::time::Duration;

    let addr: std::net::SocketAddr = hostport
        .parse()
        .map_err(|_| format!("远端地址不合法: {:?}（要 host:port）", hostport))?;
    let t = Duration::from_millis(timeout_ms.max(1));
    let mut s = TcpStream::connect_timeout(&addr, t).map_err(|e| format!("连不上 {}: {}", hostport, e))?;
    s.set_read_timeout(Some(t)).map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(t)).map_err(|e| e.to_string())?;
    protocol::write_frame(&mut s, &Frame::request(Op::Hello, Vec::new())).map_err(|e| e.to_string())?;
    let hello = protocol::read_frame(&mut s).map_err(|e| e.to_string())?;
    if hello.status() != Status::Ok {
        return Err(format!("jstatd 拒绝握手: {}", hello.body_string()));
    }
    protocol::write_frame(&mut s, &Frame::request(Op::Command, format!("perf {}", pid).into_bytes()))
        .map_err(|e| e.to_string())?;
    let mut data = Vec::new();
    let mut first = true;
    loop {
        let f = protocol::read_frame(&mut s).map_err(|e| e.to_string())?;
        if first {
            first = false;
            if f.status() != Status::Ok {
                return Err(format!("jstatd 返回错误: {}", f.body_string()));
            }
        }
        data.extend_from_slice(f.body());
        if !f.more() {
            break;
        }
        if data.len() > 16 << 20 {
            return Err("远端 perfdata 过大（>16MB）".to_string());
        }
    }
    let mut tail = [0u8; 0];
    let _ = s.read(&mut tail);
    crate::perf::parse(&data).map_err(|e| format!("解析远端 perfdata 失败: {}", e))
}

/// `-h` / `--help` 的公共版式。
pub fn print_usage(text: &str) -> ! {
    out(text);
    std::process::exit(exit::OK)
}
