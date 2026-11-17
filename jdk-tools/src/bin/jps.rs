//! `jps` —— 列出本机的 JVM 进程（pid + 主类名）。
//!
//! 数据源：`<TMP>/vmless_jps/<pid>.txt` —— 我们的 launcher（`javaw` / `jshell` / `jdb`）
//! 启动时登记、退出时删除。`jps` 会剔除已死的 pid（Windows 用 `tasklist` 查存活，
//! 其它平台看 `/proc`），并把僵尸登记项清掉。
//!
//! 为什么不读真 JDK 的 `hsperfdata_*`：那是**随 JVM 版本变的私有二进制布局**，
//! 解析它等于绑死某个 JDK 小版本；我们的进程本来也不是 HotSpot。这一点写在 `--help`。

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
    println!("  -v 输出启动时给的参数");
    println!("  -V 只输出 pid（与 -q 相同，真 jps 里 -V 表示“不输出 JVM 参数”）");
    println!("  -h, -?, --help 打印这段帮助");
    println!();
    println!("数据源是本项目 launcher 写的进程登记表（<TMP>/vmless_jps），不是 HotSpot 的");
    println!("hsperfdata 共享内存 —— 那个布局随 JDK 版本变，我们没有对应解析器。");
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
    let with_args = args.flag("-m") || args.flag("-v");

    let entries = jpsreg::list();
    for e in entries {
        if !jpsreg::is_alive(e.pid) {
            // 死进程的登记项直接清掉（launcher 被强杀时不会自己删）
            jpsreg::unregister(e.pid);
            continue;
        }
        let mut line = String::new();
        if quiet {
            println!("{}", e.pid);
            continue;
        }
        line.push_str(&e.pid.to_string());
        line.push(' ');
        if long {
            line.push_str(&e.main);
        } else {
            // 短名：取最后一段（真 jps 也是这么显示）
            let simple = e.main.rsplit('.').next().unwrap_or(&e.main);
            line.push_str(simple);
            if !e.launcher.is_empty() && e.main.is_empty() {
                line.push_str(&e.launcher);
            }
        }
        if with_args && !e.args.is_empty() {
            line.push(' ');
            line.push_str(&e.args);
        }
        println!("{}", line.trim_end());
    }
}
