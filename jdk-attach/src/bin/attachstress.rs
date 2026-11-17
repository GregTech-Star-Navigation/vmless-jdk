//! `attachstress` —— attach 通道的压力/健壮性自检。
//!
//! 存在的原因：**"不 panic、不泄漏、并发安全"这三条必须能跑出来**，不能靠读代码保证。
//!
//! ```text
//! attachstress --probe <pid>           1000 次 attach/detach，报告句柄数变化
//! attachstress --endpoint <pid>        打印端点（port + token），用于手工验证
//! attachstress --bad-token <pid>       用错令牌握手（期望 exit 3 + 明确报错）
//! attachstress --bad-version <pid>     用错协议版本握手（期望 exit 3）
//! attachstress --partial <pid>         只发半帧然后干等（期望 exit 4 超时）
//! attachstress --concurrent <pid> <n>  n 个并发客户端同时执行命令
//! attachstress --missing <pid>         连一个不存在的 pid（期望 exit 2）
//! ```

use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use vmless_attach::protocol::{self, Frame, Op, Status, VERSION};
use vmless_attach::registry::{self, Endpoint};
use vmless_attach::tool;
use vmless_attach::Client;

const USAGE: &str = "用法: attachstress <mode> <pid> [n]

  --probe <pid>        1000 次 attach/detach，报告句柄数与耗时
  --endpoint <pid>     打印端点
  --bad-token <pid>    错令牌
  --bad-version <pid>  错协议版本
  --partial <pid>      半帧后干等（测超时）
  --concurrent <pid> <n>  并发 n 个客户端
  --missing <pid>      目标不存在
";

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.len() < 2 {
        tool::usage(USAGE);
    }
    let mode = argv[0].clone();
    let pid: u32 = match argv[1].parse() {
        Ok(p) => p,
        Err(_) => tool::die_msg(tool::exit::USAGE, "pid 必须是数字"),
    };
    match mode.as_str() {
        "--probe" => probe(pid),
        "--endpoint" => {
            match Endpoint::read(pid) {
                Some(ep) => tool::out(&format!("port={}\ntoken={}\n", ep.port, ep.token)),
                None => tool::die_msg(tool::exit::NO_VM, "读不到端点（perfdata 里没有 vmless.attach.*）"),
            }
        }
        "--bad-token" => bad_handshake(pid, VERSION, "deadbeefdeadbeefdeadbeefdeadbeef"),
        "--bad-version" => {
            let ep = match Endpoint::read(pid) {
                Some(e) => e,
                None => tool::die_msg(tool::exit::NO_VM, "读不到端点"),
            };
            bad_handshake(pid, 99, &ep.token)
        }
        "--partial" => partial(pid),
        "--concurrent" => {
            let n: usize = argv.get(2).and_then(|v| v.parse().ok()).unwrap_or(8);
            concurrent(pid, n)
        }
        "--missing" => {
            // 走正常客户端路径，期望 AttachError::NoSuchVm
            match Client::attach(pid, 2000) {
                Ok(_) => tool::die_msg(tool::exit::REMOTE, "居然连上了不存在的 VM？"),
                Err(e) => {
                    tool::out(&format!("预期失败: {}（exit={}）\n", e, e.exit_code()));
                    std::process::exit(e.exit_code())
                }
            }
        }
        other => tool::die_msg(tool::exit::USAGE, &format!("不认识的模式: {}", other)),
    }
}

fn connect(pid: u32) -> (TcpStream, Endpoint) {
    let ep = match Endpoint::read(pid) {
        Some(e) => e,
        None => tool::die_msg(tool::exit::NO_VM, "读不到端点（目标不是 VMless VM 或已退出）"),
    };
    let addr = SocketAddr::from(([127, 0, 0, 1], ep.port));
    let s = match TcpStream::connect_timeout(&addr, Duration::from_millis(2000)) {
        Ok(s) => s,
        Err(e) => tool::die_msg(tool::exit::IO, &format!("连不上 {}: {}", addr, e)),
    };
    let _ = s.set_read_timeout(Some(Duration::from_millis(2000)));
    let _ = s.set_write_timeout(Some(Duration::from_millis(2000)));
    (s, ep)
}

