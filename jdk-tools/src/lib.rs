//! vmless 的 JDK 工具集（jimage / jmod / serialver / ...）。
//!
//! 设计原则：
//! - **不 panic**：所有对外入口返回 `Result<_, String>`，错误在 bin 层统一转成
//!   `eprintln!` + 退出码 1（用法错误用 2），绝不 `unwrap()` 崩栈。
//! - **不加依赖**：zip/deflate/SHA-1/jimage 索引全部自己实现于本 crate。
//! - 公共逻辑都放这里，`src/bin/*.rs` 只做参数解析与输出。

pub mod classops;
pub mod cli;
pub mod jimagex;
pub mod modinfo;
pub mod scan;
pub mod sha1;
pub mod svuid;
pub mod zipx;

// ---------------------------------------------------------------- 错误与退出

/// 用法错误（参数不合法）的退出码。JDK 的 launcher 对用法错误也用 1 或 2，
/// 我们固定 2 便于脚本区分「参数错」与「运行错」。
pub const EXIT_USAGE: i32 = 2;
/// 运行错误（文件不存在、格式坏、校验失败）的退出码
pub const EXIT_FAIL: i32 = 1;

/// `tool: 消息` 到 stderr 并退出。bin 层用它把 `Err(String)` 转成进程退出。
pub fn die(tool: &str, msg: &str) -> ! {
    eprintln!("{tool}: {msg}");
    std::process::exit(EXIT_FAIL)
}

/// 用法错误：`tool: 消息` + 提示看 `--help`，退出码 2
pub fn usage_err(tool: &str, msg: &str) -> ! {
    eprintln!("{tool}: {msg}");
    eprintln!("使用 {tool} --help 查看用法");
    std::process::exit(EXIT_USAGE)
}

// ---------------------------------------------------------------- 字符串工具

/// 路径里只保留 `/`，Windows 的 `\` 也认（工具的输入允许两种）
pub fn norm_sep(s: &str) -> String {
    s.replace('\\', "/")
}

/// 从 `--opt=value` / `--opt value` 两种形式里取值。
/// 返回 `(值, 消耗的参数个数)`；`args[i]` 是选项本身。
pub fn opt_value<'a>(args: &'a [String], i: usize, name: &str) -> Result<(String, usize), String> {
    let a = &args[i];
    if let Some(rest) = a.strip_prefix(&format!("{name}=")) {
        return Ok((rest.to_string(), 1));
    }
    if a == name {
        let v = args
            .get(i + 1)
            .ok_or_else(|| format!("选项 {name} 后面缺少值"))?;
        return Ok((v.clone(), 2));
    }
    Err(format!("不是选项 {name}（内部错误）"))
}

/// 逗号分隔的列表（`--include a,b` 与重复给出的选项都合并）
pub fn split_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

/// 简易 glob：`*` 不跨 `/`，`**` 跨，`?` 单字符。`/java.base/**` 这类模块前缀直接可用。
pub fn glob_match(pat: &str, text: &str) -> bool {
    fn go(p: &[u8], t: &[u8]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        match p[0] {
            b'*' => {
                // `**` 跨目录，`*` 不跨
                let (rest, cross) = if p.len() > 1 && p[1] == b'*' {
                    (&p[2..], true)
                } else {
                    (&p[1..], false)
                };
                // 允许 `**` 后面紧跟 `/` 时把它吃掉（`/a/**` 也能匹配 `/a/b`）
                for k in 0..=t.len() {
                    if go(rest, &t[k..]) {
                        return true;
                    }
                    if k < t.len() && !cross && t[k] == b'/' {
                        return false;
                    }
                }
                false
            }
            b'?' => !t.is_empty() && t[0] != b'/' && go(&p[1..], &t[1..]),
            c => !t.is_empty() && t[0] == c && go(&p[1..], &t[1..]),
        }
    }
    go(pat.as_bytes(), text.as_bytes())
}

/// 一组 include 模式里是否有一个命中（空列表 = 全命中）
pub fn any_match(pats: &[String], text: &str) -> bool {
    if pats.is_empty() {
        return true;
    }
    pats.iter().any(|p| {
        let p = p.trim();
        if let Some(rest) = p.strip_prefix("glob:") {
            glob_match(rest, text)
        } else if p.starts_with("regex:") {
            // 故意不支持：正则引擎会引入依赖。明确报出来比静默不匹配好。
            false
        } else {
            glob_match(p, text)
        }
    })
}

