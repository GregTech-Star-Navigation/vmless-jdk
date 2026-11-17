//! `jhsdb` —— 服务我们自己 VM 的「调试器等价物」。
//!
//! # 为什么不照抄真 `jhsdb`
//!
//! 真 `jhsdb`（Serviceability Agent）靠 **ptrace / 调试器**去读别的进程的内存，
//! Windows 上没有 ptrace；即使有，attach 一个「别人家的 JVM」也要求目标端配合。
//!
//! 这里的等价能力是：**目标 VM 自己愿意上报**（`vmless-attach` 协议），
//! 我们只是把探测结果按 `jhsdb jmap/jstack/jinfo` 的版式打出来。
//! 结论就是：**只能服务我们自己的 VM**（VMless 的 JVM），对真 HotSpot 无效 ——
//! 真 HotSpot 请用 JDK 自带的 `jhsdb`。
//!
//! # 支持的命令
//!
//! | 命令 | 等价于 | 数据来源 |
//! | --- | --- | --- |
//! | `jhsdb jmap --histo --pid P` | `jmap -histo` | `GC.class_histogram` |
//! | `jhsdb jmap --heap --pid P` | `jmap -heap` | `GC.heap_info` |
//! | `jhsdb jstack --pid P` | `jstack` | `Thread.print` |
//! | `jhsdb jinfo --pid P` | `jinfo` | `VM.system_properties` + `VM.flags` |
//! | `jhsdb --heap --pid P` | `jhsdb --heap`（堆布局摘要） | `GC.heap_info` + 直方图 |
//! | `jhsdb --locks --pid P` | 锁摘要 | `Thread.print` 解析 |
//!
//! # 与真 `jhsdb` 的差别（help 里也写着）
//!
//! - 只能连**本机、同用户、跑着 vmless-attach 的 VM**（不是任意 JVM）
//! - 没有 `--core`（core dump）模式，没有 SA 的反汇编/寄存器视图
//! - `--heap` 是**上报的堆布局摘要**，不是 SA 从原始内存里刨出来的对象图

use std::io::Write;

use vmless_attach::client::{AttachError, Client};
use vmless_attach::fmt;
use vmless_attach::heap::ClassHisto;
use vmless_attach::protocol::Op;

const USAGE: &str = "\
用法: jhsdb <子命令> [选项] --pid <pid>

子命令:
  jmap --histo   --pid <pid>   堆直方图（等价 jmap -histo）
  jmap --heap    --pid <pid>   堆布局摘要（等价 jmap -heap）
  jmap --clstats --pid <pid>   类加载器统计
  jstack         --pid <pid>   线程栈（等价 jstack）
  jinfo         --pid <pid>    系统属性与 VM 参数（等价 jinfo）
  --heap         --pid <pid>   堆布局摘要（jhsdb 风格）
  --locks        --pid <pid>   锁摘要（谁在等谁）
  --help                       本帮助

说明: 本工具**只服务 VMless 的 JVM**（目标要跑着 vmless-attach 的服务端）。
      真 HotSpot 进程请用 JDK 自带的 jhsdb —— 那需要调试器（ptrace），
      Windows 上不存在。

通用选项:
  --pid <pid>    目标进程号（必需）
  --timeout <ms> attach 超时，默认 2000
";

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(run(&argv));
}

