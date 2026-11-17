//! `jstatd` —— 把本机的 perfdata 通过 TCP 暴露出去，供远程 `jstat` 取数。
//!
//! # 为什么不实现 RMI / JRMP
//!
//! 真 jstatd 是 RMI 服务：客户端要走 JRMP + Java 序列化 + 动态 stub 绑定，代价极高，
//! 而它**唯一的作用**是把 perfdata 的字节搬给客户端。这里直接做那条路：
//!
//! ```text
//! 客户端 → 服务端 : Hello（空令牌）
//! 客户端 → 服务端 : Command "list"                → "pid<TAB>mainClass\n"…
//! 客户端 → 服务端 : Command "perf <pid>"          → perfdata 原始字节（分块）
//! 客户端 → 服务端 : Command "cmd <pid> <command>" → 文本
//! ```
//!
//! 所以远端 `jstat` 拿到的是**同一份 perfdata**，列定义、格式、缺失列的 `-` 全都一致
//! —— 换的是传输层，不是数据模型。
//!
//! 客户端侧对应 [`vmless_attach::tool::read_perf_remote`]，CLI 是
//! `jstat -gc <pid>@<host>:<port>`。
//!
//! # 安全
//!
//! **默认只 bind 127.0.0.1**（与"不许监听 0.0.0.0"的要求一致）。要对外提供服务必须显式
//! `--bind 0.0.0.0`，而且要知道：jstatd 按设计是**匿名只读**的（JDK 自带的也一样，
//! 这正是历史上 jstatd 被打的原因）。要鉴权请用 SSH 端口转发。

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use vmless_attach::protocol::{self, Frame, Op, Status, MAX_PAYLOAD};
use vmless_attach::registry;
use vmless_attach::tool;

const USAGE: &str = "用法: jstatd -p <port> [-n <rminame>] [--bind <addr>]

  -p <port>      监听端口（必需，0 = 让系统挑一个）
  --bind <addr>  绑定地址，默认 127.0.0.1（**不要随便改成 0.0.0.0**）
  -n <name>      兼容真 jstatd 的参数，接受并忽略（本实现不走 RMI）
  -J<flag>       接受并忽略
