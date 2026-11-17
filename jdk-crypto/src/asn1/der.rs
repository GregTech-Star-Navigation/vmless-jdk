//! DER/BER 的 TLV 编解码。
//!
//! 解析侧**严格按 DER**：拒绝不定长（BER 的 `0x80`）、拒绝非最小长度编码、
//! 拒绝超长/越界的长度、拒绝负整数与冗余前导零。所有函数只返回 `Err`，不 panic。

use crate::bigint::BigUint;
use crate::sys;
use crate::{Error, Result};

// 通用 tag
pub const TAG_BOOLEAN: u8 = 0x01;
pub const TAG_INTEGER: u8 = 0x02;
pub const TAG_BIT_STRING: u8 = 0x03;
pub const TAG_OCTET_STRING: u8 = 0x04;
pub const TAG_NULL: u8 = 0x05;
pub const TAG_OID: u8 = 0x06;
pub const TAG_UTF8_STRING: u8 = 0x0c;
pub const TAG_SEQUENCE: u8 = 0x30;
pub const TAG_SET: u8 = 0x31;
pub const TAG_PRINTABLE_STRING: u8 = 0x13;
pub const TAG_T61_STRING: u8 = 0x14;
pub const TAG_IA5_STRING: u8 = 0x16;
pub const TAG_UTC_TIME: u8 = 0x17;
pub const TAG_GENERALIZED_TIME: u8 = 0x18;
pub const TAG_BMP_STRING: u8 = 0x1e;

/// 一个已解析的 TLV：`content` 是去掉 tag/len 的负载。
#[derive(Clone, Copy, Debug)]
pub struct Tlv<'a> {
    pub tag: u8,
    pub content: &'a [u8],
}

impl<'a> Tlv<'a> {
    pub fn is_constructed(&self) -> bool {
        self.tag & 0x20 != 0
    }
    pub fn class(&self) -> u8 {
        self.tag & 0xc0
    }
    pub fn number(&self) -> u8 {
        self.tag & 0x1f
    }
    pub fn is_context(&self) -> bool {
        self.class() == 0x80
    }
    pub fn expect(&self, tag: u8, what: &str) -> Result<&'a [u8]> {
        if self.tag != tag {
            return Err(Error::new(format!(
                "{what} 的 tag 应为 0x{tag:02x}，实际是 0x{:02x}",
                self.tag
            )));
        }
        Ok(self.content)
    }
}

/// 解析一个 TLV，返回它和剩余的字节。
pub fn parse_tlv(data: &[u8]) -> Result<(Tlv<'_>, &[u8])> {
    if data.is_empty() {
        return Err(Error::new("DER 截断：没有 tag 字节"));
    }
    let tag = data[0];
    if tag == 0 {
        return Err(Error::new("DER 里的 tag 不能是 0x00"));
    }
    if tag & 0x1f == 0x1f {
        return Err(Error::new("不支持多字节 tag（高 tag 号形式）"));
    }
    if data.len() < 2 {
        return Err(Error::new("DER 截断：没有长度字节"));
    }
    let first = data[1];
    let (len, hdr) = if first & 0x80 == 0 {
        (first as usize, 2usize)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 {
            return Err(Error::new("DER 不允许不定长编码（0x80）"));
        }
        if n > 8 {
            return Err(Error::new("DER 长度字段过长（>8 字节）"));
        }
        if data.len() < 2 + n {
            return Err(Error::new("DER 截断：长度字段不完整"));
        }
        if data[2] == 0 {
            return Err(Error::new("DER 长度用了非最小编码（前导 0）"));
        }
        let mut v: u64 = 0;
        for &b in &data[2..2 + n] {
            v = (v << 8) | b as u64;
        }
        if v < 128 {
            return Err(Error::new("DER 长度用了非最小编码（长形式表示 <128）"));
        }
        if v > usize::MAX as u64 {
            return Err(Error::new("DER 长度超出平台范围"));
        }
        (v as usize, 2 + n)
    };
    let end = hdr
        .checked_add(len)
        .ok_or_else(|| Error::new("DER 长度溢出"))?;
    if end > data.len() {
        return Err(Error::new(format!(
            "DER 截断：声明 {len} 字节，实际只剩 {} 字节",
            data.len() - hdr
        )));
    }
    Ok((Tlv { tag, content: &data[hdr..end] }, &data[end..]))
}

