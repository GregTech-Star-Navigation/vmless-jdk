//! 进程注册与发现。
//!
//! 目录布局 **刻意与 HotSpot 一致**（`%TEMP%/hsperfdata_<user>/`）：
//!
//! ```text
//! <temp>/hsperfdata_<user>/
//!     <pid>           perfdata 二进制文件（HotSpot v2.0 格式，见 perf.rs）
//!                     —— 真 jps / 真 jstat 也认这个目录，所以我们的 VM 能被真工具看见
//!     <pid>.attach    控制通道端点（port + token），key=value 文本
//! ```
//!
//! 为什么用 `%TEMP%` 而不是自己造一个目录：Windows 上 `%TEMP%` 是**按用户隔离**的
//! （`C:\Users\<user>\AppData\Local\Temp`），目录 ACL 天然只允许本用户访问 —— 这就是
//! HotSpot 用来做"只允许同用户"的那一层。我们在此基础上再加一层：控制通道要令牌，
//! 令牌只写在 `.attach` 文件里，所以拿到文件 = 同一个用户。

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 当前用户名（决定 perfdata 目录名）。
pub fn user_name() -> String {
    env::var("USERNAME")
        .or_else(|_| env::var("USER"))
        .or_else(|_| env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

/// `%TEMP%/hsperfdata_<user>`
pub fn perf_dir() -> PathBuf {
    env::temp_dir().join(format!("hsperfdata_{}", user_name()))
}

/// `<perf_dir>/<pid>`
pub fn perf_path(pid: u32) -> PathBuf {
    perf_dir().join(pid.to_string())
}

/// `<perf_dir>/<pid>.attach` —— **已不再使用**，保留只为兼容排查旧版本残留。
pub fn endpoint_path(pid: u32) -> PathBuf {
    perf_dir().join(format!("{}.attach", pid))
}

/// 确保目录存在。并发创建是安全的（已存在不算错）。
pub fn ensure_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    // Windows: 顺手把目录标成隐藏 —— HotSpot 也这么做，避免用户看着一堆 pid 文件发懵。
    #[cfg(windows)]
    {
        let _ = crate::sys::set_hidden(dir);
    }
    Ok(())
}

/// 控制通道端点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub pid: u32,
    pub port: u16,
    pub token: String,
    /// VM 启动时间（Unix 毫秒）。
    pub started_ms: u64,
    /// 协议主版本，便于在连接前就给出更好的错误信息。
    pub version: u8,
}

impl Endpoint {
    pub fn to_text(&self) -> String {
        format!(
            "pid={}\nport={}\ntoken={}\nstarted={}\nversion={}\n",
            self.pid, self.port, self.token, self.started_ms, self.version
        )
    }

    /// 解析端点文件。**任何一行坏掉都返回 `None`**，绝不 panic。
    pub fn parse(pid: u32, text: &str) -> Option<Endpoint> {
        let mut port = None;
        let mut token = None;
        let mut started = 0u64;
        let mut version = crate::protocol::VERSION;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (k, v) = line.split_once('=')?;
            match k.trim() {
                "port" => port = v.trim().parse::<u16>().ok(),
                "token" => token = Some(v.trim().to_string()),
                "started" => started = v.trim().parse::<u64>().unwrap_or(0),
                "version" => version = v.trim().parse::<u8>().unwrap_or(crate::protocol::VERSION),
                _ => {}
            }
        }
        let port = port?;
        let token = token?;
        if port == 0 || token.is_empty() {
            return None;
        }
        Some(Endpoint { pid, port, token, started_ms: started, version })
    }

    /// 从 perfdata 里读出端点。
    ///
    /// **不留单独的控制文件**：端口与令牌就是两个 perfdata 计数器
    /// （`vmless.attach.port` / `vmless.attach.token`）。这样只有一个共享文件，
    /// 少一种"文件被清掉 / 名字对不上 / 两个文件不一致"的故障。
    pub fn from_perf(pid: u32, pd: &crate::perf::PerfData) -> Option<Endpoint> {
        let port = pd.get_i64("vmless.attach.port")?;
        if port <= 0 || port > u16::MAX as i64 {
            return None;
        }
        let token = pd.get_str("vmless.attach.token")?.to_string();
        if token.is_empty() {
            return None;
        }
        Some(Endpoint {
            pid,
            port: port as u16,
            token,
            started_ms: pd.get_i64("sun.rt.createVmBeginTime").unwrap_or(0).max(0) as u64,
            version: crate::protocol::VERSION,
        })
    }

    /// 读端点：先读 perfdata（唯一真相源）。
    pub fn read(pid: u32) -> Option<Endpoint> {
        let pd = crate::perf::read(&perf_path(pid)).ok()?;
        Endpoint::from_perf(pid, &pd)
    }

}