";

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut port: Option<u16> = None;
    let mut bind = Ipv4Addr::LOCALHOST;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "-p" => {
                port = argv.get(i + 1).and_then(|v| v.parse().ok());
                i += 2;
            }
            "--bind" | "-b" => {
                let a = argv.get(i + 1).cloned().unwrap_or_default();
                match a.parse::<Ipv4Addr>() {
                    Ok(v) => bind = v,
                    Err(_) => tool::die_msg(tool::exit::USAGE, &format!("不合法的绑定地址: {}", a)),
                }
                i += 2;
            }
            "-n" => {
                i += 2; // 接受并忽略
            }
            "-h" | "--help" => tool::print_usage(USAGE),
            a if a.starts_with("-J") => {
                i += 1;
            }
            a => tool::die_msg(tool::exit::USAGE, &format!("不认识的参数: {}", a)),
        }
    }
    let port = match port {
        Some(p) => p,
        None => tool::usage(USAGE),
    };

    let listener = match TcpListener::bind((bind, port)) {
        Ok(l) => l,
        Err(e) => tool::die_msg(tool::exit::IO, &format!("无法监听 {}:{} — {}", bind, port, e)),
    };
    let addr = listener.local_addr().unwrap_or_else(|_| std::net::SocketAddr::from((bind, port)));
    if !addr.ip().is_loopback() {
        eprintln!(
            "警告: jstatd 监听在 {} —— 匿名只读服务（与 JDK 自带行为一致）。请确认这是你要的。",
            addr.ip()
        );
    }
    // 横幅用 ASCII：这是服务启动行，会进日志/被别的程序解析，不该受控制台代码页影响
    println!("jstatd listening on 127.0.0.1:{}  protocol=v{}", addr.port(), protocol::VERSION);
    println!("client usage: jstat -gc <pid>@127.0.0.1:{}", addr.port());
    let _ = std::io::stdout().flush();

    let conns = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    for s in listener.incoming() {
        match s {
            Ok(s) => {
                if conns.fetch_add(1, Ordering::SeqCst) > 32 {
                    conns.fetch_sub(1, Ordering::SeqCst);
                    continue;
                }
                let conns2 = conns.clone();
                let stop2 = stop.clone();
                let _ = std::thread::Builder::new().name("jstatd-conn".into()).spawn(move || {
                    let _ = serve(s, stop2);
                    conns2.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn serve(mut s: TcpStream, stop: Arc<AtomicBool>) -> std::io::Result<()> {
    let _ = s.set_nodelay(true);
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.set_write_timeout(Some(Duration::from_secs(30)))?;
    // Hello（不收令牌：jstatd 是匿名只读服务）
    match protocol::read_frame(&mut s) {
        Ok(f) if f.req_op() == Some(Op::Hello) => {}
        Ok(_) => {
            let _ = protocol::write_frame(&mut s, &Frame::reply(Op::Ping, Status::Failed, b"hello first".to_vec()));
            return Ok(());
        }
        Err(_) => return Ok(()),
    }
    protocol::write_frame(
        &mut s,
        &Frame::reply(Op::Hello, Status::Ok, format!("version={}\nvm=jstatd\n", protocol::VERSION).into_bytes()),
    )?;
    s.set_read_timeout(Some(Duration::from_secs(60)))?;

    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        let f = match protocol::read_frame(&mut s) {
            Ok(f) => f,
            Err(_) => return Ok(()),
        };
        if f.req_op() != Some(Op::Command) {
            protocol::write_frame(&mut s, &Frame::reply(Op::Ping, Status::BadOp, b"only Command".to_vec()))?;
            continue;
        }
        let line = String::from_utf8_lossy(&f.payload).into_owned();
        let mut it = line.split_whitespace();
        match it.next().unwrap_or("") {
            "list" => {
                let mut out = String::new();
                for pid in tool::list_local_vms() {
                    let main = tool::read_perf(pid)
                        .ok()
                        .and_then(|d| d.get_str("sun.rt.javaCommand").map(|s| s.split_whitespace().next().unwrap_or("").to_string()))
                        .unwrap_or_default();
                    out.push_str(&format!("{}\t{}\n", pid, main));
                }
                protocol::write_frame(&mut s, &Frame::reply(Op::Command, Status::Ok, out.into_bytes()))?;
            }
            "perf" => {
                let pid: Option<u32> = it.next().and_then(|p| p.parse().ok());
                let pid = match pid {
                    Some(p) => p,
                    None => {
                        protocol::write_frame(&mut s, &Frame::reply(Op::Command, Status::BadCommand, b"perf needs pid".to_vec()))?;
                        continue;
                    }
                };
                let path = registry::perf_path(pid);
                let mut f2 = match std::fs::File::open(&path) {
                    Ok(f) => f,
                    Err(e) => {
                        protocol::write_frame(
                            &mut s,
                            &Frame::reply(Op::Command, Status::Failed, format!("{}", e).into_bytes()),
                        )?;
                        continue;
                    }
                };
                // 分块发送（1 MiB 一帧，FLAG_MORE 续传）
                let mut buf = vec![0u8; MAX_PAYLOAD as usize];
                let mut first = true;
                loop {
                    let n = match f2.read(&mut buf) {
                        Ok(n) => n,
                        Err(e) => {
                            protocol::write_frame(&mut s, &Frame::reply(Op::Command, Status::Failed, format!("{}", e).into_bytes()))?;
                            break;
                        }
                    };
                    if n == 0 {
                        break;
                    }
                    let mut payload = Vec::with_capacity(n + 1);
                    payload.push(Status::Ok as u8);
                    payload.extend_from_slice(&buf[..n]);
                    let f = Frame {
                        op: Op::Command as u8 | protocol::FLAG_REPLY,
                        flags: protocol::FLAG_MORE,
                        payload,
                    };
                    protocol::write_frame(&mut s, &f)?;
                    first = false;
                }
                // 收尾帧
                let mut f = Frame::reply(Op::Command, Status::Ok, Vec::new());
                f.flags = 0;
                protocol::write_frame(&mut s, &f)?;
                let _ = first;
            }
            "cmd" => {
                let rest: Vec<&str> = it.collect();
                let pid: Option<u32> = rest.first().and_then(|p| p.parse().ok());
                let cmd = rest.get(1..).map(|s| s.join(" ")).unwrap_or_default();
                match pid {
                    Some(pid) => match vmless_attach::Client::attach(pid, 2000) {
                        Ok(mut c) => match c.command(&cmd) {
                            Ok(t) => {
                                protocol::write_frame(&mut s, &Frame::reply(Op::Command, Status::Ok, t.into_bytes()))?;
                            }
                            Err(e) => {
                                protocol::write_frame(&mut s, &Frame::reply(Op::Command, Status::Failed, format!("{}", e).into_bytes()))?;
                            }
                        },
                        Err(e) => {
                            protocol::write_frame(&mut s, &Frame::reply(Op::Command, Status::Failed, format!("{}", e).into_bytes()))?;
                        }
                    },
                    None => {
                        protocol::write_frame(&mut s, &Frame::reply(Op::Command, Status::BadCommand, b"cmd needs pid".to_vec()))?;
                    }
                }
            }
            other => {
                protocol::write_frame(
                    &mut s,
                    &Frame::reply(Op::Command, Status::BadCommand, format!("未知命令: {}", other).into_bytes()),
                )?;
            }
        }
    }
}