/// 解析一段数据里连续的 TLV。
pub fn parse_all(data: &[u8]) -> Result<Vec<Tlv<'_>>> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let (tlv, r) = parse_tlv(rest)?;
        out.push(tlv);
        rest = r;
    }
    Ok(out)
}

/// 严格解析一段数据为**恰好一个** TLV（多余字节报错）。
pub fn parse_exact<'a>(data: &'a [u8], what: &str) -> Result<Tlv<'a>> {
    let (tlv, rest) = parse_tlv(data)?;
    if !rest.is_empty() {
        return Err(Error::new(format!("{what} 后面还有 {} 字节多余数据", rest.len())));
    }
    Ok(tlv)
}

// ------------------------------ 值解析 ------------------------------

/// 解析整数内容为无符号大数（拒绝负数与冗余前导零）。
pub fn parse_unsigned(content: &[u8]) -> Result<BigUint> {
    if content.is_empty() {
        return Err(Error::new("INTEGER 内容为空"));
    }
    if content[0] & 0x80 != 0 {
        return Err(Error::new("INTEGER 是负数（本栈只处理无符号）"));
    }
    if content.len() > 1 && content[0] == 0 && content[1] & 0x80 == 0 {
        return Err(Error::new("INTEGER 有冗余前导 0（非 DER 最小编码）"));
    }
    Ok(BigUint::from_bytes_be(content))
}

pub fn parse_oid(content: &[u8]) -> Result<String> {
    if content.is_empty() {
        return Err(Error::new("OID 内容为空"));
    }
    if content.len() > 64 {
        return Err(Error::new("OID 过长（>64 字节）"));
    }
    let first = content[0] as u32;
    let mut parts: Vec<u64> = vec![(first / 40).min(2) as u64, (first % 40) as u64];
    if first >= 80 {
        parts[1] = (first - 80) as u64;
    }
    let mut val: u64 = 0;
    let mut started = false;
    for &b in &content[1..] {
        if val > (u64::MAX >> 7) {
            return Err(Error::new("OID 分量溢出"));
        }
        val = (val << 7) | (b & 0x7f) as u64;
        started = true;
        if b & 0x80 == 0 {
            parts.push(val);
            val = 0;
            started = false;
        }
    }
    if started {
        return Err(Error::new("OID 末段没有终止字节"));
    }
    Ok(parts
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join("."))
}

pub fn parse_bool(content: &[u8]) -> Result<bool> {
    if content.len() != 1 {
        return Err(Error::new("BOOLEAN 长度应为 1"));
    }
    match content[0] {
        0xff => Ok(true),
        0x00 => Ok(false),
        other => Err(Error::new(format!("DER BOOLEAN 只能是 0xFF/0x00，实际 0x{other:02x}"))),
    }
}

/// 解析 BIT STRING，返回（位串字节，未使用位数）。
pub fn parse_bit_string<'a>(content: &'a [u8]) -> Result<(&'a [u8], usize)> {
    if content.is_empty() {
        return Err(Error::new("BIT STRING 内容为空"));
    }
    let unused = content[0] as usize;
    if unused > 7 {
        return Err(Error::new("BIT STRING 的未使用位数 > 7"));
    }
    if content.len() == 1 && unused != 0 {
        return Err(Error::new("空 BIT STRING 不能有未使用位"));
    }
    Ok((&content[1..], unused))
}

