//! 交叉验证与压测小工具（不进交付面，但每条验收都靠它出数据）。
//!
//! ```text
//! ctool hash <md5|sha1|sha256|sha384|sha512> <file>
//! ctool hash-hex <alg> <hex>
//! ctool hmac <alg> <keyhex|ascii:text> <file>
//! ctool pbkdf2 <alg> <passascii> <salthex> <iter> <len>
//! ctool kdf <alg> <passascii> <salthex> <iter> <id> <len>
//! ctool genblob <bytes> <file>
//! ctool bigcheck <vectors.txt>
//! ctool bench-sha256 <mb>
//! ctool bigin <op> <a-hex> <b-hex> [mod-hex]
//! ```

use std::process::ExitCode;

use vmless_crypto::bigint::BigUint;
use vmless_crypto::hash::{hex, unhex, Alg};

fn alg_of(s: &str) -> Alg {
    match s.to_ascii_lowercase().as_str() {
        "md5" => Alg::Md5,
        "sha1" | "sha-1" => Alg::Sha1,
        "sha256" | "sha-256" => Alg::Sha256,
        "sha384" | "sha-384" => Alg::Sha384,
        "sha512" | "sha-512" => Alg::Sha512,
        other => {
            eprintln!("未知算法: {other}");
            std::process::exit(2);
        }
    }
}

fn read_input(path: &str) -> Vec<u8> {
    if path == "-" {
        use std::io::Read;
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf).expect("读 stdin 失败");
        buf
    } else {
        std::fs::read(path).unwrap_or_else(|e| {
            eprintln!("读文件失败 {path}: {e}");
            std::process::exit(2);
        })
    }
}

fn key_bytes(s: &str) -> Vec<u8> {
    match s.strip_prefix("ascii:") {
        Some(rest) => rest.as_bytes().to_vec(),
        None => unhex(s).expect("密钥必须是十六进制或 ascii: 前缀"),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("用法见源码头部注释");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "hash" => {
            let a = alg_of(&args[1]);
            let data = read_input(&args[2]);
            println!("{}", hex(&a.digest(&data)));
        }
        "hash-hex" => {
            let a = alg_of(&args[1]);
            let data = unhex(&args[2]).expect("hex");
            println!("{}", hex(&a.digest(&data)));
        }
        "hmac" => {
            let a = alg_of(&args[1]);
            let key = key_bytes(&args[2]);
            let data = read_input(&args[3]);
            println!("{}", hex(&vmless_crypto::hash::hmac(a, &key, &data)));
        }
        "pbkdf2" => {
            let a = alg_of(&args[1]);
            let pass = args[2].as_bytes();
            let salt = unhex(&args[3]).expect("salt");
            let iter: u32 = args[4].parse().expect("iter");
            let len: usize = args[5].parse().expect("len");
            println!("{}", hex(&vmless_crypto::hash::pbkdf2(a, pass, &salt, iter, len)));
        }
        "kdf" => {
            let a = alg_of(&args[1]);
            let pass = args[2].as_bytes();
            let salt = unhex(&args[3]).expect("salt");
            let iter: u32 = args[4].parse().expect("iter");
            let id: u8 = args[5].parse().expect("id");
            let len: usize = args[6].parse().expect("len");
            println!("{}", hex(&vmless_crypto::hash::pkcs12_kdf(a, pass, &salt, iter, id, len)));
        }
        "genblob" => {
            let n: usize = args[1].parse().expect("bytes");
            let mut out = Vec::with_capacity(n);
            let mut counter = 0u64;
            while out.len() < n {
                let mut seed = b"vmless-blob".to_vec();
                seed.extend_from_slice(&counter.to_le_bytes());
                out.extend_from_slice(&Alg::Sha256.digest(&seed));
                counter += 1;
            }
            out.truncate(n);
            std::fs::write(&args[2], &out).expect("写文件失败");
        }
        "bigcheck" => return bigcheck(&args[1]),
        "fuzz" => {
            let n: usize = args[1].parse().expect("iters");
            let only = args.get(2).map(|s| s.as_str()).unwrap_or("all");
            let panics = fuzz_all(n, only);
            return if panics == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE };
        }
        "bench-rsa" => {
            let bits: usize = args.get(1).map(|s| s.parse().unwrap_or(2048)).unwrap_or(2048);
            let iters: usize = args.get(2).map(|s| s.parse().unwrap_or(20)).unwrap_or(20);
            bench_rsa(bits, iters);
        }
        "bench-p12" => bench_p12(&args[1], &args[2], args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10)),
        "bigin" => {
            let op = args[1].as_str();
            let a = BigUint::from_hex(&args[2]).expect("a");
            let b = BigUint::from_hex(&args[3]).expect("b");
            let m = args.get(4).map(|s| BigUint::from_hex(s).expect("m"));
            let out = match op {
                "add" => a.add(&b),
                "sub" => a.sub(&b).expect("下溢"),
                "mul" => a.mul(&b),
                "div" => a.div_rem(&b).expect("除零").0,
                "rem" => a.div_rem(&b).expect("除零").1,
                "gcd" => a.gcd(&b),
                "modpow" => a.modpow(&b, m.as_ref().expect("需要模数")).expect("modpow"),
                "modinv" => a.modinv(m.as_ref().expect("需要模数")).expect("modinv"),
                other => {
                    eprintln!("未知运算: {other}");
                    return ExitCode::from(2);
                }
            };
            println!("{}", out.to_hex());
        }
        "bench-sha256" => {
            let mb: usize = args[1].parse().expect("mb");
            let data = vec![0x5au8; mb * 1024 * 1024];
            let t0 = std::time::Instant::now();
            let d = Alg::Sha256.digest(&data);
            let dt = t0.elapsed().as_secs_f64();
            println!(
                "{} MB in {:.3}s = {:.1} MB/s (digest {})",
                mb,
                dt,
                mb as f64 / dt,
                hex(&d[..4])
            );
        }
        other => {
            eprintln!("未知子命令: {other}");
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}