/// 里有没有 `regex:` 前缀的模式（调用方据此报「不支持」而不是静默不过滤）
pub fn has_regex(pats: &[String]) -> bool {
    pats.iter().any(|p| p.trim_start().starts_with("regex:"))
}

// ---------------------------------------------------------------- 递归遍历

/// 递归收集 `root` 下全部文件（排序后返回，保证输出稳定）。
/// `skip_hidden` 只影响 `.` 开头的名字（`META-INF` 不受影响）。
pub fn walk_files(root: &std::path::Path, skip_hidden: bool) -> Result<Vec<std::path::PathBuf>, String> {
    let mut out = Vec::new();
    fn go(
        dir: &std::path::Path,
        skip_hidden: bool,
        out: &mut Vec<std::path::PathBuf>,
        depth: usize,
    ) -> Result<(), String> {
        if depth > 64 {
            return Err(format!("目录层级超过 64 层，可能有符号链接环：{}", dir.display()));
        }
        let rd = std::fs::read_dir(dir).map_err(|e| format!("读目录 {} 失败: {e}", dir.display()))?;
        let mut items: Vec<_> = Vec::new();
        for e in rd {
            let e = e.map_err(|e| format!("读目录 {} 失败: {e}", dir.display()))?;
            items.push(e.path());
        }
        items.sort();
        for p in items {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if skip_hidden && name.starts_with('.') && name.len() > 1 {
                continue;
            }
            let md = match std::fs::symlink_metadata(&p) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if md.is_dir() {
                go(&p, skip_hidden, out, depth + 1)?;
            } else if md.is_file() {
                out.push(p);
            }
        }
        Ok(())
    }
    if !root.exists() {
        return Err(format!("路径不存在: {}", root.display()));
    }
    if root.is_file() {
        return Ok(vec![root.to_path_buf()]);
    }
    go(root, skip_hidden, &mut out, 0)?;
    Ok(out)
}

/// 建父目录（`create_dir_all` 的包装，错误信息带路径）
pub fn mkdirs(p: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(p).map_err(|e| format!("建目录 {} 失败: {e}", p.display()))
}

/// 把一个类名（`java.lang.String`）转成条目路径（`java/lang/String.class`）
pub fn class_to_entry(name: &str) -> String {
    let n = name.trim().replace('.', "/");
    if n.ends_with(".class") {
        n
    } else {
        format!("{n}.class")
    }
}

/// 条目路径（`java/lang/String.class`）转类名（`java.lang.String`）
pub fn entry_to_class(entry: &str) -> String {
    let e = entry.trim_start_matches('/');
    let e = e.strip_suffix(".class").unwrap_or(e);
    e.replace('/', ".")
}

// ---------------------------------------------------------------- 版本

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 统一的 `--version` 输出（真 JDK 各工具的格式基本是 `<tool> <版本>`）
pub fn print_version(tool: &str, jdk: &str) {
    println!("{tool} {VERSION} (vmless, 目标 JDK {jdk})");
}

/// 工具里表示「这不是我们能实现的那部分」的统一措辞：
/// 写清**缺什么能力**、**归哪个子系统**、**有没有替代路径**。
/// 注意：不打印「未实现/TODO」这类字样 —— 用户明确要求说清边界而不是占位。
pub fn capability_gap(tool: &str, missing: &str, subsystem: &str, alternative: &str) -> ! {
    println!("{tool}: 本 crate 不提供这项能力");
    println!("缺少的能力：{missing}");
    println!("归属子系统：{subsystem}（由该子系统实现，不在 jdk-tools 范围内）");
    println!("可用替代：{alternative}");
    println!("退出码 1");
    std::process::exit(EXIT_FAIL)
}

/// 类路径/模块路径的分段：`;` 或 `:` 分隔，但**盘符**（`D:`）要拼回去。
/// `D:/a;E:/b` → ["D:/a", "E:/b"]；`/a:/b` → ["/a", "/b"]。
pub fn split_paths(spec: &str) -> Vec<String> {
    let parts: Vec<&str> = spec
        .split([':', ';'])
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < parts.len() {
        let is_drive = parts[i].len() == 1
            && parts[i].as_bytes()[0].is_ascii_alphabetic()
            && i + 1 < parts.len();
        if is_drive {
            out.push(format!("{}:{}", parts[i], parts[i + 1]));
            i += 2;
        } else {
            out.push(parts[i].to_string());
            i += 1;
        }
    }
    out
}

