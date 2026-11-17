//! `jfrfuzz` —— `.jfr` 解析入口的 fuzz 驱动。
//!
//! # 为什么要它
//!
//! 解析器读的是**别人（JVM）写的二进制**：截断、翻转一位、偏移指向文件中间某个
//! 随机位置，全是现实里会遇到的东西（磁盘满、进程被杀、传输截断）。
//! 「不会 panic」不是靠看代码保证的，是靠跑几十万例随机字节跑出来的。
//!
//! # 用法
//!
//! ```text
//! jfrfuzz --cases 50000 --file t.jfr
//! ```
//!
//! - `--cases N` 例数（默认 50000）
//! - `--seed S` 随机种子（默认 12345，可复现）
//! - `--file F` 真 `.jfr`，用来做**结构感知的变异**（比纯随机更能钻进深层分支）
//! - `--quiet` 不打印每例进度
//!
//! 每例都在 `catch_unwind` 里跑，**任何 panic 都会被计数并打印**，最后给汇总。
//! 退出码：0 = 0 panic；1 = 有 panic。

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};

use vmless_jfr::reader::Recording;

static CASES: AtomicU64 = AtomicU64::new(0);
static PANICS: AtomicU64 = AtomicU64::new(0);

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut cases = 50_000usize;
    let mut seed = 12_345u64;
    let mut file: Option<String> = None;
    let mut quiet = false;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--cases" => {
                i += 1;
                cases = argv.get(i).and_then(|v| v.parse().ok()).unwrap_or(cases);
            }
            "--seed" => {
                i += 1;
                seed = argv.get(i).and_then(|v| v.parse().ok()).unwrap_or(seed);
            }
            "--file" => {
                i += 1;
                file = argv.get(i).cloned();
            }
            "--quiet" => quiet = true,
            other => {
                eprintln!("未知参数 {}（支持 --cases / --seed / --file / --quiet）", other);
                std::process::exit(2);
            }
        }
        i += 1;
    }

    // 把 panic 的默认打印关掉：一例一条长栈会把 5 万例的进度冲没。
    // 真正的信息由 catch_unwind 抓到的 payload 给出。
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let seed_bytes = match &file {
        Some(f) => match std::fs::read(f) {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("读 {} 失败: {}", f, e);
                std::process::exit(2);
            }
        },
        None => None,
    };

    let mut rng = Rng::new(seed);
    let t0 = std::time::Instant::now();
    for case in 0..cases {
        let data = match &seed_bytes {
            // 六成走结构感知变异：这样才够得到「元数据/常量池/事件」那些分支
            Some(base) if case % 10 < 6 => mutate(base, &mut rng),
            _ => random_junk(&mut rng),
        };
        CASES.fetch_add(1, Ordering::Relaxed);
        let r = catch_unwind(AssertUnwindSafe(|| exercise(data)));
        if let Err(payload) = r {
            PANICS.fetch_add(1, Ordering::Relaxed);
            let msg = panic_message(&payload);
            println!("!!! panic on case {}: {}", case, msg);
            if !quiet && PANICS.load(Ordering::Relaxed) > 20 {
                println!("panic 太多，提前结束");
                break;
            }
        }
        if !quiet && case > 0 && case % 10_000 == 0 {
            println!(
                "  ... {} 例，{} panic，{:.1}s",
                case,
                PANICS.load(Ordering::Relaxed),
                t0.elapsed().as_secs_f64()
            );
        }
    }
    std::panic::set_hook(prev_hook);

    let n = CASES.load(Ordering::Relaxed);
    let p = PANICS.load(Ordering::Relaxed);
    println!(
        "fuzz 完成：{} 例，{} panic，耗时 {:.2}s，seed={}{}",
        n,
        p,
        t0.elapsed().as_secs_f64(),
        seed,
        if seed_bytes.is_some() {
            "（含结构感知变异）"
        } else {
            "（纯随机字节）"
        }
    );
    std::process::exit(if p == 0 { 0 } else { 1 });
}

/// 把解析器的每个公开入口都过一遍。
///
/// 顺序按「浅 → 深」：能从字节流构造出 `Recording` 的才继续往下走。
fn exercise(data: Vec<u8>) {
    let rec = match Recording::from_bytes(data) {
        Ok(r) => r,
        Err(_) => return,
    };
    let _ = rec.summary();
    let _ = rec.chunk_data(0);
    for i in 0..rec.chunks().len().min(4) {
        let _ = rec.chunk_data(i);
    }
    if let Ok(mut s) = rec.events() {
        let mut n = 0usize;
        while let Some(item) = s.next() {
            if item.is_err() {
                break;
            }
            n += 1;
            if n > 500 {
                break;
            }
        }
    }
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<非字符串 panic>".to_string()
    }
}

/// xorshift64*：无依赖、够用、可复现。
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
    fn byte(&mut self) -> u8 {
        (self.next() >> 24) as u8
    }
}

/// 纯随机字节。
///
/// 有一半会带上正确的魔数 —— 否则每个样本都在第一道检查上被挡掉，
/// 后面的解析路径一次也走不到。
fn random_junk(rng: &mut Rng) -> Vec<u8> {
    let len = rng.below(4096);
    let mut v: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
    if rng.next() % 2 == 0 && v.len() >= 4 {
        v[0] = b'F';
        v[1] = b'L';
        v[2] = b'R';
        v[3] = 0;
        if v.len() >= 8 {
            // 版本 2.1：过掉版本检查
            v[4] = 0;
            v[5] = 2;
            v[6] = 0;
            v[7] = 1;
        }
    }
    v
}

/// 结构感知变异：截断 / 翻位 / 插入 / 清零 / 改一个变长整数。
fn mutate(base: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut v = base.to_vec();
    if v.is_empty() {
        return v;
    }
    let rounds = 1 + rng.below(4);
    for _ in 0..rounds {
        match rng.next() % 5 {
            0 => {
                // 截断到某个位置
                let n = rng.below(v.len());
                v.truncate(n);
            }
            1 => {
                // 翻一位
                let i = rng.below(v.len());
                v[i] ^= 1u8 << (rng.next() % 8);
            }
            2 => {
                // 覆盖一个随机字节
                let i = rng.below(v.len());
                v[i] = rng.byte();
            }
            3 => {
                // 把一段清零
                let i = rng.below(v.len());
                let n = rng.below(64).min(v.len() - i);
                for b in v.iter_mut().skip(i).take(n) {
                    *b = 0;
                }
            }
            _ => {
                // 在一处插入几个随机字节（整体后移，破坏所有偏移）
                let i = rng.below(v.len());
                let n = 1 + rng.below(8);
                let junk: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
                v.splice(i..i, junk);
            }
        }
        if v.is_empty() {
            break;
        }
    }
    v
}
