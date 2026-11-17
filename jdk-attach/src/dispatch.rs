//! jcmd 命令的分发与渲染。
//!
//! 从 `server.rs` 拆出来的原因：**命令清单是"对外的契约"**，它应该和传输层分开看、
//! 分开改。这里只依赖 `VmProbe` 与 perfdata 文件，不碰 socket。
//!
//! 每条命令的正文**不带** `<pid>:` 头 —— 头由 [`run_command`] 统一加，保证 `jcmd`
//! 的输出版式和真 jcmd 一致（`<pid>:` 然后正文）。

use std::path::Path;
use std::sync::Arc;

use crate::counters::PerfSnapshot;
use crate::heap::ClassHisto;
use crate::perf::{self, PerfValue};
use crate::protocol::Status;
use crate::{ClassStat, VmIdentity, VmProbe, VM_NAME, VM_VERSION};

/// 支持的命令（`jcmd <pid> help` 按这个顺序列）。
pub const COMMANDS: &[&str] = &[
    "GC.class_histogram",
    "GC.class_stats",
    "GC.heap_info",
    "GC.run",
    "PerfCounter.print",
    "Thread.print",
    "VM.command_line",
    "VM.flags",
    "VM.get_flag",
    "VM.native_memory",
    "VM.set_flag",
    "VM.system_properties",
    "VM.uptime",
    "VM.version",
];

const NL: &str = "\n";

/// 追加一行（`&str + &str` 在 Rust 里不合法，统一走这里）。
fn push_line(s: &mut String, t: &str) {
    s.push_str(t);
    s.push_str(NL);
}

/// 执行一条命令，返回 (状态, 正文)。正文已带 `<pid>:` 头。
pub fn run_command(
    pid: u32,
    identity: &VmIdentity,
    probe: &Arc<dyn VmProbe>,
    perf_path: &Path,
    line: &str,
) -> (Status, String) {
    let line = line.trim();
    if line.is_empty() {
        return (Status::BadCommand, "命令为空".to_string());
    }
    let (cmd, rest) = match line.split_once(' ') {
        Some((c, r)) => (c, r.trim()),
        None => (line, ""),
    };
    let head = |body: &str| format!("{}:{}", pid, NL) + body.trim_end_matches(&NL.to_string()) + NL;

    match cmd {
        "help" => {
            let text = if rest.is_empty() { help_text() } else { command_help(rest) };
            (Status::Ok, head(&text))
        }
        _ if cmd.ends_with("_help") => {
            let base = cmd.trim_end_matches("_help");
            (Status::Ok, head(&command_help(base)))
        }
        "VM.version" => (
            Status::Ok,
            head(&format!(
                "{} version {}{}VMless JVM (attach protocol v{})",
                VM_NAME,
                VM_VERSION,
                NL,
                crate::protocol::VERSION
            )),
        ),
        "VM.uptime" => {
            let snap: PerfSnapshot = probe.perf();
            (Status::Ok, head(&format!("{:.3} s", snap.uptime_ms as f64 / 1000.0)))
        }
        "VM.flags" => (Status::Ok, head(&flags_line(identity, probe))),
        "VM.command_line" => {
            let mut s = String::new();
            s.push_str("VM Arguments:");
            s.push_str(NL);
            s.push_str(&format!("jvm_args: {}", identity.jvm_args.join(" ")));
            s.push_str(NL);
            s.push_str(&format!("java_command: {}", identity.java_command));
            s.push_str(NL);
            s.push_str(&format!("java_class_path (initial): {}", identity.class_path));
            s.push_str(NL);
            s.push_str("Launcher Type: SUN_STANDARD");
            (Status::Ok, head(&s))
        }
        "VM.system_properties" => {
            let mut props = identity.system_properties.clone();
            if props.is_empty() {
                // 没显式给属性时，从 perfdata 的 java.property.* 反向还原，
                // 这样 `jinfo -sysprops` 与 `PerfCounter.print` 永远一致
                if let Ok(pd) = perf::read(perf_path) {
                    for (k, v) in pd.counters.iter() {
                        if let Some(name) = k.strip_prefix("java.property.") {
                            if let PerfValue::Str(sval) = v {
                                props.push((name.to_string(), sval.clone()));
                            }
                        }
                    }
                }
            }
            (Status::Ok, head(&crate::fmt::properties_text(&props)))
        }
        "VM.info" => {
            let mut s = String::new();
            push_line(&mut s, &format!("{} version {}", VM_NAME, VM_VERSION));
            s.push_str(&format!("java_command: {}", identity.java_command));
            s.push_str(NL);
            push_line(&mut s, "Launcher Type: SUN_STANDARD");
            s.push_str(NL);
            s.push_str(&crate::fmt::properties_text(&identity.system_properties));
            (Status::Ok, head(&s))
        }
        "VM.native_memory" => {
            if !rest.is_empty() && rest != "summary" {
                return (Status::BadCommand, format!("VM.native_memory 只支持 summary（收到 {:?}）", rest));
            }
            match probe.native_memory() {
                Ok(t) => (Status::Ok, head(&t)),
                Err(e) => (Status::Unsupported, e),
            }
        }
        "VM.get_flag" => {
            if rest.is_empty() {
                return (Status::BadCommand, "VM.get_flag 需要一个标志名".into());
            }
            match probe.get_flag(rest) {
                Some(v) => (Status::Ok, head(&v)),
                None => (Status::Ok, head("null")),
            }
        }
        "VM.set_flag" => {
            let mut it = rest.splitn(2, ' ');
            let name = it.next().unwrap_or("");
            let value = it.next().unwrap_or("");
            if name.is_empty() || value.is_empty() {
                return (Status::BadCommand, "VM.set_flag 需要 <名字> <值>".into());
            }
            match probe.set_flag(name, value) {
                Ok(()) => (Status::Ok, head("OK")),
                Err(e) => (Status::Failed, e),
            }
        }
        "GC.heap_info" => match probe.heap_info() {
            Ok(t) => (Status::Ok, head(&t)),
            Err(e) => (Status::Unsupported, e),
        },
        "GC.run" => match probe.run_gc() {
            Ok(t) => (Status::Ok, head(&t)),
            Err(e) => (Status::Unsupported, e),
        },
        "GC.class_histogram" => match probe.heap_histogram() {
            Ok(mut v) => {
                ClassHisto::sort_by_bytes(&mut v);
                (Status::Ok, head(&crate::fmt::histogram_table(&v)))
            }
            Err(e) => (Status::Unsupported, e),
        },
        "GC.class_stats" => match probe.class_stats() {
            Ok(v) => (Status::Ok, head(&class_stats_table(&v))),
            Err(e) => (Status::Unsupported, e),
        },
        "GC.heap_dump" => (
            Status::BadCommand,
            "GC.heap_dump 走流式通道：请用 `jmap -dump:format=b,file=<f> <pid>`".into(),
        ),
        "Thread.print" => {
            let with_locks = rest.contains("-l");
            match probe.threads() {
                Ok(d) => (Status::Ok, head(&d.render(VM_NAME, VM_VERSION, with_locks))),
                Err(e) => (Status::Unsupported, e),
            }
        }
        "PerfCounter.print" => match perf::read(perf_path) {
            Ok(d) => {
                let mut s = String::new();
                for (k, v) in &d.counters {
                    push_line(&mut s, &format!("{}={}", k, fmt_perf_value(v)));
                }
                (Status::Ok, head(&s))
            }
            Err(e) => (Status::Failed, format!("读 perfdata 失败: {}", e)),
        },
        other => (
            Status::BadCommand,
            format!("命令不存在: {}（用 help 看可用命令）", other),
        ),
    }
}

