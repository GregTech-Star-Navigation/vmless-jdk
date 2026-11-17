//! `.jfr` 二进制格式的底层原语：magic、变长整数、游标、错误。
//!
//! # 这份格式的真相来源
//!
//! 不是猜的，是对着 `src.zip` 里的 `jdk.jfr.internal.consumer.*` 逐行读出来的：
//!
//! | 字节含义 | 权威实现 |
//! | --- | --- |
//! | chunk 头 68 字节的字段偏移 | `ChunkHeader.<clinit>` + `refresh()` |
//! | 变长整数（1~9 字节） | `RecordingInput.readLong()` |
//! | 元数据的字符串池 + 元素树 | `MetadataWriter.writeBinary()` / `MetadataReader` |
//! | 事件块 `[size][typeId][payload]` | `ChunkParser.readStreamingEvent()` |
//! | 常量池链 `delta → 下一块` | `ChunkParser.fillConstantPools()` |
//! | 值的编码（float/double 是裸大端） | `ParserFactory` 里各 `*Parser` |
//!
//! 所有读取都走 [`Cursor`]，**越界一律返回错误，不 panic**。这是 fuzz 5 万例的前提。

use std::fmt;

pub mod chunk;
pub mod cp;
pub mod event;
pub mod metadata;

/// 所有格式层错误的统一类型。
///
/// 不用 `io::Error` 的原因：这里大部分错误是「字节不对」而不是「IO 出错」，
/// 而且需要原样带给用户看（哪个偏移、期望什么）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    msg: String,
}

impl Error {
    pub fn new<S: Into<String>>(m: S) -> Self {
        Error { msg: m.into() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::new(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// `bail!("...")` —— 构造一条格式错误。
#[macro_export]
macro_rules! bail {
    ($($arg:tt)*) => {
        return Err($crate::format::Error::new(format!($($arg)*)))
    };
}

/// 文件魔数 `FLR\0`。
pub const MAGIC: [u8; 4] = *b"FLR\0";

/// chunk 头固定长度（`ChunkHeader.HEADER_SIZE`）。
pub const HEADER_SIZE: u64 = 68;

/// 元数据块的事件类型 id（`ChunkHeader.METADATA_TYPE_ID`）。
pub const BLOCK_METADATA: i64 = 0;

/// checkpoint 块的事件类型 id（`ChunkParser.CONSTANT_POOL_TYPE_ID`）。
pub const BLOCK_CHECKPOINT: i64 = 1;

/// checkpoint 的类型位（`ChunkParser.CheckpointType`）。
pub const CP_FLUSH: u8 = 1;
pub const CP_CHUNK_HEADER: u8 = 2;
pub const CP_STATICS: u8 = 4;
pub const CP_THREAD: u8 = 8;

/// 递归解析嵌套结构的上限。
///
/// 真实录制里最深的链是 `OldObjectSample → reference chain`，个位数层；
/// 32 足够，再深就是坏数据（或圈），必须断开而不是栈溢出。
pub const MAX_DEPTH: usize = 32;

/// 单个字符串的上限。真实录制里最长的是 classloader 路径，几十 KB 级。
pub const MAX_STRING_BYTES: usize = 16 * 1024 * 1024;

/// 单个数组的上限（元素个数）。
pub const MAX_ARRAY_ELEMS: usize = 4 * 1024 * 1024;

/// 变长整数编码后的字节数（与 [`write_leb`] 逐字节一致）。
pub fn leb_len(v: i64) -> usize {
    let mut x = v as u64;
    for i in 0..9 {
        if i == 8 {
            return 9;
        }
        let b = (x & 0x7f) as u8;
        x >>= 7;
        if x == 0 && (b & 0x40) == 0 {
            return i + 1;
        }
    }
    9
}

/// 一个带边界检查的读游标。
///
/// 为什么不直接用 `&[u8]` + 索引：索引越界是 panic，而这里每读一个字节都可能越界
/// （截断的 .jfr）。游标把所有读操作收敛成返回 `Result`。
#[derive(Debug, Clone, Copy)]
pub struct Cursor<'a> {
    pub buf: &'a [u8],
    pub pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Cursor { buf, pos: 0 }
    }

    pub fn at(buf: &'a [u8], pos: usize) -> Self {
        Cursor { buf, pos }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    pub fn at_end(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn need(&self, n: usize) -> Result<()> {
        if self.remaining() < n {
            bail!(
                "数据不足：偏移 {} 处需要 {} 字节，只剩 {}",
                self.pos,
                n,
                self.remaining()
            );
        }
        Ok(())
    }

    pub fn u8(&mut self) -> Result<u8> {
        self.need(1)?;
        let b = self.buf[self.pos];
        self.pos += 1;
        Ok(b)
    }

    pub fn peek_u8(&self) -> Result<u8> {
        self.need(1)?;
        Ok(self.buf[self.pos])
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        self.need(n)?;
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    /// 变长整数：每字节 7 位，小端序；第 9 字节是裸 8 位（符号位）。
    pub fn leb(&mut self) -> Result<i64> {
        let mut ret: i64 = 0;
        for i in 0..8 {
            let b = self.u8()?;
            ret = ret.wrapping_add(((b & 0x7f) as i64) << (7 * i));
            if b < 0x80 {
                return Ok(ret);
            }
        }
        let b8 = self.u8()?;
        // 与 RecordingInput.readLong 一致：最后一字节是裸 8 位，左移 56。
        ret = ret.wrapping_add(((b8 as u64) << 56) as i64);
        Ok(ret)
    }

    pub fn be_u16(&mut self) -> Result<u16> {
        let s = self.take(2)?;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }

    pub fn be_u32(&mut self) -> Result<u32> {
        let s = self.take(4)?;
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }

    pub fn be_u64(&mut self) -> Result<u64> {
        let s = self.take(8)?;
        Ok(u64::from_be_bytes([
            s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
        ]))
    }

    /// `float` 是**裸大端 4 字节**，不是变长整数（`ParserFactory.FloatParser`）。
    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.be_u32()?))
    }

    pub fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.be_u64()?))
    }

    pub fn skip(&mut self, n: u64) -> Result<()> {
        let n = usize::try_from(n).map_err(|_| Error::new("偏移超出平台可表示范围"))?;
        self.need(n)?;
        self.pos += n;
        Ok(())
    }

    pub fn seek(&mut self, pos: u64) -> Result<()> {
        let pos = usize::try_from(pos).map_err(|_| Error::new("偏移超出平台可表示范围"))?;
        if pos > self.buf.len() {
            bail!("偏移 {} 超出数据长度 {}", pos, self.buf.len());
        }
        self.pos = pos;
        Ok(())
    }
}

