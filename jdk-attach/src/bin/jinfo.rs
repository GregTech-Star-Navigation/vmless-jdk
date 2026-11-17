//! `jinfo` —— 读/写 VM 标志与系统属性。
//!
//! 读走 attach 通道（`VM.flags` / `VM.system_properties`），写走 `jcmd` 风格的
//! `VM.set_flag`。**改不了就报错**，不假装成功。

use vmless_attach::tool::{self, exit};

const USAGE: &str = "用法: jinfo [option] <pid>

  （无 option）      同时打印系统属性与 VM 标志
  -flags            打印 VM 标志
  -sysprops         打印系统属性
  -flag <name>      打印单个标志的值
  -flag +<name>     打开布尔标志
  -flag -<name>     关闭布尔标志
  -flag <name>=<v>  设置标志的值
  -t <ms>           超时，默认 2000
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

    let mut mode = Mode::All;
    let mut flag: Option<String> = None;
    let mut pid: Option<u32> = None;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "-flags" => {
                mode = Mode::Flags;
                i += 1;
            }
            "-sysprops" => {
                mode = Mode::SysProps;
                i += 1;
            }
            "-flag" => {
                match argv.get(i + 1) {
                    Some(v) => {
                        flag = Some(v.clone());
                        mode = Mode::Flag;
                        i += 2;
                    }
                    None => tool::usage(USAGE),
                }
            }
            other => {
                match other.parse::<u32>() {
                    Ok(p) => pid = Some(p),
                    Err(_) => tool::die_msg(exit::USAGE, &format!("不认识的参数: {}", other)),
                }
                i += 1;
            }
        }
    }
    let pid = match pid {
        Some(p) => p,
        None => tool::usage(USAGE),
    };
    let mut c = tool::attach(pid, timeout);

    match mode {
        Mode::All => {
            let props = c.command("VM.system_properties");
            match props {
                Ok(t) => {
                    tool::out("Java System Properties:\n");
                    tool::out(&strip_pid_header(&t));
                }
                Err(e) => tool::die(&e),
            }
            match c.command("VM.flags") {
                Ok(t) => {
                    tool::out("VM Flags:\n");
                    tool::out(&strip_pid_header(&t));
                }
                Err(e) => tool::die(&e),
            }
        }
        Mode::SysProps => match c.command("VM.system_properties") {
            Ok(t) => {
                tool::out("Java System Properties:\n");
                tool::out(&strip_pid_header(&t));
            }
            Err(e) => tool::die(&e),
        },
        Mode::Flags => match c.command("VM.flags") {
            Ok(t) => {
                tool::out("VM Flags:\n");
                tool::out(&strip_pid_header(&t));
            }
            Err(e) => tool::die(&e),
        },
        Mode::Flag => {
            let spec = flag.unwrap_or_default();
            let (name, op) = parse_flag_spec(&spec);
            match op {
                FlagOp::Read => match c.command(&format!("VM.get_flag {}", name)) {
                    Ok(t) => {
                        let body = strip_pid_header(&t);
                        let v = body.trim();
                        if v.is_empty() || v == "null" {
                            tool::die_msg(exit::REMOTE, &format!("没有这个标志: {}", name));
                        }
                        // 与真 jinfo 一致：`-XX:Name=value`
                        if v == "true" {
                            tool::out(&format!("-XX:+{}\n", name));
                        } else if v == "false" {
                            tool::out(&format!("-XX:-{}\n", name));
                        } else {
                            tool::out(&format!("-XX:{}={}\n", name, v));
                        }
                    }
                    Err(e) => tool::die(&e),
                },
                FlagOp::Write(val) => match c.command(&format!("VM.set_flag {} {}", name, val)) {
                    Ok(t) => {
                        let body = strip_pid_header(&t);
                        let b = body.trim();
                        if b.is_empty() || b.starts_with("OK") {
                            tool::out(&format!("-XX:{}={}\n", name, val));
                        } else {
                            tool::die_msg(exit::REMOTE, b);
                        }
                    }
                    Err(e) => tool::die(&e),
                },
            }
        }
    }
}

enum Mode {
    All,
    Flags,
    SysProps,
    Flag,
}

enum FlagOp {
    Read,
    Write(String),
}

/// `MaxHeapSize` → 读；`+PrintGC` → 写 true；`-PrintGC` → 写 false；`MaxHeapSize=1g` → 写值。
fn parse_flag_spec(spec: &str) -> (String, FlagOp) {
    if let Some(rest) = spec.strip_prefix('+') {
        return (rest.to_string(), FlagOp::Write("true".into()));
    }
    if let Some(rest) = spec.strip_prefix('-') {
        // 注意：`-flag -PrintGC` 里的 `-PrintGC` 是标志名的减号，不是命令行选项
        return (rest.to_string(), FlagOp::Write("false".into()));
    }
    if let Some((k, v)) = spec.split_once('=') {
        return (k.to_string(), FlagOp::Write(v.to_string()));
    }
    (spec.to_string(), FlagOp::Read)
}

/// 去掉 `<pid>:` 这一行（服务端会带，jinfo 的版式里不要）。
fn strip_pid_header(t: &str) -> String {
    match t.split_once('\n') {
        Some((first, rest)) if first.trim_end().ends_with(':') && first.trim_end().trim_end_matches(':').chars().all(|c| c.is_ascii_digit()) => {
            rest.to_string()
        }
        _ => t.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_spec_parsing() {
        assert!(matches!(parse_flag_spec("MaxHeapSize").1, FlagOp::Read));
        match parse_flag_spec("+PrintGC").1 {
            FlagOp::Write(v) => assert_eq!(v, "true"),
            _ => panic!(),
        }
        match parse_flag_spec("-PrintGC").1 {
            FlagOp::Write(v) => assert_eq!(v, "false"),
            _ => panic!(),
        }
        match parse_flag_spec("MaxHeapSize=1g").1 {
            FlagOp::Write(v) => assert_eq!(v, "1g"),
            _ => panic!(),
        }
    }

    #[test]
    fn strip_header() {
        let t = "1234:\nVM Flags:\n-XX:+X\n";
        assert_eq!(strip_pid_header(t), "VM Flags:\n-XX:+X\n");
        let t2 = "no header\nbody\n";
        assert_eq!(strip_pid_header(t2), "no header\nbody\n");
    }
}
