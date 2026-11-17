//! `rmiregistry` —— 名字 ↔ 远程引用 的注册表服务（监听 TCP，一个连接一个线程）。
//!
//! **协议**：真 JDK 用 JRMP（Java Remote Method Protocol + 序列化），我们没有 stub
//! 序列化层，所以跑的是本 crate 定义的**行协议**（`--help` 里写全）。注册表本身是真的：
//! 能 bind/lookup/unbind/rebind/list，进程退出前一直持有状态，`nc`/`telnet` 就能操作。
//!
//! 边界：JDK 自带的 `rmi` 客户端（JRMP）连不上这个服务 —— 它期待 Java 序列化的
//! 调用头。要用标准 JRMP 得先有 RMI stub 序列化，那属于另一条工作线。

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use vmless_tools::cli::Args;
use vmless_tools::{die, usage_err, EXIT_USAGE};

const TOOL: &str = "rmiregistry";

fn help() {
    println!("Usage: rmiregistry [port]");
    println!();
    println!("开始在当前主机上运行远程对象注册表。port 默认 1099。");
    println!();
    println!("  -h, -?, --help   打印这段帮助");
    println!("  -J<option>       忽略（真 rmiregistry 把它传给 JVM）");
    println!();
    println!("协议（本 crate 定义的行协议，UTF-8、每行一条命令，响应也是行）：");
    println!("  BIND <name> <ref>      绑定；名字已存在则报 ERR exists");
    println!("  REBIND <name> <ref>    覆盖绑定");
    println!("  LOOKUP <name>          查到回 `REF <ref>`，否则 `NOTFOUND`");
    println!("  UNBIND <name>          解绑；不存在回 `NOTFOUND`");
    println!("  LIST                   每个绑定一行 `NAME <name> <ref>`，最后一行 `END`");
    println!("  QUIT                   关闭连接");
    println!();
    println!("注意：这是**注册表本身**的实现，不是 JRMP。JDK 的 rmi 客户端（走 Java 序列化的");
    println!("JRMP 调用头）连不上它 —— 我们还没有 RMI stub 的序列化层。");
}

fn main() {
    vmless_tools::install_pipe_guard();
    let args = Args::new();
    if args.wants_help() {
        help();
        return;
    }
    if args.wants_version() {
        vmless_tools::print_version(TOOL, "25");
        return;
    }
    let mut port = 1099u16;
    for a in &args.raw {
        if a.starts_with("-J") || a == "-h" {
            continue;
        }
        if a.starts_with('-') {
            eprintln!("{TOOL}: 不认识的选项 {a}");
            std::process::exit(EXIT_USAGE);
        }
        match a.parse::<u16>() {
            Ok(p) => port = p,
            Err(_) => usage_err(TOOL, &format!("端口不合法：{a}")),
        }
    }

    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => l,
        Err(e) => die(TOOL, &format!("监听 0.0.0.0:{port} 失败: {e}")),
    };
    let table: Arc<Mutex<BTreeMap<String, String>>> = Arc::new(Mutex::new(BTreeMap::new()));
    println!("{TOOL}: 监听 0.0.0.0:{port}（行协议，见 --help）");
    for conn in listener.incoming() {
        match conn {
            Ok(s) => {
                let t = table.clone();
                std::thread::spawn(move || {
                    if let Err(e) = serve(s, t) {
                        eprintln!("{TOOL}: 连接处理结束: {e}");
                    }
                });
            }
            Err(e) => eprintln!("{TOOL}: accept 失败: {e}"),
        }
    }
}

fn serve(stream: TcpStream, table: Arc<Mutex<BTreeMap<String, String>>>) -> Result<(), String> {
    let mut out = stream.try_clone().map_err(|e| e.to_string())?;
    let reader = BufReader::new(stream);
    let remote = out
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "-".into());
    println!("{TOOL}: 连接 {remote}");
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        let mut it = l.splitn(3, char::is_whitespace);
        let cmd = it.next().unwrap_or("").to_ascii_uppercase();
        let name = it.next().unwrap_or("").trim().to_string();
        let r#ref = it.next().unwrap_or("").trim().to_string();
        let reply = match cmd.as_str() {
            "BIND" => {
                if name.is_empty() || r#ref.is_empty() {
                    "ERR usage: BIND <name> <ref>".to_string()
                } else {
                    let mut t = table.lock().map_err(|_| "注册表锁中毒")?;
                    if t.contains_key(&name) {
                        "ERR exists".to_string()
                    } else {
                        t.insert(name.clone(), r#ref.clone());
                        "OK".to_string()
                    }
                }
            }
            "REBIND" => {
                if name.is_empty() || r#ref.is_empty() {
                    "ERR usage: REBIND <name> <ref>".to_string()
                } else {
                    table
                        .lock()
                        .map_err(|_| "注册表锁中毒")?
                        .insert(name.clone(), r#ref.clone());
                    "OK".to_string()
                }
            }
            "LOOKUP" => {
                let t = table.lock().map_err(|_| "注册表锁中毒")?;
                match t.get(&name) {
                    Some(v) => format!("REF {v}"),
                    None => "NOTFOUND".to_string(),
                }
            }
            "UNBIND" => {
                let mut t = table.lock().map_err(|_| "注册表锁中毒")?;
                if t.remove(&name).is_some() {
                    "OK".to_string()
                } else {
                    "NOTFOUND".to_string()
                }
            }
            "LIST" => {
                let t = table.lock().map_err(|_| "注册表锁中毒")?;
                let mut s = String::new();
                for (k, v) in t.iter() {
                    s.push_str(&format!("NAME {k} {v}\n"));
                }
                s.push_str("END");
                s
            }
            "QUIT" | "EXIT" => {
                let _ = out.write_all(b"BYE\n");
                break;
            }
            other => format!("ERR unknown-command {other}"),
        };
        // 多行响应（LIST）已经带好了换行，其余补一个
        let payload = if reply.ends_with('\n') || reply == "END" {
            format!("{reply}\n")
        } else {
            format!("{reply}\n")
        };
        out.write_all(payload.as_bytes()).map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())?;
    }
    println!("{TOOL}: 连接 {remote} 关闭");
    Ok(())
}