// ---------------------------------------------------------------- 进程注册表（jps 用）

/// 我们的 launcher（javaw / jshell / jdb）在 `<TMP>/vmless_jps/<pid>.txt` 里登记
/// 「pid + 主类 + 参数」，`jps` 读它并剔除已死的 pid。
/// 为什么不读真 JDK 的 hsperfdata：那是按 JVM 版本变的二进制布局，我们没有对应实现。
pub mod jpsreg {
    use std::path::{Path, PathBuf};

    pub fn dir() -> PathBuf {
        std::env::temp_dir().join("vmless_jps")
    }

    pub fn entry_path(pid: u32) -> PathBuf {
        dir().join(format!("{pid}.txt"))
    }

    /// 登记当前进程（launcher 是 `javaw` / `jshell` / `jdb`）
    pub fn register(pid: u32, main: &str, args: &str, launcher: &str) -> Result<(), String> {
        let d = dir();
        std::fs::create_dir_all(&d).map_err(|e| format!("建 {} 失败: {e}", d.display()))?;
        let text = format!(
            "pid={pid}\nmain={main}\nargs={args}\nlauncher={launcher}\nstarted={}\n",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|x| x.as_secs())
                .unwrap_or(0)
        );
        std::fs::write(entry_path(pid), text)
            .map_err(|e| format!("写进程登记失败: {e}"))
    }

    pub fn unregister(pid: u32) {
        let _ = std::fs::remove_file(entry_path(pid));
    }

    pub struct Entry {
        pub pid: u32,
        pub main: String,
        pub args: String,
        pub launcher: String,
    }

    /// 读出全部登记项（坏行跳过，不报错：注册表是尽力而为的）
    pub fn list() -> Vec<Entry> {
        let mut out = Vec::new();
        let d = dir();
        let Ok(rd) = std::fs::read_dir(&d) else {
            return out;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("txt") {
                continue;
            }
            if let Some(en) = parse_entry(&p) {
                out.push(en);
            }
        }
        out.sort_by_key(|e| e.pid);
        out
    }

    fn parse_entry(p: &Path) -> Option<Entry> {
        let text = std::fs::read_to_string(p).ok()?;
        let mut pid = None;
        let (mut main, mut args, mut launcher) = (String::new(), String::new(), String::new());
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k {
                "pid" => pid = v.trim().parse().ok(),
                "main" => main = v.to_string(),
                "args" => args = v.to_string(),
                "launcher" => launcher = v.to_string(),
                _ => {}
            }
        }
        Some(Entry { pid: pid?, main, args, launcher })
    }

    /// pid 还活着吗？
    /// Windows 用 `tasklist /NH /FI "PID eq N"`（std 里没有进程存活查询，系统工具最省事）；
    /// 其它平台看 /proc。
    pub fn is_alive(pid: u32) -> bool {
        #[cfg(windows)]
        {
            let out = std::process::Command::new("tasklist")
                .args(["/NH", "/FI", &format!("PID eq {pid}")])
                .output();
            match out {
                Ok(o) => {
                    let s = String::from_utf8_lossy(&o.stdout);
                    // 没有匹配时输出 "信息: 没有运行的任务匹配指定标准。"
                    s.contains(&pid.to_string())
                        && !s.contains("No tasks")
                        && !s.contains("没有运行的任务")
                }
                Err(_) => false,
            }
        }
        #[cfg(not(windows))]
        {
            Path::new(&format!("/proc/{pid}")).exists()
        }
    }
}

/// 管道被下游提前关闭（`jmod list | head -1` 这种）时，Rust 的 `println!` 会 panic。
/// 真 JDK 的工具在这种情况下安静结束 —— 我们装一个 panic hook 对齐它：
/// 只吞「写 stdout/stderr 失败」这一类，别的 panic 照常打（不掩盖真 bug）。
pub fn install_pipe_guard() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info.to_string();
        if msg.contains("failed printing to stdout") || msg.contains("failed printing to stderr") {
            std::process::exit(0);
        }
        default(info);
    }));
}
