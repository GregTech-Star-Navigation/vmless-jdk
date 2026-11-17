//! ZIP 读写（JAR 就是 ZIP）。零依赖：自己实现 raw DEFLATE 解码（RFC 1951）与 CRC-32。
//!
//! - 读：中央目录 → 本地头 → 取原始压缩字节 → inflate
//! - 写：整包重写（已有条目原样搬运压缩字节，新条目用 STORED）
//!
//! 不支持 ZIP64（>4GB / >65535 条目），遇到时明确报错而不是给出坏结果。

use crate::{Error, Result};

// ------------------------------ CRC-32 ------------------------------

fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    for i in 0..256u32 {
        let mut c = i;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        t[i as usize] = c;
    }
    t
}

pub fn crc32(data: &[u8]) -> u32 {
    let t = crc_table();
    let mut c = 0xffff_ffffu32;
    for &b in data {
        c = t[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    c ^ 0xffff_ffff
}

// ------------------------------ inflate ------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bitbuf: u32,
    bitcnt: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0, bitbuf: 0, bitcnt: 0 }
    }
    fn need(&mut self, n: u32) -> Result<()> {
        while self.bitcnt < n {
            if self.pos >= self.data.len() {
                return Err(Error::new("DEFLATE 数据提前结束"));
            }
            self.bitbuf |= (self.data[self.pos] as u32) << self.bitcnt;
            self.pos += 1;
            self.bitcnt += 8;
        }
        Ok(())
    }
    fn bits(&mut self, n: u32) -> Result<u32> {
        if n == 0 {
            return Ok(0);
        }
        self.need(n)?;
        let v = self.bitbuf & ((1u32 << n) - 1);
        self.bitbuf >>= n;
        self.bitcnt -= n;
        Ok(v)
    }
    fn align(&mut self) {
        let drop = self.bitcnt % 8;
        self.bitbuf >>= drop;
        self.bitcnt -= drop;
    }
}

struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Huffman> {
        let mut counts = [0u16; 16];
        for &l in lengths {
            if l > 15 {
                return Err(Error::new("DEFLATE 码长超过 15"));
            }
            counts[l as usize] += 1;
        }
        counts[0] = 0;
        // 检查码树是否过订阅（Kraft 不等式）
        let mut left = 1i32;
        for len in 1..16 {
            left <<= 1;
            left -= counts[len] as i32;
            if left < 0 {
                return Err(Error::new("DEFLATE 霍夫曼码过订阅"));
            }
        }
        let mut offsets = [0u16; 16];
        for len in 1..15 {
            offsets[len + 1] = offsets[len] + counts[len];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                let idx = offsets[l as usize] as usize;
                if idx >= symbols.len() {
                    return Err(Error::new("DEFLATE 符号表越界"));
                }
                symbols[idx] = sym as u16;
                offsets[l as usize] += 1;
            }
        }
        Ok(Huffman { counts, symbols })
    }

    fn decode(&self, br: &mut BitReader<'_>) -> Result<u16> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..16 {
            code |= br.bits(1)? as i32;
            let count = self.counts[len] as i32;
            if code - count < first {
                let i = (index + (code - first)) as usize;
                return self
                    .symbols
                    .get(i)
                    .copied()
                    .ok_or_else(|| Error::new("DEFLATE 符号索引越界"));
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(Error::new("DEFLATE 非法霍夫曼码"))
    }
}

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

fn fixed_tables() -> Result<(Huffman, Huffman)> {
    let mut lit_lens = [0u8; 288];
    for i in 0..144 {
        lit_lens[i] = 8;
    }
    for i in 144..256 {
        lit_lens[i] = 9;
    }
    for i in 256..280 {
        lit_lens[i] = 7;
    }
    for i in 280..288 {
        lit_lens[i] = 8;
    }
    let dist_lens = [5u8; 30];
    Ok((Huffman::new(&lit_lens)?, Huffman::new(&dist_lens)?))
}