/// 列出目录下所有"像 pid 的"条目对应的 pid。
///
/// 判定标准与 HotSpot 的 `MonitoredHost` 一致：文件名能解析成 `u32`，且该文件不是
/// `.attach` 之类带后缀的东西。
pub fn list_pids() -> Vec<u32> {
    let dir = perf_dir();
    let mut out = Vec::new();
    let rd = match fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return out,
    };
    for ent in rd.flatten() {
        let name = ent.file_name();
        let name = match name.to_str() {
            Some(s) => s,
            None => continue,
        };
        if let Ok(pid) = name.parse::<u32>() {
            if pid > 0 {
                out.push(pid);
            }
        }
    }
    out.sort_unstable();
    out
}

/// 本进程的 pid（`u32`）。
pub fn current_pid() -> u32 {
    std::process::id()
}

/// 清理一个 pid 的全部注册残留。
pub fn unregister(pid: u32) {
    let _ = fs::remove_file(perf_path(pid));
}

/// 清理**死进程**留下的残留（pid 已不存在）。返回清理条数。
///
/// 只在启动时调一次：正常退出会自己删文件，但被 kill -9 的进程会留下尸体，
/// 不清掉的话 `jps` 会一直显示一个不存在的 VM。
pub fn sweep_stale() -> usize {
    let mut n = 0;
    for pid in list_pids() {
        if pid == current_pid() {
            continue;
        }
        if !pid_alive(pid) {
            unregister(pid);
            n += 1;
        }
    }
    n
}

/// 进程是否还活着。
#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    crate::sys::pid_alive_windows(pid)
}

#[cfg(not(windows))]
pub fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{}", pid)).exists()
}

/// 生成 128 位令牌。
///
/// 熵来自 `RandomState`（std 自己用 OS 熵源播种），所以强度等于 OS 的种子质量；
/// 拼两轮就是 128 位。**不引入 rand 依赖**。
pub fn random_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut out = String::with_capacity(32);
    for i in 0..2u64 {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(nanos);
        h.write_u64(i);
        h.write_u32(std::process::id());
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

/// 当前 Unix 毫秒时间戳。
pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_name_nonempty() {
        assert!(!user_name().is_empty());
    }

    #[test]
    fn perf_dir_ends_with_hsperfdata() {
        let d = perf_dir();
        let n = d.file_name().unwrap().to_str().unwrap().to_string();
        assert!(n.starts_with("hsperfdata_"), "{}", n);
    }

    #[test]
    fn endpoint_roundtrip() {
        let ep = Endpoint {
            pid: 4242,
            port: 51234,
            token: "abc123".into(),
            started_ms: 99,
            version: 1,
        };
        let parsed = Endpoint::parse(4242, &ep.to_text()).unwrap();
        assert_eq!(parsed, ep);
    }

    #[test]
    fn endpoint_rejects_garbage() {
        assert!(Endpoint::parse(1, "").is_none());
        assert!(Endpoint::parse(1, "port=0\ntoken=x\n").is_none());
        assert!(Endpoint::parse(1, "port=123\ntoken=\n").is_none());
        assert!(Endpoint::parse(1, "port=123\n").is_none());
        assert!(Endpoint::parse(1, "no equals sign").is_none());
    }

    #[test]
    fn token_is_128_bits_hex() {
        let t = random_token();
        assert_eq!(t.len(), 32);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(t, random_token());
    }

    #[test]
    fn endpoint_from_perfdata() {
        use crate::perf::{PerfValue, Sample, Units, Var};
        use std::collections::BTreeMap;
        let mut c: BTreeMap<String, PerfValue> = BTreeMap::new();
        c.insert("vmless.attach.port".into(), PerfValue::Long(4577));
        c.insert("vmless.attach.token".into(), PerfValue::Str("abc123".into()));
        let pd = crate::perf::PerfData { counters: c, ..Default::default() };
        let ep = Endpoint::from_perf(99, &pd).expect("应该能解析出端点");
        assert_eq!(ep.port, 4577);
        assert_eq!(ep.token, "abc123");
        // 缺令牌 → None（不 panic）
        let pd2 = crate::perf::PerfData::default();
        assert!(Endpoint::from_perf(99, &pd2).is_none());
        let _ = (Sample::long("x", 1, Units::None, Var::Constant), endpoint_path(1));
    }

    #[test]
    fn current_pid_alive() {
        assert!(pid_alive(current_pid()));
    }
}
