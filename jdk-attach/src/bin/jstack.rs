//! `jstack` —— 线程栈。
//!
//! 输出与真 jstack 一致：**第一行是时间戳**，接着 `Full thread dump ...`。
//! （`jcmd Thread.print` 没有那行时间戳 —— 所以时间戳由本工具加，而不是服务端。）

use vmless_attach::tool::{self, exit};

const USAGE: &str = "用法: jstack [-l] <pid>

  -l   同时打印 ownable synchronizers 与 Java 级死锁检测结果
  -t <ms>  超时，默认 2000
";

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (argv, timeout) = vmless_attach::client::take_timeout(&argv);
    if argv.is_empty() {
        tool::usage(USAGE);
    }
    if argv[0] == "-h" || argv[0] == "--help" {
        tool::print_usage(USAGE);
    }
    let mut locks = false;
    let mut pid: Option<u32> = None;
    for a in &argv {
        if a == "-l" {
            locks = true;
        } else if let Ok(p) = a.parse::<u32>() {
            pid = Some(p);
        } else if a != "-J" && !a.starts_with("-J") {
            tool::die_msg(exit::USAGE, &format!("不认识的参数: {}", a));
        }
    }
    let pid = match pid {
        Some(p) => p,
        None => tool::usage(USAGE),
    };
    let mut c = tool::attach(pid, timeout);

    // 时间戳：与真 jstack 一样的 `YYYY-MM-DD HH:MM:SS`
    tool::out(&format!("{}\n", timestamp()));

    let cmd = if locks { "Thread.print -l" } else { "Thread.print" };
    match c.command(cmd) {
        Ok(t) => tool::out(&vmless_attach::strip_pid_header(&t)),
        Err(e) => tool::die(&e),
    }
}

fn timestamp() -> String {
    match vmless_attach::sys::local_time() {
        Some(t) => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            t.year, t.month, t.day, t.hour, t.min, t.sec
        ),
        None => "0000-00-00 00:00:00".to_string(),
    }
}