fn inflate_block(
    br: &mut BitReader<'_>,
    out: &mut Vec<u8>,
    lit: &Huffman,
    dist: &Huffman,
    max_out: usize,
) -> Result<()> {
    loop {
        let sym = lit.decode(br)?;
        if sym < 256 {
            if out.len() >= max_out {
                return Err(Error::new("DEFLATE 解压超过预期长度"));
            }
            out.push(sym as u8);
        } else if sym == 256 {
            return Ok(());
        } else {
            let idx = (sym - 257) as usize;
            if idx >= LEN_BASE.len() {
                return Err(Error::new("DEFLATE 长度符号越界"));
            }
            let len = LEN_BASE[idx] as usize + br.bits(LEN_EXTRA[idx] as u32)? as usize;
            let dsym = dist.decode(br)? as usize;
            if dsym >= DIST_BASE.len() {
                return Err(Error::new("DEFLATE 距离符号越界"));
            }
            let d = DIST_BASE[dsym] as usize + br.bits(DIST_EXTRA[dsym] as u32)? as usize;
            if d == 0 || d > out.len() {
                return Err(Error::new("DEFLATE 回溯距离非法"));
            }
            if out.len() + len > max_out {
                return Err(Error::new("DEFLATE 解压超过预期长度"));
            }
            let start = out.len() - d;
            for i in 0..len {
                let b = out[start + i];
                out.push(b);
            }
        }
    }
}

/// raw DEFLATE 解压。`max_out` 是预期的解压长度上限（防止 zip 炸弹）。
pub fn inflate(data: &[u8], max_out: usize) -> Result<Vec<u8>> {
    let mut br = BitReader::new(data);
    let mut out = Vec::with_capacity(max_out.min(1 << 20));
    loop {
        let final_block = br.bits(1)? == 1;
        let btype = br.bits(2)?;
        match btype {
            0 => {
                br.align();
                let len = br.bits(16)? as usize;
                let nlen = br.bits(16)? as usize;
                if len != (!nlen & 0xffff) {
                    return Err(Error::new("DEFLATE 存储块长度校验失败"));
                }
                for _ in 0..len {
                    if out.len() >= max_out {
                        return Err(Error::new("DEFLATE 解压超过预期长度"));
                    }
                    out.push(br.bits(8)? as u8);
                }
            }
            1 => {
                let (lit, dist) = fixed_tables()?;
                inflate_block(&mut br, &mut out, &lit, &dist, max_out)?;
            }
            2 => {
                let hlit = br.bits(5)? as usize + 257;
                let hdist = br.bits(5)? as usize + 1;
                let hclen = br.bits(4)? as usize + 4;
                if hlit > 286 || hdist > 30 {
                    return Err(Error::new("DEFLATE 动态码树参数越界"));
                }
                const ORDER: [usize; 19] =
                    [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
                let mut cl_lens = [0u8; 19];
                for i in 0..hclen {
                    cl_lens[ORDER[i]] = br.bits(3)? as u8;
                }
                let cl = Huffman::new(&cl_lens)?;
                let mut lengths = vec![0u8; hlit + hdist];
                let mut i = 0usize;
                while i < lengths.len() {
                    let sym = cl.decode(&mut br)?;
                    match sym {
                        0..=15 => {
                            lengths[i] = sym as u8;
                            i += 1;
                        }
                        16 => {
                            if i == 0 {
                                return Err(Error::new("DEFLATE 重复码出现在开头"));
                            }
                            let prev = lengths[i - 1];
                            let rep = 3 + br.bits(2)? as usize;
                            for _ in 0..rep {
                                if i >= lengths.len() {
                                    return Err(Error::new("DEFLATE 码长重复越界"));
                                }
                                lengths[i] = prev;
                                i += 1;
                            }
                        }
                        17 => {
                            let rep = 3 + br.bits(3)? as usize;
                            i = (i + rep).min(lengths.len());
                        }
                        18 => {
                            let rep = 11 + br.bits(7)? as usize;
                            i = (i + rep).min(lengths.len());
                        }
                        _ => return Err(Error::new("DEFLATE 非法码长符号")),
                    }
                }
                let lit = Huffman::new(&lengths[..hlit])?;
                let dist = Huffman::new(&lengths[hlit..])?;
                inflate_block(&mut br, &mut out, &lit, &dist, max_out)?;
            }
            _ => return Err(Error::new("DEFLATE 块类型 3 非法")),
        }
        if final_block {
            break;
        }
    }
    Ok(out)
}

// ------------------------------ ZIP ------------------------------

#[derive(Clone, Debug)]
pub struct ZipEntry {
    pub name: String,
    pub method: u16,
    pub flags: u16,
    pub crc: u32,
    pub csize: u32,
    pub usize_: u32,
    pub mod_time: u16,
    pub mod_date: u16,
    pub extra: Vec<u8>,
    pub comment: Vec<u8>,
    /// 原始（压缩后）字节，写回时原样搬运。
    pub raw: Vec<u8>,
    /// 解压后的内容（惰性解压后填充）。
    pub data: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Default)]
