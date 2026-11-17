//! 系统层：随机源、时钟格式化、以及一些各模块共用的小工具。
//!
//! 随机源直接用操作系统的 CSPRNG（Windows `BCryptGenRandom`，Unix `/dev/urandom`），
//! 不自己造 PRNG —— 密钥生成与 salt 都靠它。

use crate::{Error, Result};

/// 填充随机字节。
pub fn random_bytes(buf: &mut [u8]) -> Result<()> {
    if buf.is_empty() {
        return Ok(());
    }
    #[cfg(windows)]
    {
        // BCryptGenRandom(hAlgorithm=NULL, pbBuffer, cbBuffer, BCRYPT_USE_SYSTEM_PREFERRED_RNG=2)
        #[link(name = "bcrypt")]
        extern "system" {
            fn BCryptGenRandom(
                h_algorithm: *mut core::ffi::c_void,
                pb_buffer: *mut u8,
                cb_buffer: u32,
                dw_flags: u32,
            ) -> i32;
        }
        let mut off = 0usize;
        while off < buf.len() {
            let n = (buf.len() - off).min(1 << 20);
            let status = unsafe {
                BCryptGenRandom(core::ptr::null_mut(), buf[off..].as_mut_ptr(), n as u32, 2)
            };
            if status != 0 {
                return Err(Error::new(format!("系统随机源失败（BCryptGenRandom 返回 {status}）")));
            }
            off += n;
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        use std::io::Read;
        let mut f = std::fs::File::open("/dev/urandom")
            .map_err(|e| Error::new(format!("打不开 /dev/urandom: {e}")))?;
        let mut off = 0usize;
        while off < buf.len() {
            let n = f
                .read(&mut buf[off..])
                .map_err(|e| Error::new(format!("读 /dev/urandom 失败: {e}")))?;
            if n == 0 {
                return Err(Error::new("/dev/urandom 提前 EOF"));
            }
            off += n;
        }
        Ok(())
    }
}

pub fn random_vec(n: usize) -> Result<Vec<u8>> {
    let mut v = vec![0u8; n];
    random_bytes(&mut v)?;
    Ok(v)
}

/// 当前 Unix 时间（秒）。
pub fn now_unix() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => 0,
    }
}

/// 系统本地时区相对 UTC 的偏移秒数（Windows 取当前时区偏差，Unix 恒 0 由调用方处理）。
#[cfg(windows)]
pub fn local_offset_secs() -> i64 {
    #[repr(C)]
    struct SystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetLocalTime(lp: *mut SystemTime);
        fn GetSystemTime(lp: *mut SystemTime);
    }
    let mut l = SystemTime {
        year: 0,
        month: 0,
        day_of_week: 0,
        day: 0,
        hour: 0,
        minute: 0,
        second: 0,
        milliseconds: 0,
    };
    let mut u = SystemTime {
        year: 0,
        month: 0,
        day_of_week: 0,
        day: 0,
        hour: 0,
        minute: 0,
        second: 0,
        milliseconds: 0,
    };
    unsafe {
        GetLocalTime(&mut l);
        GetSystemTime(&mut u);
    }
    let days_from_civil = |y: i64, m: i64, d: i64| -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let mp = (m + 9) % 12;
        let doy = (153 * mp + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146097 + doe - 719468
    };
    let to_secs = |t: &SystemTime| -> i64 {
        days_from_civil(t.year as i64, t.month as i64, t.day as i64) * 86400
            + t.hour as i64 * 3600
            + t.minute as i64 * 60
            + t.second as i64
    };
    to_secs(&l) - to_secs(&u)
}

#[cfg(not(windows))]
pub fn local_offset_secs() -> i64 {
    8 * 3600
}

/// 把 Unix 秒拆成 (年, 月, 日, 时, 分, 秒) —— 公历换算，不依赖本地时区。
pub fn civil_from_unix(t: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = t.div_euclid(86400);
    let secs = t.rem_euclid(86400);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m as u32,
        d as u32,
        (secs / 3600) as u32,
        ((secs % 3600) / 60) as u32,
        (secs % 60) as u32,
    )
}

/// Unix 秒 -> UTC 的 "YYYYMMDDHHMMSSZ"（DER GeneralizedTime / UTCTime 用）。
pub fn utc_stamp(t: i64) -> String {
    let (y, mo, d, h, mi, s) = civil_from_unix(t);
    format!("{y:04}{mo:02}{d:02}{h:02}{mi:02}{s:02}Z")
}

