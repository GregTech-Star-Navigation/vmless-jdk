//! `jstat` —— 与真 jstat **逐列对齐**的计数器采样器。
//!
//! # 列定义从哪来
//!
//! 不是猜的。JDK 里 jstat 的列规格是一份**文本**（`option <name> { column { header ^X^
//! data <表达式> scale <K|sec|raw> align <l|r> width N format "0.0" } }`），就在
//! `lib/modules` 里（`sun.jvmstat.perfdata.monitor` 的 option 规格资源）。本文件照着它
//! 一条条实现，所以：
//!
//! - 列名（`S0C` … `GCT`）与顺序一致
//! - **scale**：`K` = 字节/1024，`sec` = 值/hrt.frequency
//! - **format**：`0` / `0.0` / `0.00` / `0.000`
//! - 表头与列宽（下面 `EDGES`）是从真 jstat 的输出量出来的右边界
//! - 任何一项计数器缺失 → 该列打 `-`（真 jstat 的行为）
//!
//! # 空间顺序的坑（值得单独说）
//!
//! `sun.gc.generation.0.space.0` 是 **Eden**，`.space.1` 是 Survivor 0，`.space.2` 是
//! Survivor 1。按"0、1、2 = S0、S1、Eden"直觉写出来的计数器，真 jstat 会把 S0C 和 EC
//! 的值对调 —— 数字都"像样"，但全是错的。

use vmless_attach::client::DEFAULT_TIMEOUT_MS;
use vmless_attach::perf::PerfData;
use vmless_attach::tool::{self, exit};

/// 一列的取值器：从 perfdata 算出**已经格式化好**的字符串，缺数据返回 None。
type Getter = fn(&PerfData) -> Option<String>;

struct Col {
    header: &'static str,
    get: Getter,
    left: bool,
}

struct Table {
    /// 真 jstat 的表头原文（含行尾空格，逐字符照抄）
    header: &'static str,
    /// 每列的右边界（从真 jstat 输出量出）
    edges: &'static [usize],
    cols: &'static [Col],
}

const GC_EDGES: &[usize] = &[11, 23, 35, 47, 60, 73, 86, 99, 110, 121, 131, 141, 148, 158, 164, 174, 180, 190, 200];
const GC_HEADER: &str = "    S0C         S1C         S0U         S1U          EC           EU           OC           OU          MC         MU       CCSC      CCSU     YGC     YGCT     FGC    FGCT     CGC    CGCT       GCT   ";

const GCUTIL_EDGES: &[usize] = &[6, 13, 20, 27, 34, 41, 48, 58, 64, 74, 80, 90, 100];
const GCUTIL_HEADER: &str = "  S0     S1     E      O      M     CCS    YGC     YGCT     FGC    FGCT     CGC    CGCT       GCT   ";

const CLASS_EDGES: &[usize] = &[6, 14, 23, 31, 42];
const CLASS_HEADER: &str = "Loaded  Bytes  Unloaded  Bytes     Time   ";

const COMPILER_EDGES: &[usize] = &[8, 15, 23, 32, 43, 71];
const COMPILER_HEADER: &str = "Compiled Failed Invalid   Time   FailedType FailedMethod";

// ---------------------------------------------------------------------------
// 取值器
// ---------------------------------------------------------------------------

fn hz(p: &PerfData) -> f64 {
    p.get_i64("sun.os.hrt.frequency").filter(|v| *v > 0).map(|v| v as f64).unwrap_or(1e9)
}

/// 字节 → KB
fn kb(p: &PerfData, name: &str) -> Option<f64> {
    p.get_i64(name).map(|v| v as f64 / 1024.0)
}
/// ticks → 秒
fn sec(p: &PerfData, name: &str) -> Option<f64> {
    p.get_i64(name).map(|v| v as f64 / hz(p))
}
/// 和（表达式 `a + b` 的语义：**任一项缺失整列就是 `-`**）
fn sum(p: &PerfData, names: &[&str]) -> Option<f64> {
    let mut t = 0.0;
    for n in names {
        t += p.get_i64(n)? as f64;
    }
    Some(t)
}
fn sec_sum(p: &PerfData, names: &[&str]) -> Option<f64> {
    sum(p, names).map(|v| v / hz(p))
}
/// 使用率百分比：`(1 - (capacity - used) / capacity) * 100`
fn pct(p: &PerfData, cap: &str, used: &str) -> Option<f64> {
    let c = p.get_i64(cap)? as f64;
    let u = p.get_i64(used)? as f64;
    if c == 0.0 {
        // 真 jstat 对 0 容量也会算出 NaN；输出 "-" 更实在
        return None;
    }
    Some((1.0 - (c - u) / c) * 100.0)
}