fn run(argv: &[String]) -> i32 {
    if argv.is_empty() || argv[0] == "--help" || argv[0] == "-h" || argv[0] == "help" {
        print!("{}", USAGE);
        return if argv.is_empty() { 1 } else { 0 };
    }
    let mut pid: Option<u32> = None;
    let mut timeout = 2000u64;
    let mut words: Vec<String> = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--pid" | "-p" => {
                i += 1;
                pid = argv.get(i).and_then(|v| v.parse().ok());
                if pid.is_none() {
                    return die("--pid 需要一个十进制进程号");
                }
            }
            "--timeout" => {
                i += 1;
                timeout = argv.get(i).and_then(|v| v.parse().ok()).unwrap_or(timeout);
            }
            other => words.push(other.to_string()),
        }
        i += 1;
    }
    let pid = match pid {
        Some(p) => p,
        None => return die("缺少 --pid（本工具只能操作跑着 vmless-attach 的本机 VM）"),
    };
    let mode = match words.first().map(|s| s.as_str()) {
        Some("jmap") => match words.get(1).map(|s| s.as_str()) {
            Some("--histo") | Some("-histo") => Mode::Histo,
            Some("--heap") | Some("-heap") => Mode::Heap,
            Some("--clstats") | Some("-clstats") => Mode::Clstats,
            _ => return die("jmap 需要 --histo / --heap / --clstats"),
        },
        Some("jstack") | Some("-l") => Mode::Stack,
        Some("jinfo") => Mode::Info,
        Some("--heap") | Some("heap") => Mode::Heap,
        Some("--locks") | Some("locks") => Mode::Locks,
        Some(other) => return die(&format!("未知子命令 {}", other)),
        None => return die("缺少子命令（--help 看用法）"),
    };

    match exec(pid, timeout, mode) {
        Ok(text) => {
            out(&text);
            0
        }
        Err(e) => {
            eprintln!("错误: {}", e);
            e.exit_code()
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Mode {
    Histo,
    Heap,
    Clstats,
    Stack,
    Info,
    Locks,
}

fn exec(pid: u32, timeout_ms: u64, mode: Mode) -> Result<String, AttachError> {
    let mut c = Client::attach(pid, timeout_ms)?;
    match mode {
        Mode::Histo => {
            let body = c.request_bin(Op::HeapHisto, &[])?;
            match ClassHisto::from_wire(&body) {
                Some(v) => Ok(fmt::histogram_table(&v)),
                None => Err(AttachError::Protocol(
                    "直方图数据格式不对（版本不匹配？）".into(),
                )),
            }
        }
        Mode::Clstats => {
            let body = c.request_bin(Op::ClassLoaders, &[])?;
            match vmless_attach::heap::LoaderStat::from_wire(&body) {
                Some(v) => Ok(vmless_attach::fmt::clstats_table(&v)),
                None => Ok(vmless_attach::strip_pid_header(
                    &c.command("GC.class_stats")?,
                )),
            }
        }
        Mode::Heap => {
            // `jhsdb --heap`：先给 SA 风格的表头，再贴 VM 上报的堆布局 + 直方图摘要
            let info = vmless_attach::strip_pid_header(&c.command("GC.heap_info")?);
            let body = c.request_bin(Op::HeapHisto, &[])?;
            let mut text = String::new();
            text.push_str(&format!(
                "\nAttaching to pid {} (VMless JVM, 非 HotSpot)\n\n",
                pid
            ));
            text.push_str("Heap layout (reported by the VM)");
            text.push('\n');
            text.push_str(&info);
            if let Some(v) = ClassHisto::from_wire(&body) {
                let live: u64 = v.iter().map(|c| c.bytes as u64).sum();
                let instances: u64 = v.iter().map(|c| c.instances as u64).sum();
                text.push_str(&format!(
                    "\nSummary: {} classes, {} live instances, {} live bytes\n",
                    v.len(),
                    instances,
                    live
                ));
            }
            Ok(text)
        }
        Mode::Stack => Ok(vmless_attach::strip_pid_header(&c.command("Thread.print")?)),
        Mode::Info => {
            let props = vmless_attach::strip_pid_header(&c.command("VM.system_properties")?);
            let flags = vmless_attach::strip_pid_header(&c.command("VM.flags")?);
            Ok(format!("{}\n{}", props, flags))
        }
        Mode::Locks => {
            let dump = vmless_attach::strip_pid_header(&c.command("Thread.print")?);
            Ok(summarize_locks(&dump))
        }
    }
}

/// 从线程 dump 里摘出「谁在等谁」。
///
/// 认的行形态（HotSpot 版式）：
/// - `"name" #id ...` 线程头
/// - `\t- waiting to lock <0x...> (a Foo)`
/// - `\t- locked <0x...> (a Foo)`
fn summarize_locks(dump: &str) -> String {
    #[derive(Default)]
    struct T {
        name: String,
        id: String,
        waiting: Vec<String>,
        locked: Vec<String>,
    }
    let mut list: Vec<T> = Vec::new();
    for line in dump.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix('"') {
            if let Some((name, after)) = rest.split_once('"') {
                let id = after
                    .trim_start()
                    .strip_prefix('#')
                    .map(|s| s.chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
                    .unwrap_or_default();
                list.push(T {
                    name: name.to_string(),
                    id,
                    ..Default::default()
                });
            }
            continue;
        }
        let target = if let Some(r) = t.strip_prefix("- waiting to lock ") {
            Some((r, true))
        } else if let Some(r) = t.strip_prefix("- waiting on ") {
            Some((r, true))
        } else if let Some(r) = t.strip_prefix("- locked ") {
            Some((r, false))
        } else {
            None
        };
        if let Some((r, waiting)) = target {
            if let Some(cur) = list.last_mut() {
                if waiting {
                    cur.waiting.push(r.trim().to_string());
                } else {
                    cur.locked.push(r.trim().to_string());
                }
            }
        }
    }
    let mut o = String::new();
    o.push_str("Lock summary (from Thread.print)\n");
    let mut any = false;
    for t in &list {
        if t.waiting.is_empty() && t.locked.is_empty() {
            continue;
        }
        any = true;
        o.push_str(&format!("\"{}\" #{}:\n", t.name, t.id));
        for w in &t.waiting {
            o.push_str(&format!("  waiting: {}\n", w));
        }
        for l in &t.locked {
            o.push_str(&format!("  holding: {}\n", l));
        }
    }
    if !any {
        o.push_str("  (没有任何线程报告锁等待或持有)\n");
    }
    o
}

fn die(msg: &str) -> i32 {
    eprintln!("错误: {}", msg);
    1
}

fn out(s: &str) {
    let mut so = std::io::stdout();
    let _ = so.write_all(s.as_bytes());
    let _ = so.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_summary() {
        let dump = "\"main\" #1 prio=5\n\t- waiting to lock <0x00000000000000aa> (a java.lang.Object)\n\t- locked <0x00000000000000bb> (a demo.Busy)\n\n\"worker\" #2 prio=5\n\t- locked <0x00000000000000aa> (a java.lang.Object)\n";
        let s = summarize_locks(dump);
        assert!(s.contains("\"main\" #1:"));
        assert!(s.contains("waiting: <0x00000000000000aa> (a java.lang.Object)"));
        assert!(s.contains("holding: <0x00000000000000bb> (a demo.Busy)"));
        assert!(s.contains("\"worker\" #2:"));
    }
}