/// 逐条核对 JDK BigInteger 生成的向量，任一不匹配就返回失败。
fn bigcheck(path: &str) -> ExitCode {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("读向量文件失败: {e}");
            return ExitCode::from(2);
        }
    };
    let mut ok = 0usize;
    let mut bad = 0usize;
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split('|').collect();
        if f.len() != 5 {
            eprintln!("第 {} 行格式不对: {line}", lineno + 1);
            bad += 1;
            continue;
        }
        let op = f[0];
        let a = match BigUint::from_hex(f[1]) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("第 {} 行 a 解析失败: {e}", lineno + 1);
                bad += 1;
                continue;
            }
        };
        let b = BigUint::from_hex(f[2]).unwrap_or_else(|_| BigUint::zero());
        let extra = if f[3] == "-" { None } else { BigUint::from_hex(f[3]).ok() };
        let got = match op {
            "add" => a.add(&b),
            "sub" => match a.sub(&b) {
                Ok(v) => v,
                Err(_) => {
                    eprintln!("第 {} 行 sub 下溢", lineno + 1);
                    bad += 1;
                    continue;
                }
            },
            "mul" => a.mul(&b),
            "div" => a.div_rem(&b).map(|(q, _)| q).unwrap_or_else(|_| BigUint::zero()),
            "rem" => a.div_rem(&b).map(|(_, r)| r).unwrap_or_else(|_| BigUint::zero()),
            "gcd" => a.gcd(&b),
            "modpow" => a
                .modpow(&b, extra.as_ref().unwrap_or(&BigUint::zero()))
                .unwrap_or_else(|_| BigUint::zero()),
            "modinv" => a.modinv(extra.as_ref().unwrap_or(&BigUint::zero())).unwrap_or_else(|_| BigUint::zero()),
            other => {
                eprintln!("第 {} 行未知运算 {other}", lineno + 1);
                bad += 1;
                continue;
            }
        };
        let want = match BigUint::from_hex(f[4]) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("第 {} 行结果解析失败: {e}", lineno + 1);
                bad += 1;
                continue;
            }
        };
        if got == want {
            ok += 1;
        } else {
            bad += 1;
            println!(
                "MISMATCH line {} op={} got={} want={}",
                lineno + 1,
                op,
                got.to_hex(),
                want.to_hex()
            );
        }
    }
    println!("bigcheck: {ok} ok, {bad} bad");
    if bad == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

// ============================ fuzz 与压测（追加实现） ============================

/// 确定性的小 PRNG（fuzz 要可复现）。
struct Xs(u64);
impl Xs {
    fn new(seed: u64) -> Self {
        Xs(seed | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn byte(&mut self) -> u8 {
        (self.next() >> 24) as u8
    }
}

/// 用 `catch_unwind` 统计 panic 次数；返回 (cases, panics)。
fn fuzz_run(label: &str, n: usize, mut gen: impl FnMut(&mut Xs) -> Vec<u8>, mut target: impl FnMut(&[u8])) -> usize {
    std::panic::set_hook(Box::new(|_| {}));
    let mut panics = 0usize;
    let mut rng = Xs::new(0x9e3779b97f4a7c15);
    for _ in 0..n {
        let input = gen(&mut rng);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| target(&input)));
        if r.is_err() {
            panics += 1;
        }
    }
    let _ = std::panic::take_hook();
    println!("fuzz {label}: {n} 例, panic {panics} 次");
    panics
}