fn f(v: Option<f64>, decimals: usize) -> Option<String> {
    v.map(|x| {
        if !x.is_finite() {
            return "-".to_string();
        }
        if decimals == 0 {
            format!("{}", x as i64)
        } else {
            format!("{:.*}", decimals, x)
        }
    })
}

// ---- -gc 列 ----
fn c_s0c(p: &PerfData) -> Option<String> {
    f(kb(p, "sun.gc.generation.0.space.1.capacity"), 1)
}
fn c_s1c(p: &PerfData) -> Option<String> {
    f(kb(p, "sun.gc.generation.0.space.2.capacity"), 1)
}
fn c_s0u(p: &PerfData) -> Option<String> {
    f(kb(p, "sun.gc.generation.0.space.1.used"), 1)
}
fn c_s1u(p: &PerfData) -> Option<String> {
    f(kb(p, "sun.gc.generation.0.space.2.used"), 1)
}
fn c_ec(p: &PerfData) -> Option<String> {
    f(kb(p, "sun.gc.generation.0.space.0.capacity"), 1)
}
fn c_eu(p: &PerfData) -> Option<String> {
    f(kb(p, "sun.gc.generation.0.space.0.used"), 1)
}
fn c_oc(p: &PerfData) -> Option<String> {
    f(kb(p, "sun.gc.generation.1.space.0.capacity"), 1)
}
fn c_ou(p: &PerfData) -> Option<String> {
    f(kb(p, "sun.gc.generation.1.space.0.used"), 1)
}
fn c_mc(p: &PerfData) -> Option<String> {
    f(kb(p, "sun.gc.metaspace.capacity"), 1)
}
fn c_mu(p: &PerfData) -> Option<String> {
    f(kb(p, "sun.gc.metaspace.used"), 1)
}
fn c_ccsc(p: &PerfData) -> Option<String> {
    f(kb(p, "sun.gc.compressedclassspace.capacity"), 1)
}
fn c_ccsu(p: &PerfData) -> Option<String> {
    f(kb(p, "sun.gc.compressedclassspace.used"), 1)
}
fn c_ygc(p: &PerfData) -> Option<String> {
    f(p.get_i64("sun.gc.collector.0.invocations").map(|v| v as f64), 0)
}
fn c_ygct(p: &PerfData) -> Option<String> {
    f(sec(p, "sun.gc.collector.0.time"), 3)
}
fn c_fgc(p: &PerfData) -> Option<String> {
    f(p.get_i64("sun.gc.collector.1.invocations").map(|v| v as f64), 0)
}
fn c_fgct(p: &PerfData) -> Option<String> {
    f(sec(p, "sun.gc.collector.1.time"), 3)
}
fn c_cgc(p: &PerfData) -> Option<String> {
    f(p.get_i64("sun.gc.collector.2.invocations").map(|v| v as f64), 0)
}
fn c_cgct(p: &PerfData) -> Option<String> {
    f(sec(p, "sun.gc.collector.2.time"), 3)
}
fn c_gct(p: &PerfData) -> Option<String> {
    f(
        sec_sum(
            p,
            &[
                "sun.gc.collector.0.time",
                "sun.gc.collector.1.time",
                "sun.gc.collector.2.time",
            ],
        ),
        3,
    )
}

// ---- -gcutil 列 ----
fn u_s0(p: &PerfData) -> Option<String> {
    f(pct(p, "sun.gc.generation.0.space.1.capacity", "sun.gc.generation.0.space.1.used"), 2)
}
fn u_s1(p: &PerfData) -> Option<String> {
    f(pct(p, "sun.gc.generation.0.space.2.capacity", "sun.gc.generation.0.space.2.used"), 2)
}
fn u_e(p: &PerfData) -> Option<String> {
    f(pct(p, "sun.gc.generation.0.space.0.capacity", "sun.gc.generation.0.space.0.used"), 2)
}
fn u_o(p: &PerfData) -> Option<String> {
    f(pct(p, "sun.gc.generation.1.space.0.capacity", "sun.gc.generation.1.space.0.used"), 2)
}
fn u_m(p: &PerfData) -> Option<String> {
    f(pct(p, "sun.gc.metaspace.capacity", "sun.gc.metaspace.used"), 2)
}
fn u_ccs(p: &PerfData) -> Option<String> {
    f(pct(p, "sun.gc.compressedclassspace.capacity", "sun.gc.compressedclassspace.used"), 2)
}