/// Unix 秒 -> 便于人读的 "YYYY-MM-DD HH:MM:SS"（UTC+offset）。
pub fn human_stamp(t: i64, offset_secs: i64) -> String {
    let (y, mo, d, h, mi, s) = civil_from_unix(t + offset_secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// 从 "YYYYMMDDHHMMSSZ"/"YYMMDDHHMMSSZ"/"YYYY-MM-DD HH:MM:SS" 解析 Unix 秒。
pub fn parse_stamp(s: &str) -> Result<i64> {
    let t: Vec<char> = s.trim().chars().filter(|c| !c.is_whitespace()).collect();
    let digits: String = t.iter().filter(|c| c.is_ascii_digit()).collect();
    let is_utc_time = digits.len() == 12;
    if digits.len() < 12 {
        return Err(Error::new(format!("时间格式无法识别: {s}")));
    }
    let num = |a: usize, b: usize| -> i64 { digits[a..b.min(digits.len())].parse::<i64>().unwrap_or(0) };
    let (y, mo, d, h, mi, sec) = if is_utc_time {
        let y2 = num(0, 2);
        (if y2 >= 50 { 1900 + y2 } else { 2000 + y2 }, num(2, 4), num(4, 6), num(6, 8), num(8, 10), num(10, 12))
    } else {
        (num(0, 4), num(4, 6), num(6, 8), num(8, 10), num(10, 12), num(12, 14))
    };
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return Err(Error::new(format!("时间字段越界: {s}")));
    }
    let days = days_from_civil(y, mo, d);
    Ok(days * 86400 + h * 3600 + mi * 60 + sec)
}

pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// 十六进制打印（调试/`-v` 输出）。
pub fn hex(d: &[u8]) -> String {
    crate::hash::hex(d)
}

/// 严格 base64 编码（RFC 4648，带 `=` 填充）。
pub fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn base64_decode(s: &str) -> Result<Vec<u8>> {
    let mut rev = [255u8; 256];
    for (i, c) in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/".iter().enumerate() {
        rev[*c as usize] = i as u8;
    }
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0u32;
    for c in s.bytes() {
        if c.is_ascii_whitespace() || c == b'=' {
            continue;
        }
        let v = rev[c as usize];
        if v == 255 {
            return Err(Error::new(format!("base64 里有非法字符: {}", c as char)));
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

/// PEM 包裹。
pub fn pem_encode(label: &str, der: &[u8]) -> String {
    let b64 = base64_encode(der);
    let mut s = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        s.push_str(std::str::from_utf8(chunk).unwrap_or(""));
        s.push('\n');
    }
    s.push_str(&format!("-----END {label}-----\n"));
    s
}

/// 从 PEM 里取出指定 label 的 DER；没有 label 时取第一个块。
pub fn pem_decode(s: &str, label: Option<&str>) -> Result<Vec<u8>> {
    let mut blocks: Vec<(String, String)> = Vec::new();
    let mut cur_label = String::new();
    let mut body = String::new();
    let mut in_block = false;
    for line in s.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("-----BEGIN ") {
            cur_label = rest.trim_end_matches("-----").to_string();
            body.clear();
            in_block = true;
        } else if l.starts_with("-----END ") {
            if in_block {
                blocks.push((cur_label.clone(), body.clone()));
            }
            in_block = false;
        } else if in_block {
            body.push_str(l);
        }
    }
    let pick = match label {
        Some(want) => blocks.iter().find(|(l, _)| l == want),
        None => blocks.first(),
    };
    match pick {
        Some((_, b)) => base64_decode(b),
        None => Err(Error::new(match label {
            Some(l) => format!("PEM 里没有 {l} 块"),
            None => "没有找到 PEM 块".into(),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_roundtrip() {
        for t in [0i64, 1, 951782400, 1_700_000_000, 2_000_000_000, 4_102_444_800] {
            let s = utc_stamp(t);
            assert_eq!(parse_stamp(&s).unwrap(), t, "{s}");
        }
        assert_eq!(utc_stamp(0), "19700101000000Z");
        assert_eq!(human_stamp(1_700_000_000, 8 * 3600), "2023-11-15 06:13:20");
    }

    #[test]
    fn base64_roundtrip() {
        for n in [0usize, 1, 2, 3, 4, 100] {
            let d: Vec<u8> = (0..n).map(|i| (i * 7 + 1) as u8).collect();
            assert_eq!(base64_decode(&base64_encode(&d)).unwrap(), d);
        }
        assert_eq!(base64_encode(b"abc"), "YWJj");
        assert!(base64_decode("!!!!").is_err());
    }

    #[test]
    fn random_is_not_constant() {
        let a = random_vec(32).unwrap();
        let b = random_vec(32).unwrap();
        assert_ne!(a, b);
        assert!(a.iter().any(|x| *x != 0));
    }
}