pub struct Zip {
    pub entries: Vec<ZipEntry>,
}

fn u16le(d: &[u8], off: usize) -> Result<u16> {
    let s = d.get(off..off + 2).ok_or_else(|| Error::new("ZIP 缓冲越界"))?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

fn u32le(d: &[u8], off: usize) -> Result<u32> {
    let s = d.get(off..off + 4).ok_or_else(|| Error::new("ZIP 缓冲越界"))?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

impl Zip {
    pub fn open(bytes: &[u8]) -> Result<Zip> {
        if bytes.len() < 22 {
            return Err(Error::new("不是 ZIP 文件（太短）"));
        }
        // 从尾部找 EOCD（最多 64KB 注释）
        let mut eocd = None;
        let start = bytes.len().saturating_sub(22 + 65535);
        let mut i = bytes.len() - 22;
        loop {
            if u32le(bytes, i).map(|v| v == 0x0605_4b50).unwrap_or(false) {
                eocd = Some(i);
                break;
            }
            if i == start {
                break;
            }
            i -= 1;
        }
        let eocd = eocd.ok_or_else(|| Error::new("ZIP 里没有找到中央目录结束标记（不是合法 JAR/ZIP）"))?;
        let total = u16le(bytes, eocd + 10)? as usize;
        let cd_size = u32le(bytes, eocd + 12)? as usize;
        let cd_off = u32le(bytes, eocd + 16)? as usize;
        if total == 0xffff || cd_size == 0xffff_ffff || cd_off == 0xffff_ffff {
            return Err(Error::new("不支持 ZIP64 格式"));
        }
        if cd_off.checked_add(cd_size).map(|e| e > bytes.len()).unwrap_or(true) {
            return Err(Error::new("ZIP 中央目录越界"));
        }
        let mut entries = Vec::with_capacity(total);
        let mut p = cd_off;
        for _ in 0..total {
            if u32le(bytes, p)? != 0x0201_4b50 {
                return Err(Error::new("ZIP 中央目录条目签名不对"));
            }
            let flags = u16le(bytes, p + 8)?;
            let method = u16le(bytes, p + 10)?;
            let mod_time = u16le(bytes, p + 12)?;
            let mod_date = u16le(bytes, p + 14)?;
            let crc = u32le(bytes, p + 16)?;
            let csize = u32le(bytes, p + 20)?;
            let usize_ = u32le(bytes, p + 24)?;
            let name_len = u16le(bytes, p + 28)? as usize;
            let extra_len = u16le(bytes, p + 30)? as usize;
            let comment_len = u16le(bytes, p + 32)? as usize;
            let local_off = u32le(bytes, p + 42)? as usize;
            let name_bytes = bytes
                .get(p + 46..p + 46 + name_len)
                .ok_or_else(|| Error::new("ZIP 条目名字越界"))?;
            let name = String::from_utf8_lossy(name_bytes).to_string();
            let extra = bytes
                .get(p + 46 + name_len..p + 46 + name_len + extra_len)
                .ok_or_else(|| Error::new("ZIP 条目 extra 越界"))?
                .to_vec();
            let comment = bytes
                .get(p + 46 + name_len + extra_len..p + 46 + name_len + extra_len + comment_len)
                .ok_or_else(|| Error::new("ZIP 条目 comment 越界"))?
                .to_vec();
            p = p + 46 + name_len + extra_len + comment_len;

            // 本地头
            if u32le(bytes, local_off)? != 0x0403_4b50 {
                return Err(Error::new(format!("ZIP 条目 {name} 的本地头签名不对")));
            }
            let l_name_len = u16le(bytes, local_off + 26)? as usize;
            let l_extra_len = u16le(bytes, local_off + 28)? as usize;
            let data_off = local_off
                .checked_add(30 + l_name_len + l_extra_len)
                .ok_or_else(|| Error::new("ZIP 条目偏移溢出"))?;
            let raw = bytes
                .get(data_off..data_off + csize as usize)
                .ok_or_else(|| Error::new(format!("ZIP 条目 {name} 的数据越界")))?
                .to_vec();
            entries.push(ZipEntry {
                name,
                method,
                flags,
                crc,
                csize,
                usize_,
                mod_time,
                mod_date,
                extra,
                comment,
                raw,
                data: None,
            });
        }
        Ok(Zip { entries })
    }

    pub fn find(&self, name: &str) -> Option<&ZipEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    pub fn find_mut(&mut self, name: &str) -> Option<&mut ZipEntry> {
        self.entries.iter_mut().find(|e| e.name == name)
    }

    /// 取条目内容（解压）。
    pub fn read(&mut self, name: &str) -> Result<Option<Vec<u8>>> {
        let entry = match self.entries.iter_mut().find(|e| e.name == name) {
            Some(e) => e,
            None => return Ok(None),
        };
        if entry.data.is_none() {
            let data = match entry.method {
                0 => entry.raw.clone(),
                8 => inflate(&entry.raw, entry.usize_ as usize)?,
                m => return Err(Error::new(format!("不支持的 ZIP 压缩方法 {m}"))),
            };
            if data.len() != entry.usize_ as usize {
                return Err(Error::new(format!(
                    "ZIP 条目 {} 解压后长度不符：期望 {}，实际 {}",
                    entry.name,
                    entry.usize_,
                    data.len()
                )));
            }
            if crc32(&data) != entry.crc {
                return Err(Error::new(format!("ZIP 条目 {} 的 CRC 校验失败", entry.name)));
            }
            entry.data = Some(data);
        }
        Ok(entry.data.clone())
    }

    /// 插入/替换一个 STORED 条目（书签时间用当前时间）。
    pub fn put_stored(&mut self, name: &str, data: &[u8]) {
        let (time, date) = dos_datetime(crate::sys::now_unix());
        let crc = crc32(data);
        let entry = ZipEntry {
            name: name.to_string(),
            method: 0,
            flags: if name.is_ascii() { 0 } else { 0x800 },
            crc,
            csize: data.len() as u32,
            usize_: data.len() as u32,
            mod_time: time,
            mod_date: date,
            extra: Vec::new(),
            comment: Vec::new(),
            raw: data.to_vec(),
            data: Some(data.to_vec()),
        };
        match self.entries.iter_mut().find(|e| e.name == name) {
            Some(slot) => *slot = entry,
            None => self.entries.push(entry),
        }
    }

    /// 整包序列化。
    pub fn write(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for e in &self.entries {
            let offset = out.len();
            if offset > u32::MAX as usize {
                return Err(Error::new("JAR 超过 4GB，未实现 ZIP64"));
            }
            let name = e.name.as_bytes();
            // 重写时始终已知大小，必须清掉"大小在数据描述符里"的 bit 3 ——
            // 否则 JDK 的 ZipFile 会去数据后面找描述符，读到下一个本地头，报 invalid entry size。
            let flags = e.flags & !0x0008;
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&flags.to_le_bytes());
            out.extend_from_slice(&e.method.to_le_bytes());
            out.extend_from_slice(&e.mod_time.to_le_bytes());
            out.extend_from_slice(&e.mod_date.to_le_bytes());
            out.extend_from_slice(&e.crc.to_le_bytes());
            out.extend_from_slice(&e.csize.to_le_bytes());
            out.extend_from_slice(&e.usize_.to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name);
            out.extend_from_slice(&e.raw);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&flags.to_le_bytes());
            central.extend_from_slice(&e.method.to_le_bytes());
            central.extend_from_slice(&e.mod_time.to_le_bytes());
            central.extend_from_slice(&e.mod_date.to_le_bytes());
            central.extend_from_slice(&e.crc.to_le_bytes());
            central.extend_from_slice(&e.csize.to_le_bytes());
            central.extend_from_slice(&e.usize_.to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&(e.extra.len() as u16).to_le_bytes());
            central.extend_from_slice(&(e.comment.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&(offset as u32).to_le_bytes());
            central.extend_from_slice(name);
            central.extend_from_slice(&e.extra);
            central.extend_from_slice(&e.comment);
        }
        let cd_off = out.len();
        if cd_off > u32::MAX as usize || central.len() > u32::MAX as usize {
            return Err(Error::new("JAR 超过 4GB，未实现 ZIP64"));
        }
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        let n = self.entries.len();
        if n > 0xffff {
            return Err(Error::new("条目数超过 65535，未实现 ZIP64"));
        }
        out.extend_from_slice(&(n as u16).to_le_bytes());
        out.extend_from_slice(&(n as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&(cd_off as u32).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        Ok(out)
    }
}

/// DOS 日期时间（ZIP 用本地时间；这里直接用 UTC，不影响校验）。
pub fn dos_datetime(unix: i64) -> (u16, u16) {
    let (y, mo, d, h, mi, s) = crate::sys::civil_from_unix(unix);
    let year = if y < 1980 { 1980 } else { y.min(2107) } as u16;
    let date = ((year - 1980) << 9) | ((mo as u16) << 5) | d as u16;
    let time = ((h as u16) << 11) | ((mi as u16) << 5) | ((s as u16) / 2);
    (time, date)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_known() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b"The quick brown fox jumps over the lazy dog"), 0x414f_a339);
    }

    #[test]
    fn inflate_stored_block() {
        // 手工构造：BFINAL=1, BTYPE=00, 然后对齐 + LEN/NLEN + "abc"
        let block = [0x01u8, 0x03, 0x00, 0xfc, 0xff, b'a', b'b', b'c'];
        assert_eq!(inflate(&block, 10).unwrap(), b"abc");
    }

    #[test]
    fn inflate_rejects_garbage() {
        for seed in 0u32..300 {
            let mut v = Vec::new();
            let mut x = seed.wrapping_mul(2654435761).wrapping_add(7);
            for _ in 0..(seed % 24) {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                v.push(x as u8);
            }
            let _ = inflate(&v, 1 << 20);
        }
    }

    #[test]
    fn zip_roundtrip() {
        let mut z = Zip::default();
        z.put_stored("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\r\n\r\n");
        z.put_stored("a.txt", b"hello");
        let bytes = z.write().unwrap();
        let mut back = Zip::open(&bytes).unwrap();
        assert_eq!(back.entries.len(), 2);
        assert_eq!(back.read("a.txt").unwrap().unwrap(), b"hello");
        assert!(back.read("nope").unwrap().is_none());
    }

    #[test]
    fn zip_rejects_truncated() {
        let mut z = Zip::default();
        z.put_stored("a.txt", b"hello");
        let bytes = z.write().unwrap();
        for cut in [0usize, 1, 10, bytes.len() / 2, bytes.len() - 1] {
            assert!(Zip::open(&bytes[..cut]).is_err(), "截断 {cut} 应报错");
        }
    }
}