fn fuzz_all(n: usize, only: &str) -> usize {
    let want = |k: &str| only == "all" || only == k;
    use vmless_crypto::asn1::der;
    use vmless_crypto::pkcs12::P12;
    use vmless_crypto::x509::Certificate;
    let mut total = 0;
    // 1) 纯随机字节
    if want("der") { total += fuzz_run(
        "der/random",
        n,
        |r| {
            let len = (r.next() as usize) % 200;
            (0..len).map(|_| r.byte()).collect()
        },
        |b| {
            let _ = der::parse_all(b);
            let _ = der::parse_exact(b, "x");
            let _ = der::dump(b, 0);
            let _ = der::parse_oid(b);
            let _ = der::parse_unsigned(b);
            let _ = der::parse_time(der::TAG_UTC_TIME, b);
            let _ = der::parse_bit_string(b);
        },
    ); }
    if want("x509") { total += fuzz_run(
        "x509/random",
        n,
        |r| {
            let len = (r.next() as usize) % 400;
            (0..len).map(|_| r.byte()).collect()
        },
        |b| {
            let _ = Certificate::parse(b);
        },
    ); }
    if want("p12") { total += fuzz_run(
        "p12/random",
        n,
        |r| {
            let len = (r.next() as usize) % 400;
            (0..len).map(|_| r.byte()).collect()
        },
        |b| {
            let _ = P12::load(b, "x");
        },
    ); }
    // 2) 结构化变异：截断 + 位翻转（更容易打进深层解析）
    let key = vmless_crypto::rsa::generate(1024, 65537).expect("keygen");
    let dn = vmless_crypto::asn1::Name::parse_dn_string("CN=Fuzz,O=Test").expect("dn");
    let cert = vmless_crypto::x509::build_self_signed(
        &key,
        &vmless_crypto::x509::CertSpec::new(dn, 30),
    )
    .expect("cert");
    let cert_c = cert.clone();
    if want("x509m") { total += fuzz_run(
        "x509/mutate",
        n,
        |r| {
            let mut v = cert_c.clone();
            match r.next() % 3 {
                0 => v.truncate((r.next() as usize) % v.len()),
                1 => {
                    let i = (r.next() as usize) % v.len();
                    v[i] ^= 1 << (r.next() % 8);
                }
                _ => {
                    for _ in 0..1 + (r.next() % 4) {
                        let i = (r.next() as usize) % v.len();
                        v[i] = r.byte();
                    }
                }
            }
            v
        },
        |b| {
            let _ = Certificate::parse(b);
        },
    ); }
    let p12 = vmless_crypto::pkcs12::build_single(
        "fuzz",
        key,
        vec![Certificate::parse(&cert).expect("parse cert")],
    )
    .store("pw")
    .expect("store");
    let p12_c = p12.clone();
    if want("p12m") { total += fuzz_run(
        "p12/mutate",
        n,
        |r| {
            let mut v = p12_c.clone();
            match r.next() % 2 {
                0 => v.truncate((r.next() as usize) % v.len()),
                _ => {
                    let i = (r.next() as usize) % v.len();
                    v[i] ^= 1 << (r.next() % 8);
                }
            }
            v
        },
        |b| {
            let _ = P12::load(b, "pw");
        },
    ); }
    total
}

fn bench_rsa(bits: usize, iters: usize) {
    use vmless_crypto::hash::Alg;
    use vmless_crypto::rsa;
    let t0 = std::time::Instant::now();
    let key = rsa::generate(bits, 65537).expect("keygen");
    let keygen = t0.elapsed().as_secs_f64() * 1000.0;
    let msg = b"jarsigner benchmark payload";
    let mut sign_ms = Vec::new();
    let mut verify_ms = Vec::new();
    let mut sig = Vec::new();
    for _ in 0..iters {
        let t = std::time::Instant::now();
        sig = rsa::sign(&key, Alg::Sha256, msg).expect("sign");
        sign_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        let t = std::time::Instant::now();
        rsa::verify(&key.public(), Alg::Sha256, msg, &sig).expect("verify");
        verify_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let med = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    println!(
        "RSA-{} 生成 {:.1} ms | 签名中位 {:.2} ms | 验签中位 {:.2} ms ({} 次)",
        bits,
        keygen,
        med(&mut sign_ms),
        med(&mut verify_ms),
        iters
    );
}

fn bench_p12(path: &str, pass: &str, iters: usize) {
    use vmless_crypto::pkcs12::P12;
    let bytes = std::fs::read(path).expect("读 p12");
    let t = std::time::Instant::now();
    let p = P12::load(&bytes, pass).expect("load");
    let first = t.elapsed().as_secs_f64() * 1000.0;
    let mut times = Vec::new();
    for _ in 0..iters {
        let t = std::time::Instant::now();
        let _ = P12::load(&bytes, pass).expect("load");
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "PKCS#12 {} 字节, {} 条目 | 首次 {:.1} ms | 中位 {:.1} ms ({} 次)",
        bytes.len(),
        p.entries.len(),
        first,
        times[times.len() / 2],
        iters
    );
}
