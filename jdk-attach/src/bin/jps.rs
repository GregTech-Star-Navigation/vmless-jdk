//! `jps` —— 列出正在跑的 VMless VM。
//!
//! 数据源是 perfdata 文件（`sun.rt.javaCommand` / `java.rt.vmArgs`），**不 attach**
//! —— 与真 jps 一样，所以 `jps` 永远不会被慢/挂死的目标拖住。

use vmless_attach::registry;
use vmless_attach::tool::{self, exit};

const USAGE: &str = "用法: jps [-q] [-mlvV] [<hostid>]

  -q  只输出 pid
  -m  输出 main 方法的参数
  -l  输出完整主类名（含包名）
  -v  输出 JVM 参数
  -V  只输出 pid 与 JVM 参数（不输出主类）
  -h, --help  帮助

<hostid> 本实现只支持本地（省略）。
";

struct Opts {
    quiet: bool,
    long: bool,
    main_args: bool,
    vm_args: bool,
    only_vm_args: bool,
}

fn main() {
    let mut o = Opts { quiet: false, long: false, main_args: false, vm_args: false, only_vm_args: false };
    let mut host: Option<String> = None;
    for a in std::env::args().skip(1) {
        if a == "-h" || a == "--help" || a == "-?" {
            tool::print_usage(USAGE);
        }
        if let Some(rest) = a.strip_prefix("-J") {
            let _ = rest;
            continue;
        }
        if a.starts_with('-') && a.len() > 1 {
            for c in a.chars().skip(1) {
                match c {
                    'q' => o.quiet = true,
                    'l' => o.long = true,
                    'm' => o.main_args = true,
                    'v' => o.vm_args = true,
                    'V' => o.only_vm_args = true,
                    _ => tool::usage(USAGE),
                }
            }
            continue;
        }
        host = Some(a);
    }
    if let Some(h) = host {
        // 真 jps 支持远程，本实现不支持（jstatd 只服务 perfdata，不服务进程列表）
        tool::die_msg(exit::USAGE, &format!("只支持本地主机；收到 hostid={}", h));
    }

    // 顺手清掉死进程留下的 perfdata（否则会显示已经不存在的 VM）
    let _ = registry::sweep_stale();

    let pids = tool::list_local_vms();
    for pid in pids {
        if o.quiet {
            tool::out(&format!("{}\n", pid));
            continue;
        }
        let pd = match tool::read_perf(pid) {
            Ok(d) => d,
            Err(_) => {
                tool::out(&format!("{} -- process information unavailable\n", pid));
                continue;
            }
        };
        let cmd = pd.get_str("sun.rt.javaCommand").unwrap_or("").trim().to_string();
        if cmd.is_empty() {
            tool::out(&format!("{} -- main class information unavailable\n", pid));
            continue;
        }
        let (class, args) = split_command(&cmd);
        let mut line = format!("{} ", pid);
        if !o.only_vm_args {
            if o.long {
                line.push_str(&class);
            } else {
                line.push_str(&short_name(&class));
            }
            if o.main_args && !args.is_empty() {
                line.push(' ');
                line.push_str(&args);
            }
        }
        if o.vm_args || o.only_vm_args {
            let vm = pd.get_str("java.rt.vmArgs").unwrap_or("").trim();
            if !vm.is_empty() {
                if !line.ends_with(' ') {
                    line.push(' ');
                }
                line.push_str(vm);
            }
        }
        tool::out(&format!("{}\n", line.trim_end()));
    }
}

/// `sun.rt.javaCommand` 拆成「主类」和「主类参数」。
///
/// 真 HotSpot 里这个字符串就是启动命令行（`主类 参数...`），所以按第一个空格切。
fn split_command(cmd: &str) -> (String, String) {
    match cmd.split_once(char::is_whitespace) {
        Some((c, a)) => (c.to_string(), a.trim().to_string()),
        None => (cmd.to_string(), String::new()),
    }
}

/// 非 `-l` 模式下真 jps 只显示最后一段：`a.b.C` → `C`，`jdk.jcmd/sun.tools.jps.Jps` → `Jps`。
fn short_name(class: &str) -> String {
    let last = class.rsplit(['.', '/']).next().unwrap_or(class);
    last.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_shorten() {
        let (c, a) = split_command("com.example.Main --flag 1");
        assert_eq!(c, "com.example.Main");
        assert_eq!(a, "--flag 1");
        assert_eq!(short_name("com.example.Main"), "Main");
        assert_eq!(short_name("jdk.jcmd/sun.tools.jps.Jps"), "Jps");
        let (c2, a2) = split_command("Main");
        assert_eq!(c2, "Main");
        assert_eq!(a2, "");
    }
}