/// 把变长整数写进 `out`。
pub fn write_leb(out: &mut Vec<u8>, v: i64) {
    let mut x = v as u64;
    for i in 0..9 {
        if i == 8 {
            // 第 9 字节：裸 8 位
            out.push((x & 0xff) as u8);
            return;
        }
        let b = (x & 0x7f) as u8;
        x >>= 7;
        // 已经能表达完，且最高位为 0（保证读侧停在这里）
        if x == 0 && (b & 0x40) == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// 写一个「块」：`[size][typeId][payload]`，`size` 含自身。
///
/// size 的字节数取决于 size 本身，所以要迭代一次收敛。
pub fn push_block(out: &mut Vec<u8>, type_id: i64, payload: &[u8]) -> u64 {
    let mut total = payload.len() as u64 + leb_len_u64(type_id as u64) as u64;
    let size_len = leb_len_u64(total) as u64;
    total += size_len;
    // 多算的那一位可能又跨过一个 7 位的边界，再收敛一次
    let size_len2 = leb_len_u64(total) as u64;
    let total = if size_len2 == size_len {
        total
    } else {
        total + (size_len2 - size_len) as u64
    };
    write_leb(out, total as i64);
    write_leb(out, type_id);
    out.extend_from_slice(payload);
    total
}

fn leb_len_u64(v: u64) -> usize {
    let mut n = 1;
    let mut x = v >> 7;
    while x != 0 {
        n += 1;
        x >>= 7;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leb_roundtrip() {
        for v in [
            0i64, 1, 5, 100, 127, 128, 300, 16384, 1 << 20, 1 << 40, 1 << 55,
        ] {
            let mut b = Vec::new();
            write_leb(&mut b, v);
            let mut c = Cursor::new(&b);
            assert_eq!(c.leb().unwrap(), v, "v={}", v);
            assert!(c.at_end(), "v={} 多写了字节", v);
        }
        // 负数走 9 字节形态
        for v in [-1i64, -2, i64::MIN, -1234567890123] {
            let mut b = Vec::new();
            write_leb(&mut b, v);
            assert_eq!(b.len(), 9, "v={}", v);
            let mut c = Cursor::new(&b);
            assert_eq!(c.leb().unwrap(), v);
        }
    }

    #[test]
    fn leb_len_matches_write() {
        for v in [0i64, 1, 63, 64, 127, 128, 16383, 16384, 1 << 30, 1 << 50] {
            let mut b = Vec::new();
            write_leb(&mut b, v);
            assert_eq!(leb_len(v), b.len(), "v={}", v);
        }
    }

    #[test]
    fn cursor_no_panic_on_short_input() {
        let data: &[u8] = &[0x80];
        let mut c = Cursor::new(data);
        assert!(c.leb().is_err());
        let mut c = Cursor::new(&[]);
        assert!(c.u8().is_err());
        assert!(c.be_u64().is_err());
    }
}