// ---- -class 列 ----
fn k_loaded(p: &PerfData) -> Option<String> {
    f(sum(p, &["java.cls.loadedClasses", "java.cls.sharedLoadedClasses"]), 0)
}
fn k_bytes(p: &PerfData) -> Option<String> {
    f(sum(p, &["sun.cls.loadedBytes", "sun.cls.sharedLoadedBytes"]).map(|v| v / 1024.0), 1)
}
fn k_unloaded(p: &PerfData) -> Option<String> {
    f(sum(p, &["java.cls.unloadedClasses", "java.cls.sharedUnloadedClasses"]), 0)
}
fn k_ubytes(p: &PerfData) -> Option<String> {
    f(sum(p, &["sun.cls.unloadedBytes", "sun.cls.sharedUnloadedBytes"]).map(|v| v / 1024.0), 1)
}
fn k_time(p: &PerfData) -> Option<String> {
    f(sec(p, "sun.cls.time"), 2)
}

// ---- -compiler 列 ----
fn j_compiled(p: &PerfData) -> Option<String> {
    f(p.get_i64("sun.ci.totalCompiles").map(|v| v as f64), 0)
}
fn j_failed(p: &PerfData) -> Option<String> {
    f(p.get_i64("sun.ci.totalBailouts").map(|v| v as f64), 0)
}
fn j_invalid(p: &PerfData) -> Option<String> {
    f(p.get_i64("sun.ci.totalInvalidates").map(|v| v as f64), 0)
}
fn j_time(p: &PerfData) -> Option<String> {
    f(sec(p, "java.ci.totalTime"), 2)
}
fn j_ftype(p: &PerfData) -> Option<String> {
    f(p.get_i64("sun.ci.lastFailedType").map(|v| v as f64), 0)
}
fn j_fmethod(p: &PerfData) -> Option<String> {
    p.get_str("sun.ci.lastFailedMethod").map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// 表
// ---------------------------------------------------------------------------

fn gc_cols() -> Vec<Col> {
    vec![
        Col { header: "S0C", get: c_s0c, left: false },
        Col { header: "S1C", get: c_s1c, left: false },
        Col { header: "S0U", get: c_s0u, left: false },
        Col { header: "S1U", get: c_s1u, left: false },
        Col { header: "EC", get: c_ec, left: false },
        Col { header: "EU", get: c_eu, left: false },
        Col { header: "OC", get: c_oc, left: false },
        Col { header: "OU", get: c_ou, left: false },
        Col { header: "MC", get: c_mc, left: false },
        Col { header: "MU", get: c_mu, left: false },
        Col { header: "CCSC", get: c_ccsc, left: false },
        Col { header: "CCSU", get: c_ccsu, left: false },
        Col { header: "YGC", get: c_ygc, left: false },
        Col { header: "YGCT", get: c_ygct, left: false },
        Col { header: "FGC", get: c_fgc, left: false },
        Col { header: "FGCT", get: c_fgct, left: false },
        Col { header: "CGC", get: c_cgc, left: false },
        Col { header: "CGCT", get: c_cgct, left: false },
        Col { header: "GCT", get: c_gct, left: false },
    ]
}

fn gcutil_cols() -> Vec<Col> {
    vec![
        Col { header: "S0", get: u_s0, left: false },
        Col { header: "S1", get: u_s1, left: false },
        Col { header: "E", get: u_e, left: false },
        Col { header: "O", get: u_o, left: false },
        Col { header: "M", get: u_m, left: false },
        Col { header: "CCS", get: u_ccs, left: false },
        Col { header: "YGC", get: c_ygc, left: false },
        Col { header: "YGCT", get: c_ygct, left: false },
        Col { header: "FGC", get: c_fgc, left: false },
        Col { header: "FGCT", get: c_fgct, left: false },
        Col { header: "CGC", get: c_cgc, left: false },
        Col { header: "CGCT", get: c_cgct, left: false },
        Col { header: "GCT", get: c_gct, left: false },
    ]
}

fn class_cols() -> Vec<Col> {
    vec![
        Col { header: "Loaded", get: k_loaded, left: false },
        Col { header: "Bytes", get: k_bytes, left: false },
        Col { header: "Unloaded", get: k_unloaded, left: false },
        Col { header: "Bytes", get: k_ubytes, left: false },
        Col { header: "Time", get: k_time, left: false },
    ]
}

fn compiler_cols() -> Vec<Col> {
    vec![
        Col { header: "Compiled", get: j_compiled, left: false },
        Col { header: "Failed", get: j_failed, left: false },
        Col { header: "Invalid", get: j_invalid, left: false },
        Col { header: "Time", get: j_time, left: false },
        Col { header: "FailedType", get: j_ftype, left: false },
        Col { header: "FailedMethod", get: j_fmethod, left: true },
    ]
}

/// 渲染一行：每列对齐到 `edges[i]`。
///
/// 最后一列也要对齐（右对齐 = 左边补空格，左对齐 = 不补）—— 这是"行宽严格等于最后一列
/// 右边界"的关键。真 jstat 的行宽就恒等于表头宽度（-gc 是 200），少了这一点会出现
/// `0.0000.012` 这种两列粘在一起的效果。
fn render_row(p: &PerfData, cols: &[Col], edges: &[usize]) -> String {
    let mut s = String::new();
    let mut prev = 0usize;
    for (i, c) in cols.iter().enumerate() {
        let edge = *edges.get(i).unwrap_or(&(prev + 12));
        let width = edge.saturating_sub(prev).max(1);
        let last = i + 1 == cols.len();
        let text = (c.get)(p).unwrap_or_else(|| "-".to_string());
        let pad = width.saturating_sub(text.chars().count());
        if c.left {
            // 真 jstat 的左对齐列前面恒有一个空格（表头里 `^FailedMethod` 的 ^ 前面就是它）
            s.push(' ');
            s.push_str(&text);
            if !last {
                for _ in 0..pad {
                    s.push(' ');
                }
            }
        } else {
            for _ in 0..pad {
                s.push(' ');
            }
            s.push_str(&text);
        }
        prev = edge;
    }
    s.push(char::from(10u8));
    s
}

fn table_for(option: &str) -> Option<(&'static str, &'static [usize], Vec<Col>)> {
    Some(match option {
        "gc" => (GC_HEADER, GC_EDGES, gc_cols()),
        "gcutil" => (GCUTIL_HEADER, GCUTIL_EDGES, gcutil_cols()),
        "class" => (CLASS_HEADER, CLASS_EDGES, class_cols()),
        "compiler" => (COMPILER_HEADER, COMPILER_EDGES, compiler_cols()),
        _ => return None,
    })
}

const OPTIONS: &[&str] = &["class", "compiler", "gc", "gcutil"];

const USAGE: &str = "用法: jstat -<option> [-t] [-h<lines>] <vmid> [<interval> [<count>]]

说明:
  <option>      class | compiler | gc | gcutil（本实现支持这 4 个）
  <vmid>        <pid>（本实现只支持本地）
  <interval>    采样间隔，<n>[ms|s]，默认 ms
  <count>       采样次数，默认 1
  -t            每行前加一列时间戳（相对 VM 启动的秒数）
  -t<ms>        超时（毫秒）—— 与 -t 的时间戳模式靠『有没有紧跟数字』区分
  --timeout <ms> 超时（毫秒），默认 2000
  -J<flag>      传给 JVM 的参数（接受并忽略，保持命令行兼容）
  -options      列出支持的 option
";

fn parse_interval(s: &str) -> Result<u64, String> {
    let (num, mult) = if let Some(x) = s.strip_suffix("ms") {
        (x, 1u64)
    } else if let Some(x) = s.strip_suffix('s') {
        (x, 1000u64)
    } else {
        (s, 1u64)
    };
    let n: u64 = num.trim().parse().map_err(|_| format!("间隔不合法: {}", s))?;
    Ok(n * mult)
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut timeout = DEFAULT_TIMEOUT_MS;
    let mut timestamp = false;
    let mut option: Option<String> = None;
    let mut vmid: Option<String> = None;
    let mut interval: Option<u64> = None;
    let mut count: Option<u64> = None;
    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].clone();
        if a == "-options" || a == "-options " {
            for o in OPTIONS {
                tool::out(&format!("{}\n", o));
            }
            return;
        }
        if a == "-h" || a == "--help" {
            tool::print_usage(USAGE);
        }
        if let Some(rest) = a.strip_prefix("-J") {
            let _ = rest; // 接受并忽略
            i += 1;
            continue;
        }
        if a == "-t" {
            timestamp = true;
            i += 1;
            continue;
        }
        if let Some(rest) = a.strip_prefix("-t") {
            if let Ok(ms) = rest.parse::<u64>() {
                timeout = ms.max(1);
                i += 1;
                continue;
            }
        }
        if a == "--timeout" {
            if let Some(v) = argv.get(i + 1).and_then(|v| v.parse::<u64>().ok()) {
                timeout = v.max(1);
                i += 2;
                continue;
            }
            tool::usage("--timeout 需要一个毫秒数");
        }
        if let Some(rest) = a.strip_prefix("-h") {
            // -h<lines>：接受并忽略（只在长采样时需要重复表头）
            if rest.chars().all(|c| c.is_ascii_digit()) && !rest.is_empty() {
                i += 1;
                continue;
            }
        }
        if let Some(opt) = a.strip_prefix('-') {
            if option.is_none() {
                option = Some(opt.to_string());
                i += 1;
                continue;
            }
        }
        if vmid.is_none() {
            vmid = Some(a);
        } else if interval.is_none() {
            match parse_interval(&a) {
                Ok(v) => interval = Some(v),
                Err(e) => {
                    let _ = e;
                    tool::die_msg(exit::USAGE, &format!("间隔不合法: {}", a));
                }
            }
        } else if count.is_none() {
            match a.parse::<u64>() {
                Ok(v) => count = Some(v),
                Err(_) => tool::die_msg(exit::USAGE, &format!("次数不合法: {}", a)),
            }
        } else {
            tool::usage(USAGE);
        }
        i += 1;
    }

    let option = match option {
        Some(o) => o,
        None => tool::usage(USAGE),
    };
    let (header, edges, cols) = match table_for(&option) {
        Some(t) => t,
        None => tool::die_msg(
            exit::USAGE,
            &format!("不支持的 option: {}（支持: {}）", option, OPTIONS.join(", ")),
        ),
    };
    let vmid = match vmid {
        Some(v) => v,
        None => tool::usage(USAGE),
    };
    let (pid, host) = match tool::parse_vmid(&vmid) {
        Ok(v) => v,
        Err(e) => tool::die_msg(exit::USAGE, &e),
    };
    // `pid@host:port` → 走 jstatd 取远端 perfdata（真 jstat 的 vmid 语法）
    let remote = host.as_ref().map(|h| {
        if h.contains(':') {
            h.clone()
        } else {
            format!("{}:1099", h)
        }
    });

    let interval_ms = interval.unwrap_or(0);
    let count = count.unwrap_or(1).min(1_000_000);

    let mut first = true;
    let mut n = 0u64;
    loop {
        let pd = match &remote {
            Some(hp) => match tool::read_perf_remote(hp, pid, timeout) {
                Ok(d) => d,
                Err(e) => tool::die_msg(exit::IO, &e),
            },
            None => match tool::read_perf(pid) {
                Ok(d) => d,
                Err(e) => tool::die_msg(exit::NO_VM, &e),
            },
        };
        if first {
            tool::out(header);
            if !header.ends_with('\n') {
                tool::out("\n");
            }
            first = false;
        }
        let mut line = String::new();
        if timestamp {
            let begin = pd.get_i64("sun.rt.createVmBeginTime").unwrap_or(0);
            let now = vmless_attach::registry::now_ms() as i64;
            line.push_str(&format!("{:>15.1}", (now - begin) as f64 / 1000.0));
        }
        line.push_str(&render_row(&pd, &cols, edges));
        tool::out(&line);
        n += 1;
        if n >= count {
            break;
        }
        if interval_ms == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(interval_ms));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use vmless_attach::perf::PerfValue;

    fn sample() -> PerfData {
        let mut c: BTreeMap<String, PerfValue> = BTreeMap::new();
        let mut l = |n: &str, v: i64| {
            c.insert(n.to_string(), PerfValue::Long(v));
        };
        l("sun.os.hrt.frequency", 1_000_000_000);
        l("sun.gc.generation.0.space.0.capacity", 8 * 1024 * 1024); // Eden
        l("sun.gc.generation.0.space.0.used", 4 * 1024 * 1024);
        l("sun.gc.generation.0.space.1.capacity", 1 * 1024 * 1024); // S0
        l("sun.gc.generation.0.space.1.used", 0);
        l("sun.gc.generation.0.space.2.capacity", 1 * 1024 * 1024); // S1
        l("sun.gc.generation.0.space.2.used", 512 * 1024);
        l("sun.gc.generation.1.space.0.capacity", 16 * 1024 * 1024);
        l("sun.gc.generation.1.space.0.used", 8 * 1024 * 1024);
        l("sun.gc.metaspace.capacity", 4 * 1024 * 1024);
        l("sun.gc.metaspace.used", 2 * 1024 * 1024);
        l("sun.gc.compressedclassspace.capacity", 1 * 1024 * 1024);
        l("sun.gc.compressedclassspace.used", 512 * 1024);
        l("sun.gc.collector.0.invocations", 5);
        l("sun.gc.collector.0.time", 1_500_000);
        l("sun.gc.collector.1.invocations", 0);
        l("sun.gc.collector.1.time", 0);
        l("sun.gc.collector.2.invocations", 0);
        l("sun.gc.collector.2.time", 0);
        l("java.cls.loadedClasses", 100);
        l("java.cls.sharedLoadedClasses", 0);
        l("java.cls.unloadedClasses", 2);
        l("java.cls.sharedUnloadedClasses", 0);
        l("sun.cls.loadedBytes", 40 * 1024);
        l("sun.cls.sharedLoadedBytes", 0);
        l("sun.cls.unloadedBytes", 1024);
        l("sun.cls.sharedUnloadedBytes", 0);
        l("sun.cls.time", 14_270_000_000);
        l("sun.ci.totalCompiles", 42);
        l("sun.ci.totalBailouts", 1);
        l("sun.ci.totalInvalidates", 0);
        l("java.ci.totalTime", 191_560_000_000);
        l("sun.ci.lastFailedType", 1);
        c.insert(
            "sun.ci.lastFailedMethod".into(),
            PerfValue::Str("java/math/MutableBigInteger rightShift".into()),
        );
        PerfData { counters: c, ..Default::default() }
    }

    #[test]
    fn gc_row_matches_reference_layout() {
        let pd = sample();
        let (header, edges, cols) = table_for("gc").unwrap();
        assert_eq!(header.len(), 200, "表头必须和真 jstat 一样宽");
        assert!(header.starts_with("    S0C"));
        let row = render_row(&pd, &cols, edges);
        // S0C 右边界 11 → "     1024.0"
        assert_eq!(&row[..11], "     1024.0");
        // EC = space.0 = Eden = 8192 KB
        assert_eq!(&row[47..60], "      8192.0");
        // YGCT = 1.5ms → 0.002 s（3 位小数）
        assert!(row.contains("0.002"));
        assert_eq!(row.len(), 200);
    }

    #[test]
    fn missing_counter_renders_dash() {
        let mut pd = sample();
        pd.counters.remove("sun.gc.collector.1.invocations");
        let (_, edges, cols) = table_for("gc").unwrap();
        let row = render_row(&pd, &cols, edges);
        // FGC 右边界 164，前一个边界 158 → 宽 6，值是 "-"
        assert_eq!(&row[158..164], "     -");
    }

    #[test]
    fn class_table_uses_unloaded_bytes_for_fourth_column() {
        let pd = sample();
        let (_, edges, cols) = table_for("class").unwrap();
        let row = render_row(&pd, &cols, edges);
        assert!(row.starts_with("   100"), "Loaded: {:?}", &row[..8]);
        // 第 4 列是 Unloaded 的字节数 = 1 KB
        assert_eq!(&row[23..31], "     1.0");
    }

    #[test]
    fn gcutil_percentages() {
        let pd = sample();
        let (_, edges, cols) = table_for("gcutil").unwrap();
        let row = render_row(&pd, &cols, edges);
        // S0 容量 1MB、已用 0 → 0.00
        assert_eq!(&row[..6], "  0.00");
        // S1 已用 50%
        assert_eq!(&row[6..13], "  50.00");
        // E 已用 50%
        assert_eq!(&row[13..20], "  50.00");
    }

    #[test]
    fn compiler_table() {
        let pd = sample();
        let (header, edges, cols) = table_for("compiler").unwrap();
        assert!(header.starts_with("Compiled Failed Invalid"));
        let row = render_row(&pd, &cols, edges);
        assert!(row.starts_with("      42"));
        assert!(row.contains("java/math/MutableBigInteger rightShift"));
    }

    #[test]
    fn interval_parsing() {
        assert_eq!(parse_interval("1000").unwrap(), 1000);
        assert_eq!(parse_interval("1s").unwrap(), 1000);
        assert_eq!(parse_interval("250ms").unwrap(), 250);
        assert!(parse_interval("abc").is_err());
    }
}