fn flags_line(identity: &VmIdentity, probe: &Arc<dyn VmProbe>) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (k, v) in probe.flags() {
        if v.is_empty() {
            parts.push(format!("-XX:+{}", k));
        } else if v == "true" {
            parts.push(format!("-XX:+{}", k));
        } else if v == "false" {
            parts.push(format!("-XX:-{}", k));
        } else {
            parts.push(format!("-XX:{}={}", k, v));
        }
    }
    if parts.is_empty() {
        parts = identity.vm_flags.clone();
    }
    if parts.is_empty() {
        return "VMless 目前没有可调的 -XX 标志".to_string();
    }
    parts.join(" ") + " "
}

fn fmt_perf_value(v: &PerfValue) -> String {
    match v {
        PerfValue::Long(x) => x.to_string(),
        PerfValue::Int(x) => x.to_string(),
        PerfValue::Short(x) => x.to_string(),
        PerfValue::Byte(x) => x.to_string(),
        PerfValue::Bool(b) => b.to_string(),
        PerfValue::Char(c) => (*c as char).to_string(),
        PerfValue::Float(f) => format!("{}", f),
        PerfValue::Double(f) => format!("{}", f),
        PerfValue::Str(s) => s.clone(),
    }
}

fn help_text() -> String {
    let mut s = String::new();
    push_line(&mut s, "The following commands are available:");
    for c in COMMANDS {
        s.push_str(c);
        s.push_str(NL);
    }
    for c in COMMANDS {
        s.push_str(&format!("{}_help", c));
        s.push_str(NL);
    }
    s.push_str("For more information about a specific command use 'help <command>'.");
    s
}

fn command_help(cmd: &str) -> String {
    let body = match cmd {
        "GC.class_histogram" => "GC.class_histogram\nPrint histogram of heap",
        "GC.class_stats" => "GC.class_stats\nPrint class statistics",
        "GC.heap_info" => "GC.heap_info\nProvide generic heap info",
        "GC.run" => "GC.run\nRun a garbage collection",
        "PerfCounter.print" => "PerfCounter.print\nPrint all performance counters",
        "Thread.print" => "Thread.print [-l]\nPrint all threads with stacktraces",
        "VM.command_line" => "VM.command_line\nPrint VM command line arguments",
        "VM.flags" => "VM.flags\nPrint VM flags",
        "VM.get_flag" => "VM.get_flag <name>\nPrint one VM flag",
        "VM.native_memory" => "VM.native_memory [summary]\nPrint native memory statistics",
        "VM.set_flag" => "VM.set_flag <name> <value>\nSet one VM flag (fails if not manageable)",
        "VM.system_properties" => "VM.system_properties\nPrint system properties",
        "VM.uptime" => "VM.uptime\nPrint VM uptime",
        "VM.version" => "VM.version\nPrint VM version information",
        _ => return format!("没有这个命令: {}", cmd),
    };
    let mut s = String::new();
    let (name, desc) = body.split_once('\n').unwrap_or((body, ""));
    s.push_str(name);
    s.push_str(NL);
    s.push_str(desc);
    s
}

fn class_stats_table(v: &[ClassStat]) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "{:>9} {:>12} {:>8} {:>10} {:>11}  {}",
        "Instances", "Bytes", "Methods", "CPBytes", "ClassLoader", "Class"
    ));
    s.push_str(NL);
    for c in v {
        s.push_str(&format!(
            "{:>9} {:>12} {:>8} {:>10} {:>11}  {}",
            c.instances, c.bytes, c.methods, c.constant_pool_bytes, c.class_loader, c.name
        ));
        s.push_str(NL);
    }
    s
}