pub fn parse_string(tag: u8, content: &[u8]) -> Result<String> {
    match tag {
        TAG_UTF8_STRING => std::str::from_utf8(content)
            .map(|s| s.to_string())
            .map_err(|_| Error::new("UTF8String 不是合法 UTF-8")),
        TAG_PRINTABLE_STRING | TAG_IA5_STRING | TAG_T61_STRING => {
            if !content.is_ascii() {
                return Err(Error::new("PrintableString/IA5String 含非 ASCII 字节"));
            }
            Ok(String::from_utf8_lossy(content).to_string())
        }
        TAG_BMP_STRING => {
            if content.len() % 2 != 0 {
                return Err(Error::new("BMPString 长度为奇数"));
            }
            let units: Vec<u16> = content
                .chunks(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16(&units).map_err(|_| Error::new("BMPString 不是合法 UTF-16"))
        }
        other => Err(Error::new(format!("不支持的字符串类型 tag=0x{other:02x}"))),
    }
}

pub fn parse_time(tag: u8, content: &[u8]) -> Result<i64> {
    let s = std::str::from_utf8(content).map_err(|_| Error::new("时间字段不是 ASCII"))?;
    // 去掉秒的小数部分（GeneralizedTime 允许，例如 20261117103000.123Z）
    let s = match s.find('.') {
        Some(pos) => {
            let (head, tail) = s.split_at(pos);
            let zone = tail.find(|c: char| c == 'Z' || c == '+' || c == '-').unwrap_or(tail.len());
            format!("{}{}", head, &tail[zone..])
        }
        None => s.to_string(),
    };
    match tag {
        TAG_UTC_TIME => {
            if s.len() < 11 || s.len() > 17 {
                return Err(Error::new(format!("UTCTime 长度不对: {s}")));
            }
            sys::parse_stamp(&s)
        }
        TAG_GENERALIZED_TIME => {
            if s.len() < 13 {
                return Err(Error::new(format!("GeneralizedTime 长度不对: {s}")));
            }
            sys::parse_stamp(&s)
        }
        other => Err(Error::new(format!("不是时间类型 tag=0x{other:02x}"))),
    }
}

/// 递归解析出一个可打印的树（`-printcert` 之类的人类可读输出用）。
pub fn dump(data: &[u8], indent: usize) -> String {
    let mut out = String::new();
    let mut rest = data;
    while !rest.is_empty() {
        match parse_tlv(rest) {
            Ok((tlv, r)) => {
                out.push_str(&"  ".repeat(indent));
                out.push_str(&format!("[{:02x}] len={}", tlv.tag, tlv.content.len()));
                if tlv.is_constructed() {
                    out.push('\n');
                    out.push_str(&dump(tlv.content, indent + 1));
                } else {
                    let preview: String = tlv
                        .content
                        .iter()
                        .take(24)
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    out.push_str(&format!(" {preview}\n"));
                }
                rest = r;
            }
            Err(e) => {
                out.push_str(&format!("<解析失败: {e}>\n"));
                break;
            }
        }
    }
    out
}

// ------------------------------ 编码 ------------------------------

pub fn enc_len(len: usize) -> Vec<u8> {
    if len < 0x80 {
        vec![len as u8]
    } else {
        let mut bytes = Vec::new();
        let mut v = len;
        while v > 0 {
            bytes.push((v & 0xff) as u8);
            v >>= 8;
        }
        bytes.reverse();
        let mut out = vec![0x80 | bytes.len() as u8];
        out.extend_from_slice(&bytes);
        out
    }
}

pub fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + 6);
    out.push(tag);
    out.extend_from_slice(&enc_len(content.len()));
    out.extend_from_slice(content);
    out
}

pub fn seq(items: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    for it in items {
        body.extend_from_slice(it);
    }
    tlv(TAG_SEQUENCE, &body)
}

pub fn set(items: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    for it in items {
        body.extend_from_slice(it);
    }
    tlv(TAG_SET, &body)
}

pub fn boolean(v: bool) -> Vec<u8> {
    tlv(TAG_BOOLEAN, &[if v { 0xff } else { 0x00 }])
}

pub fn null() -> Vec<u8> {
    tlv(TAG_NULL, &[])
}

