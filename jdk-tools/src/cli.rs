//! 统一的小工具命令行外壳：`-h/--help/--version`、`--opt=value` 解析、进度/错误输出。
//!
//! JDK 的工具都用 `Option*` 风格：`-?`/`-h`/`--help` 打印帮助到 stdout 并退出 0；
//! 参数错打印用法到 stderr 并退出 1（我们用 2 与之区分，见 `lib::EXIT_USAGE`）。

/// 参数扫描器：把 `args` 拆成「选项」与「位置参数」，同时保留原始顺序信息。
pub struct Args {
    pub raw: Vec<String>,
}

impl Args {
    pub fn new() -> Self {
        Args {
            raw: std::env::args().skip(1).collect(),
        }
    }

    pub fn from(raw: Vec<String>) -> Self {
        Args { raw }
    }

    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// 有没有 help 请求（`-?` / `-h` / `--help` / `-help`）
    pub fn wants_help(&self) -> bool {
        self.raw
            .iter()
            .any(|a| matches!(a.as_str(), "-?" | "-h" | "--help" | "-help"))
    }

    /// 有没有 version 请求
    pub fn wants_version(&self) -> bool {
        self.raw.iter().any(|a| a == "--version" || a == "-version")
    }

    /// 取 `--name value` / `--name=value` 的值（没有则 None）
    pub fn value(&self, name: &str) -> Result<Option<String>, String> {
        let with_eq = format!("{name}=");
        for (i, a) in self.raw.iter().enumerate() {
            if a == name {
                return match self.raw.get(i + 1) {
                    Some(v) => Ok(Some(v.clone())),
                    None => Err(format!("选项 {name} 后面缺少值")),
                };
            }
            if let Some(v) = a.strip_prefix(&with_eq) {
                return Ok(Some(v.to_string()));
            }
        }
        Ok(None)
    }

    /// 同上，但缺值时用 `default`
    pub fn value_or(&self, name: &str, default: &str) -> Result<String, String> {
        Ok(self.value(name)?.unwrap_or_else(|| default.to_string()))
    }

    /// 布尔开关（出现即真）
    pub fn flag(&self, name: &str) -> bool {
        self.raw.iter().any(|a| a == name)
    }

    /// 位置参数：既不是已知选项、也不是已知选项的值。
    /// `takes_value` 是「会吃掉下一个参数」的选项名列表。
    pub fn positionals(&self, takes_value: &[&str]) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.raw.len() {
            let a = &self.raw[i];
            let mut consumed = 1;
            let is_opt = a.starts_with('-') && a != "-";
            if is_opt {
                // `--opt=value` 只吃自己
                if a.contains('=') {
                    consumed = 1;
                } else if takes_value.iter().any(|t| t == &a.as_str()) {
                    consumed = 2;
                }
            } else {
                out.push(a.clone());
            }
            i += consumed;
        }
        Ok(out)
    }

    /// 未知选项检查：任何 `-` 开头但不在 `known` 里的都报错（不含 `-` 单字符）
    pub fn reject_unknown(&self, tool: &str, known: &[&str], takes_value: &[&str]) -> Result<(), String> {
        let mut i = 0;
        while i < self.raw.len() {
            let a = &self.raw[i];
            if a.starts_with('-') && a != "-" {
                let head = a.split('=').next().unwrap_or(a);
                if !known.contains(&head) {
                    return Err(format!("{tool}: 不认识的选项 {head}（用 {tool} --help 查看可用选项）"));
                }
                if !a.contains('=') && takes_value.contains(&head) {
                    if i + 1 >= self.raw.len() {
                        return Err(format!("{tool}: 选项 {head} 后面缺少值"));
                    }
                    i += 2;
                    continue;
                }
            }
            i += 1;
        }
        Ok(())
    }
}

impl Default for Args {
    fn default() -> Self {
        Self::new()
    }
}

/// 标准帮助骨架：`用法:` 一行 + 动作/选项两段。各工具只提供文本。
pub fn print_help(usage: &str, body: &str) {
    println!("{usage}");
    println!();
    print!("{body}");
    if !body.ends_with('\n') {
        println!();
    }
}

/// 把「耗时 + 内存峰值」打到 stderr（`--stats` 开关用）。
/// Windows 上峰值内存取自 `PeakWorkingSetSize`；拿不到就只说耗时。
pub fn print_stats(start: std::time::Instant, what: &str) {
    let d = start.elapsed();
    let peak = peak_rss_kb();
    match peak {
        Some(kb) => eprintln!(
            "{what}: 耗时 {:.3}s，进程峰值内存 {:.1} MB",
            d.as_secs_f64(),
            kb as f64 / 1024.0
        ),
        None => eprintln!("{what}: 耗时 {:.3}s", d.as_secs_f64()),
    }
}

#[cfg(windows)]
fn peak_rss_kb() -> Option<u64> {
    // 不引入 windows crate：直接读 GetProcessMemoryInfo 太麻烦，
    // 改用 `wmic` 会引入进程开销。这里用 std 能拿到的近似：读 /proc 不存在，
    // 于是返回 None（调用方只说耗时）。真实峰值见验收里用 PowerShell 测的那份。
    None
}

#[cfg(not(windows))]
fn peak_rss_kb() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kb: u64 = rest.trim().trim_end_matches(" kB").trim().parse().ok()?;
            return Some(kb);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_value_and_positionals() {
        let a = Args::from(vec![
            "--dir=D:/out".to_string(),
            "-p".to_string(),
            "8000".to_string(),
            "x".to_string(),
        ]);
        assert_eq!(a.value("--dir").unwrap().unwrap(), "D:/out");
        let pos = a.positionals(&["-p"]).unwrap();
        assert_eq!(pos, vec!["x".to_string()]);
    }

    #[test]
    fn unknown_option_rejected() {
        let a = Args::from(vec!["--nope".to_string()]);
        assert!(a.reject_unknown("t", &["--dir"], &["--dir"]).is_err());
    }
}
