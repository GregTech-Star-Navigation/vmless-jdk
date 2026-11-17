//! `jcmd` —— 给目标 VM 发诊断命令。
//!
//! 与真 jcmd 的差别只有一处（有意的）：不带参数时真 jcmd 会**先用 attach 探一遍**
//! 再列出 pid，本实现直接读 perfdata（更快，也不会卡在被探的目标上）。

use vmless_attach::tool::{self, exit};
use vmless_attach::client::DEFAULT_TIMEOUT_MS;

const USAGE: &str = "用法: jcmd [-l] [-h] [<pid> | <main-class>] <command...>

  jcmd                     列出本机所有 VMless VM 的 pid 与主类
  jcmd -l                  同上（与真 jcmd 兼容）
  jcmd <pid>               列出该 VM 支持的命令
  jcmd <pid> <command>     执行命令（多段命令如 `VM.native_memory summary` 要加引号）
  jcmd <main-class> <cmd>  按主类匹配（唯一时生效）
  -t <ms>                  超时，默认 2000

  PerfCounter.print 会打印全部计数器，可与 `jstat` 交叉验证。
";

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (argv, timeout) = vmless_attach::client::take_timeout(&argv);

    if argv.is_empty() || argv[0] == "-l" {
        list();
        return;
    }
    if argv[0] == "-h" || argv[0] == "--help" {
        tool::print_usage(USAGE);
    }
    if argv[0] == "-J" || argv[0].starts_with("-J") {
        // 兼容真 jcmd 的 -J 透传
    }
    let target = argv[0].clone();
    let cmd = argv[1..].join(" ");

    // pid 或主类
    let pid = match target.parse::<u32>() {
        Ok(p) => p,
        Err(_) => match find_by_main_class(&target) {
            Some(p) => p,
            None => tool::die_msg(
                exit::NO_VM,
                &format!("找不到 pid 或主类为 {:?} 的 VMless VM", target),
            ),
        },
    };
    let mut c = tool::attach(pid, timeout);
    let cmd = if cmd.trim().is_empty() { "help".to_string() } else { cmd };
    match c.command(&cmd) {
        Ok(text) => tool::out(&text),
        Err(e) => tool::die(&e),
    }
    let _ = DEFAULT_TIMEOUT_MS;
}

fn list() {
    let _ = vmless_attach::registry::sweep_stale();
    for pid in tool::list_local_vms() {
        let main = tool::read_perf(pid)
            .ok()
            .and_then(|d| d.get_str("sun.rt.javaCommand").map(|s| s.to_string()))
            .unwrap_or_else(|| "-- process information unavailable".to_string());
        let main = main.split_whitespace().next().unwrap_or("").to_string();
        if main.is_empty() {
            tool::out(&format!("{}\n", pid));
        } else {
            tool::out(&format!("{} {}\n", pid, main));
        }
    }
}

fn find_by_main_class(name: &str) -> Option<u32> {
    let mut hits = Vec::new();
    for pid in tool::list_local_vms() {
        if let Ok(d) = tool::read_perf(pid) {
            if let Some(c) = d.get_str("sun.rt.javaCommand") {
                let cls = c.split_whitespace().next().unwrap_or("");
                if cls == name || cls.rsplit(['.', '/']).next() == Some(name) {
                    hits.push(pid);
                }
            }
        }
    }
    if hits.len() == 1 {
        Some(hits[0])
    } else {
        None
    }
}