pub fn octet_string(v: &[u8]) -> Vec<u8> {
    tlv(TAG_OCTET_STRING, v)
}

pub fn bit_string(v: &[u8], unused_bits: usize) -> Vec<u8> {
    let mut body = Vec::with_capacity(v.len() + 1);
    body.push(unused_bits as u8);
    body.extend_from_slice(v);
    tlv(TAG_BIT_STRING, &body)
}

pub fn utf8_string(v: &str) -> Vec<u8> {
    tlv(TAG_UTF8_STRING, v.as_bytes())
}

pub fn printable_string(v: &str) -> Vec<u8> {
    tlv(TAG_PRINTABLE_STRING, v.as_bytes())
}

pub fn oid(oid_content: &[u8]) -> Vec<u8> {
    tlv(TAG_OID, oid_content)
}

/// 把点分 OID 字符串编码成 DER。
pub fn oid_from_str(s: &str) -> Result<Vec<u8>> {
    let parts: Vec<u64> = s
        .split('.')
        .map(|p| p.parse::<u64>().map_err(|_| Error::new(format!("OID 分量不合法: {p}"))))
        .collect::<Result<Vec<_>>>()?;
    if parts.len() < 2 || parts[0] > 2 || (parts[0] < 2 && parts[1] > 39) {
        return Err(Error::new(format!("OID 不合法: {s}")));
    }
    let mut body = vec![(parts[0] * 40 + parts[1]) as u8];
    for &p in &parts[2..] {
        let mut tmp = Vec::new();
        let mut v = p;
        loop {
            tmp.push((v & 0x7f) as u8);
            v >>= 7;
            if v == 0 {
                break;
            }
        }
        for i in (0..tmp.len()).rev() {
            body.push(if i == 0 { tmp[i] } else { tmp[i] | 0x80 });
        }
    }
    Ok(tlv(TAG_OID, &body))
}

/// 无符号整数编码（自动补 0x00 防负数解释）。
pub fn integer(v: &BigUint) -> Vec<u8> {
    let mut bytes = v.to_bytes_be();
    if bytes.is_empty() {
        bytes.push(0);
    }
    if bytes[0] & 0x80 != 0 {
        bytes.insert(0, 0);
    }
    tlv(TAG_INTEGER, &bytes)
}

pub fn integer_u32(v: u32) -> Vec<u8> {
    integer(&BigUint::from_u32(v))
}

pub fn utc_time(unix: i64) -> Vec<u8> {
    let (y, mo, d, h, mi, s) = sys::civil_from_unix(unix);
    if (1950..2050).contains(&y) {
        tlv(TAG_UTC_TIME, format!("{:02}{mo:02}{d:02}{h:02}{mi:02}{s:02}Z", y % 100).as_bytes())
    } else {
        tlv(TAG_GENERALIZED_TIME, format!("{y:04}{mo:02}{d:02}{h:02}{mi:02}{s:02}Z").as_bytes())
    }
}

pub fn generalized_time(unix: i64) -> Vec<u8> {
    tlv(TAG_GENERALIZED_TIME, sys::utc_stamp(unix).as_bytes())
}

/// 显式标签 `[n] EXPLICIT`（构造型）。
pub fn explicit(n: u8, inner: &[u8]) -> Vec<u8> {
    tlv(0xa0 | (n & 0x1f), inner)
}

/// 隐式标签里的原始字节（构造型，内容已是 TLV 串）。
pub fn context_constructed(n: u8, inner: &[u8]) -> Vec<u8> {
    tlv(0xa0 | (n & 0x1f), inner)
}