/// 手工构造帧，可以指定任意协议版本。
fn raw_frame(s: &mut TcpStream, version: u8, op: u8, flags: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut head = [0u8; 8];
    head[0] = protocol::MAGIC;
    head[1] = version;
    head[2] = op;
    head[3] = flags;
    head[4..8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    s.write_all(&head)?;
    s.write_all(payload)?;
    s.flush()
}

fn bad_handshake(pid: u32, version: u8, token: &str) {
    let (mut s, _ep) = connect(pid);
    if let Err(e) = raw_frame(&mut s, version, Op::Hello as u8, 0, token.as_bytes()) {
        tool::die_msg(tool::exit::IO, &format!("发送握手失败: {}", e));
    }
    match protocol::read_frame(&mut s) {
        Ok(f) => {
            let st = f.status();
            let body = f.body_string();
            tool::out(&format!("服务端回复: status={:?} body={:?}\n", st, body.trim()));
            match st {
                Status::BadToken => std::process::exit(3),
                Status::BadVersion => std::process::exit(3),
                Status::Ok => {
                    tool::die_msg(tool::exit::REMOTE, "本该拒绝的握手却成功了 —— 这是安全问题")
                }
                _ => std::process::exit(6),
            }
        }
        Err(e) => tool::die_msg(tool::exit::PROTOCOL, &format!("读回复失败（版本不符时对端可能直接断链）: {}", e)),
    }
}

fn partial(pid: u32) {
    let (mut s, ep) = connect(pid);
    // 声明 32 字节载荷，只发 4 字节，然后干等 —— 客户端应在 read timeout 后报超时
    let mut head = [0u8; 8];
    head[0] = protocol::MAGIC;
    head[1] = VERSION;
    head[2] = Op::Hello as u8;
    head[4..8].copy_from_slice(&32u32.to_le_bytes());
    let _ = s.write_all(&head);
    let _ = s.write_all(b"abcd");
    let _ = s.flush();
    let t0 = Instant::now();
    let mut buf = [0u8; 64];
    let r = std::io::Read::read(&mut s, &mut buf);
    let ms = t0.elapsed().as_millis();
    match r {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
            tool::out(&format!(
                "半帧后 {}ms 超时（端点 {}），符合预期 -> exit 4\n",
                ms, ep.port
            ));
            std::process::exit(4)
        }
        Ok(n) => tool::die_msg(tool::exit::PROTOCOL, &format!("居然收到 {} 字节", n)),
        Err(e) => tool::die_msg(tool::exit::IO, &format!("{}", e)),
    }
}

/// 1000 次 attach/detach：统计句柄数与成功次数。
fn probe(pid: u32) {
    const N: usize = 1000;
    // 先热身 100 次：把 std/运行时的一次性句柄（DLL、堆、线程池…）都吃掉，
    // 这样 h0 -> h1 的差值才是"每次 attach 是否泄漏"的信号。
    for _ in 0..100 {
        let _ = Client::attach(pid, 2000).map(|mut c| c.command("VM.version").is_ok());
    }
    let h0 = vmless_attach::sys::handle_count();
    let t0 = Instant::now();
    let mut ok = 0usize;
    let mut fail = 0usize;
    for _ in 0..N {
        match Client::attach(pid, 2000) {
            Ok(mut c) => match c.command("VM.version") {
                Ok(t) if t.contains("VMless") => ok += 1,
                Ok(_) => ok += 1,
                Err(_) => fail += 1,
            },
            Err(e) => {
                fail += 1;
                if fail == 1 {
                    let _ = writeln!(std::io::stderr(), "第一次失败: {}", e);
                }
            }
        }
    }
    let elapsed = t0.elapsed();
    let h1 = vmless_attach::sys::handle_count();
    tool::out(&format!(
        "attach/detach {} 次: 成功 {} 失败 {} 耗时 {:?}\n句柄数: {} -> {} （变化 {}）\n",
        N,
        ok,
        fail,
        elapsed,
        h0.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
        h1.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
        match (h0, h1) {
            (Some(a), Some(b)) => b as i64 - a as i64,
            _ => 0,
        }
    ));
    if fail > 0 {
        std::process::exit(6);
    }
    // 句柄涨了超过 20 个就说明泄漏（std 自己也会有抖动，给一点余量）
    if let (Some(a), Some(b)) = (h0, h1) {
        if b > a + 20 {
            std::process::exit(7);
        }
    }
}

/// 并发 attach：n 个线程各自 attach + 执行命令，全部必须成功。
fn concurrent(pid: u32, n: usize) {
    let ok = Arc::new(AtomicUsize::new(0));
    let bad = Arc::new(AtomicUsize::new(0));
    let mut hs = Vec::new();
    for i in 0..n {
        let ok = ok.clone();
        let bad = bad.clone();
        hs.push(std::thread::spawn(move || {
            // 一半走短暂连接，一半走长连接并发读
            match Client::attach(pid, 3000) {
                Ok(mut c) => {
                    let cmd = if i % 3 == 0 { "VM.version" } else if i % 3 == 1 { "GC.heap_info" } else { "Thread.print" };
                    match c.command(cmd) {
                        Ok(_) => {
                            ok.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(_) => {
                            bad.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                }
                Err(_) => {
                    bad.fetch_add(1, Ordering::SeqCst);
                }
            }
        }));
    }
    for h in hs {
        let _ = h.join();
    }
    let o = ok.load(Ordering::SeqCst);
    let b = bad.load(Ordering::SeqCst);
    tool::out(&format!("并发 {} 个客户端: 成功 {} 失败 {}\n", n, o, b));
    let _ = registry::perf_dir();
    if b > 0 {
        std::process::exit(6);
    }
}
