//! `jps` —— 列出本机的 JVM 进程（pid + 主类名）。
//!
//! 两个数据源，合并去重（同一个 pid 只输出一行）：
//!
//! 1. **launcher 登记表** `<TMP>/vmless_jps/<pid>.txt` —— 我们的 launcher（`javaw` /
//!    `jshell` / `jdb`）启动时登记、退出时删除。这是**真跑起来的 VM 进程**的来源。
//! 2. **hsperfdata** `<TMP>/hsperfdata_<user>/<pid>` —— 挂了 attach 服务端的 VM
//!    （`vmsim`，以及将来 rustjavac 挂 [`vmless_attach`] 之后）写出的 HotSpot v2.0
//!    二进制计数器文件，`sun.rt.javaCommand` / `java.rt.vmArgs` 就在里面。
//!
//! 两个都要读：只读第一个，`vmsim` 这类"无 launcher 的 VM"看不见；只读第二个，
//! 真跑 Java 程序的 `javaw` 看不见 —— 而这两件事在本项目里都真实存在。
//!
//! 顺带一提：hsperfdata 是**真 HotSpot 也在写**的格式，所以本工具能直接列出机器上
//! 真 JVM 的主类名与 `-XX` 参数（实测：Minecraft/NeoForge 那几个进程）。
//!
//! 比真 `jps` 少的部分：远程 hostid（`jstatd` 只服务 perfdata，不服务进程列表）。
//!
//! 死 pid 的登记项/perfdata 会顺手清掉（launcher 被强杀时不会自己删）。

use vmless_tools::cli::Args;
use vmless_tools::jpsreg;
use vmless_tools::EXIT_USAGE;

const TOOL: &str = "jps";

fn help() {
    println!("usage: jps [-q] [-m] [-l] [-v] [-V] [-Joption] [-h]");
    println!();
    println!("  -q 只输出 pid");
    println!("  -m 输出 main 方法的参数");
    println!("  -l 输出完整的主类名");
    println!("  -v 输出启动时给的参数（-J / -XX，来自 perfdata）");
    println!("  -V 只输出 pid（与 -q 相同）");
    println!("  -h, -?, --help 打印这段帮助");
    println!();
    println!("数据源有两个并会合并：① launcher 写的进程登记表（<TMP>/vmless_jps，真跑 Java");
    println!("程序的 javaw/jshell/jdb 走这里）② HotSpot v2.0 格式的 hsperfdata 计数器文件");
    println!("（<TMP>/hsperfdata_<用户>/<pid>，挂了 attach 服务端的 VM 走这里）。");
    println!("不读远程 hostid：jstatd 只服务计数器，不服务进程列表。");
}

/// 一行输出所依赖的全部字段。两个数据源都填进同一个结构，输出侧就不必分支。
#[derive(Default)]
struct Row {
    pid: u32,
    /// 主类名（短名由输出侧再截）
    main: String,
    /// 主类参数
    args: String,
    /// `-J` / `-XX` 之类的 VM 参数（只有 perfdata 有）
    vm_args: String,
    /// 这条来自 perfdata 吗（合并时优先保留它：字段更全）
    from_perf: bool,
}

fn main() {
    vmless_tools::install_pipe_guard();
    let args = Args::new();
    if args.wants_help() {
        help();
        return;
    }
    let known = ["-q", "-m", "-l", "-v", "-V", "-h", "-?", "--help"];
    for a in &args.raw {
        if a.starts_with('-') && !a.starts_with("-J") && !known.contains(&a.as_str()) {
            eprintln!("{TOOL}: 不认识的选项 {a}");
            std::process::exit(EXIT_USAGE);
        }
    }
    let quiet = args.flag("-q") || args.flag("-V");
    let long = args.flag("-l");
    let with_args = args.flag("-m");
    let with_vm_args = args.flag("-v");

    for r in collect() {
        if quiet {
            println!("{}", r.pid);
            continue;
        }
        let mut line = format!("{} ", r.pid);
        if long {
            line.push_str(&r.main);
        } else {
            // 短名：取最后一段（真 jps 也是这么显示）
            let simple = r.main.rsplit(['.', '/']).next().unwrap_or(&r.main);
            line.push_str(simple);
        }
        if with_args && !r.args.is_empty() {
            line.push(' ');
            line.push_str(&r.args);
        }
        if with_vm_args && !r.vm_args.is_empty() {
            line.push(' ');
            line.push_str(&r.vm_args);
        }
        println!("{}", line.trim_end());
    }
}

/// 两个来源合并、去重（同 pid 保留 perfdata 那行）、剔除死进程、按 pid 排序。
fn collect() -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();

    // ① launcher 登记表：只留活的 pid，死项顺手删掉
    for e in jpsreg::list() {
        if !jpsreg::is_alive(e.pid) {
            jpsreg::unregister(e.pid);
            continue;
        }
        rows.push(Row {
            pid: e.pid,
            main: if e.main.is_empty() { e.launcher } else { e.main },
            args: e.args,
            vm_args: String::new(),
            from_perf: false,
        });
    }

    // ② hsperfdata：`tool::list_local_vms` 已经过滤掉死 pid 与不像 perfdata 的文件
    let _ = vmless_attach::registry::sweep_stale();
    for pid in vmless_attach::tool::list_local_vms() {
        let Ok(pd) = vmless_attach::tool::read_perf(pid) else {
            continue;
        };
        let cmd = pd.get_str("sun.rt.javaCommand").unwrap_or("").trim().to_string();
        let (main, pargs) = split_command(&cmd);
        let vm_args = pd.get_str("java.rt.vmArgs").unwrap_or("").trim().to_string();
        rows.push(Row {
            pid,
            main,
            args: pargs,
            vm_args,
            from_perf: true,
        });
    }

    // 去重：同一 pid 只留一行；perfdata 那行字段更全，优先
    rows.sort_by_key(|r| (r.pid, !r.from_perf));
    rows.dedup_by_key(|r| r.pid);
    rows
}

/// `sun.rt.javaCommand` 拆成「主类」和「主类参数」。真 HotSpot 里这个字符串就是
/// 启动命令行（`主类 参数...`），所以按第一个空白切。
fn split_command(cmd: &str) -> (String, String) {
    match cmd.split_once(char::is_whitespace) {
        Some((c, a)) => (c.to_string(), a.trim().to_string()),
        None => (cmd.to_string(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_command_splits_on_first_space() {
        let (c, a) = split_command("com.example.Main --flag 1");
        assert_eq!(c, "com.example.Main");
        assert_eq!(a, "--flag 1");
        let (c2, a2) = split_command("Main");
        assert_eq!(c2, "Main");
        assert_eq!(a2, "");
    }

    #[test]
    fn dedup_prefers_perfdata() {
        let mut rows = vec![
            Row { pid: 7, main: "A".into(), from_perf: false, ..Default::default() },
            Row { pid: 7, main: "B".into(), from_perf: true, ..Default::default() },
            Row { pid: 3, main: "C".into(), from_perf: false, ..Default::default() },
        ];
        rows.sort_by_key(|r| (r.pid, !r.from_perf));
        rows.dedup_by_key(|r| r.pid);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].pid, 3);
        assert_eq!(rows[1].main, "B");
    }
}