pub fn context_primitive(n: u8, inner: &[u8]) -> Vec<u8> {
    tlv(0x80 | (n & 0x1f), inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_lengths() {
        for len in [0usize, 1, 127, 128, 255, 256, 65535, 65536] {
            let data = vec![0x41u8; len];
            let enc = octet_string(&data);
            let (tlv, rest) = parse_tlv(&enc).unwrap();
            assert!(rest.is_empty());
            assert_eq!(tlv.tag, TAG_OCTET_STRING);
            assert_eq!(tlv.content, &data[..]);
        }
    }

    #[test]
    fn rejects_malformed() {
        let bad: &[&[u8]] = &[
            &[],
            &[0x30],
            &[0x30, 0x80],
            &[0x30, 0x83, 0x01],
            &[0x30, 0x84, 0xff, 0xff, 0xff, 0xff],
            &[0x30, 0x82, 0x00, 0x05, 1, 2, 3, 4, 5],
            &[0x00, 0x00],
            &[0x1f, 0x01, 0x00],
            &[0x02, 0x01],
        ];
        for b in bad {
            assert!(parse_tlv(b).is_err(), "应拒绝 {b:02x?}");
        }
    }

    #[test]
    fn integer_rules() {
        assert_eq!(parse_unsigned(&[0x00]).unwrap(), BigUint::zero());
        assert_eq!(parse_unsigned(&[0x7f]).unwrap().to_hex(), "7f");
        assert_eq!(parse_unsigned(&[0x00, 0x80]).unwrap().to_hex(), "80");
        assert!(parse_unsigned(&[0x00, 0x01]).is_err()); // 冗余前导 0
        assert!(parse_unsigned(&[0x80]).is_err()); // 负数
        assert!(parse_unsigned(&[]).is_err());
        assert_eq!(integer(&BigUint::from_u32(0x80)), vec![0x02, 0x02, 0x00, 0x80]);
        assert_eq!(integer(&BigUint::zero()), vec![0x02, 0x01, 0x00]);
    }

    #[test]
    fn oid_roundtrip() {
        let cases = [
            "1.2.840.113549.1.1.11",
            "2.5.4.3",
            "2.16.840.1.101.3.4.1.42",
            "1.3.14.3.2.26",
            "1.2.840.113549.1.12.10.1.2",
        ];
        for c in cases {
            let der = oid_from_str(c).unwrap();
            let (tlv, rest) = parse_tlv(&der).unwrap();
            assert!(rest.is_empty());
            assert_eq!(parse_oid(tlv.content).unwrap(), c);
        }
        assert!(oid_from_str("1").is_err());
        assert!(oid_from_str("3.1").is_err());
        assert!(parse_oid(&[]).is_err());
        assert!(parse_oid(&[0x2b, 0x86]).is_err());
    }

    #[test]
    fn time_roundtrip() {
        let t = 1_797_000_000i64;
        for enc in [utc_time(t), generalized_time(t)] {
            let (tlv, _) = parse_tlv(&enc).unwrap();
            assert_eq!(parse_time(tlv.tag, tlv.content).unwrap(), t);
        }
        let old = -1_000_000_000i64; // 1938 -> 必须用 GeneralizedTime
        let enc = utc_time(old);
        let (tlv, _) = parse_tlv(&enc).unwrap();
        assert_eq!(tlv.tag, TAG_GENERALIZED_TIME);
        assert_eq!(parse_time(tlv.tag, tlv.content).unwrap(), old);
    }

    #[test]
    fn bit_string_rules() {
        let enc = bit_string(&[0xde, 0xad], 0);
        let (tlv, _) = parse_tlv(&enc).unwrap();
        assert_eq!(parse_bit_string(tlv.content).unwrap(), (&[0xde, 0xad][..], 0));
        assert!(parse_bit_string(&[]).is_err());
        assert!(parse_bit_string(&[9, 1]).is_err());
    }

    #[test]
    fn dump_is_safe_on_garbage() {
        // 随机垃圾只应产生文本或错误，绝不 panic
        for seed in 0u32..500 {
            let mut v = Vec::new();
            let mut x = seed.wrapping_mul(2654435761).wrapping_add(1);
            for _ in 0..(seed % 40) {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                v.push(x as u8);
            }
            let _ = dump(&v, 0);
            let _ = parse_all(&v);
        }
    }
}
