//! zip（jar / jmod）读取与写入：**stored 与 deflate 两种方法都支持**。
//!
//! 读：自己解中央目录（`jmod` 的 `JMOD` 8 字节前缀要按相对偏移修正）；
//!     deflate 解压复用 `classfile::load::jar::inflate`（那边已实现、且被 jar 路径验证过）。
//! 写：不用 zlib —— LZ77（哈希链，贪心匹配）+ **固定 Huffman** 的 deflate 编码器，
//!     输出是合法的 RFC 1951 流，真 JDK 的 `ZipFile`/`jmod list` 能直接读。

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

/// jmod 文件头：4 字节 `J M 01 00`（"JM" + 主版本 1 + 次版本 0），之后才是 zip
/// jmod 文件头：4 字节 `J M 01 00`（"JM" + 主版本 1 + 次版本 0），之后才是 zip。
pub const JMOD_MAGIC: [u8; 4] = [0x4A, 0x4D, 0x01, 0x00];
pub const JMOD_HEADER_LEN: usize = 4;

// ---------------------------------------------------------------- CRC32

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC_TABLE: [u32; 256] = crc_table();

pub fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for b in data {
        c = CRC_TABLE[((c ^ *b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

// ---------------------------------------------------------------- 读

#[derive(Clone, Debug)]
pub struct ZipEntry {
    pub name: String,
    pub method: u16,
    pub crc: u32,
    pub csize: u64,
    pub usize_: u64,
    /// 本地头在**文件绝对偏移**上的位置
    pub local_off: u64,
    pub time: u16,
    pub date: u16,
}

pub struct ZipReader {
    pub path: PathBuf,
    data: Vec<u8>,
    pub entries: Vec<ZipEntry>,
    /// zip 数据区在文件里的起点（普通 jar = 0；jmod = 8）
    pub data_start: usize,
}

fn rd16(b: &[u8], o: usize) -> Option<u16> {
    if o + 2 > b.len() {
        return None;
    }
    Some(u16::from_le_bytes([b[o], b[o + 1]]))
}
fn rd32(b: &[u8], o: usize) -> Option<u32> {
    if o + 4 > b.len() {
        return None;
    }
    Some(u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]))
}
fn rd64(b: &[u8], o: usize) -> Option<u64> {
    if o + 8 > b.len() {
        return None;
    }
    let mut v = 0u64;
    for i in (0..8).rev() {
        v = (v << 8) | b[o + i] as u64;
    }
    Some(v)
}

impl ZipReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        let mut f = std::fs::File::open(&path).map_err(|e| format!("打开 {} 失败: {e}", path.display()))?;
        let len = f
            .metadata()
            .map_err(|e| format!("读 {} 的文件信息失败: {e}", path.display()))?
            .len();
        if len < 22 {
            return Err(format!("{} 太小（{len} 字节），不是 zip/jmod", path.display()));
        }
        if len > 2 * 1024 * 1024 * 1024 {
            return Err(format!("{} 超过 2GB（{len} 字节），本实现不处理 zip64 大文件", path.display()));
        }
        let mut data = Vec::with_capacity(len as usize);
        f.read_to_end(&mut data)
            .map_err(|e| format!("读 {} 失败: {e}", path.display()))?;

        // jmod：前 8 字节是 JMOD + 版本，之后才是 zip（内部偏移相对 zip 起点）
        let mut data_start = 0usize;
        if data.len() >= 4 && data[0] == JMOD_MAGIC[0] && data[1] == JMOD_MAGIC[1] {
            data_start = JMOD_HEADER_LEN;
            if data.len() < data_start + 22 {
                return Err(format!("{} 有 JMOD 头但后面没有 zip 数据", path.display()));
            }
        }

        // EOCD：从尾部往前找（注释最长 65535）
        let tail_start = data.len().saturating_sub(65535 + 22);
        let mut eocd = None;
        let mut i = data.len() - 22;
        loop {
            if rd32(&data, i) == Some(0x0605_4B50) {
                eocd = Some(i);
                break;
            }
            if i == tail_start {
                break;
            }
            i -= 1;
        }
        let eocd = match eocd {
            Some(v) => v,
            None => {
                return Err(format!(
                    "{} 里找不到 zip 的 End-Of-Central-Directory 记录（不是 zip/jar/jmod，或文件被截断）",
                    path.display()
                ))
            }
        };
        let count = rd16(&data, eocd + 10).unwrap_or(0) as usize;
        let cd_size = rd32(&data, eocd + 12).unwrap_or(0) as usize;
        let cd_off_raw = rd32(&data, eocd + 16).unwrap_or(0) as usize;
        if count == 0xFFFF || cd_off_raw == 0xFFFF_FFFF {
            return Err(format!("{} 是 zip64，本实现不支持", path.display()));
        }
        // 中央目录偏移可能是「相对 zip 起点」或「文件绝对偏移」，用签名试出来
        let mut cd_off = None;
        for cand in [data_start + cd_off_raw, cd_off_raw] {
            if rd32(&data, cand) == Some(0x0201_4B50) {
                cd_off = Some(cand);
                break;
            }
        }
        let cd_off = match cd_off {
            Some(v) => v,
            None => {
                return Err(format!(
                    "{} 的中央目录偏移 {cd_off_raw} 处没有目录签名（文件损坏）",
                    path.display()
                ))
            }
        };
        if cd_off + cd_size > data.len() {
            return Err(format!("{} 的中央目录区超出文件末尾（被截断）", path.display()));
        }

        let mut entries = Vec::with_capacity(count);
        let mut p = cd_off;
        for k in 0..count {
            if rd32(&data, p) != Some(0x0201_4B50) {
                return Err(format!("第 {k} 个中央目录项签名错（偏移 {p}）"));
            }
            let method = rd16(&data, p + 10).unwrap_or(0);
            let time = rd16(&data, p + 12).unwrap_or(0);
            let date = rd16(&data, p + 14).unwrap_or(0);
            let crc = rd32(&data, p + 16).unwrap_or(0);
            let csize = rd32(&data, p + 20).unwrap_or(0) as u64;
            let usize_ = rd32(&data, p + 24).unwrap_or(0) as u64;
            let name_len = rd16(&data, p + 28).unwrap_or(0) as usize;
            let extra_len = rd16(&data, p + 30).unwrap_or(0) as usize;
            let comment_len = rd16(&data, p + 32).unwrap_or(0) as usize;
            let lho = rd32(&data, p + 42).unwrap_or(0) as usize;
            let name_at = p + 46;
            if name_at + name_len > data.len() {
                return Err(format!("第 {k} 个目录项的名字越界"));
            }
            let name = String::from_utf8_lossy(&data[name_at..name_at + name_len]).into_owned();
            // 本地头偏移同理：相对 zip 起点或绝对
            let mut local_off = None;
            for cand in [data_start + lho, lho] {
                if rd32(&data, cand) == Some(0x0403_4B50) {
                    local_off = Some(cand);
                    break;
                }
            }
            let local_off = match local_off {
                Some(v) => v,
                None => {
                    return Err(format!(
                        "条目 {name} 的本地头偏移 {lho} 处没有本地头签名（文件损坏）"
                    ))
                }
            };
            entries.push(ZipEntry {
                name,
                method,
                crc,
                csize,
                usize_,
                local_off: local_off as u64,
                time,
                date,
            });
            p = name_at + name_len + extra_len + comment_len;
        }
        Ok(ZipReader {
            path,
            data,
            entries,
            data_start,
        })
    }

    pub fn names(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.name.clone()).collect()
    }

    pub fn get(&self, name: &str) -> Option<&ZipEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// 读一个条目的解压后内容
    pub fn read(&self, e: &ZipEntry) -> Result<Vec<u8>, String> {
        let lo = e.local_off as usize;
        if rd32(&self.data, lo) != Some(0x0403_4B50) {
            return Err(format!("条目 {} 的本地头签名不对", e.name));
        }
        let name_len = rd16(&self.data, lo + 26).unwrap_or(0) as usize;
        let extra_len = rd16(&self.data, lo + 28).unwrap_or(0) as usize;
        let start = lo + 30 + name_len + extra_len;
        let end = start
            .checked_add(e.csize as usize)
            .ok_or_else(|| format!("条目 {} 的长度溢出", e.name))?;
        if end > self.data.len() {
            return Err(format!(
                "条目 {} 的数据区间 [{start}, {end}) 超出文件大小 {}",
                e.name,
                self.data.len()
            ));
        }
        let raw = &self.data[start..end];
        let out = match e.method {
            0 => raw.to_vec(),
            8 => classfile::load::jar::inflate(raw)
                .map_err(|m| format!("条目 {} 解压失败: {m}", e.name))?,
            m => return Err(format!("条目 {} 用了不支持的压缩方法 {m}", e.name)),
        };
        if out.len() as u64 != e.usize_ {
            return Err(format!(
                "条目 {} 解压后 {} 字节，与目录里记录的 {} 字节不符",
                e.name,
                out.len(),
                e.usize_
            ));
        }
        let c = crc32(&out);
        if c != e.crc {
            return Err(format!(
                "条目 {} 的 CRC 校验失败（算出 {c:08x}，目录记录 {:08x}）",
                e.name, e.crc
            ));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------- deflate 编码器

struct BitWriter {
    out: Vec<u8>,
    bit: u32,
    nbit: u32,
}

impl BitWriter {
    fn new() -> Self {
        BitWriter { out: Vec::new(), bit: 0, nbit: 0 }
    }
    /// 按 LSB 先出写 n 位（deflate 的「额外位」与头字段就是这样）
    fn put(&mut self, val: u32, n: u32) {
        self.bit |= val << self.nbit;
        self.nbit += n;
        while self.nbit >= 8 {
            self.out.push((self.bit & 0xFF) as u8);
            self.bit >>= 8;
            self.nbit -= 8;
        }
    }
    /// Huffman 码要按 MSB 先出（= 把码位反过来用 LSB 写）
    fn put_code(&mut self, code: u32, n: u32) {
        let mut r = 0u32;
        for i in 0..n {
            r = (r << 1) | ((code >> i) & 1);
        }
        self.put(r, n);
    }
    fn finish(mut self) -> Vec<u8> {
        if self.nbit > 0 {
            self.out.push((self.bit & 0xFF) as u8);
        }
        self.out
    }
}

fn put_literal(bw: &mut BitWriter, lit: u32) {
    match lit {
        0..=143 => bw.put_code(0x30 + lit, 8),
        144..=255 => bw.put_code(0x190 + (lit - 144), 9),
        // 长度码 257..279 是 7 位、码值 = lit-256（这一支漏过就会被当成字面量写坏整个流）
        256..=279 => bw.put_code(lit - 256, 7),
        _ => bw.put_code(0xC0 + (lit - 280), 8),
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

fn put_length(bw: &mut BitWriter, len: u16) {
    let mut idx = 0usize;
    for (i, base) in LEN_BASE.iter().enumerate() {
        if len >= *base {
            idx = i;
        }
    }
    // 258 有专用码 285（0 额外位）
    if len == 258 {
        idx = 28;
    }
    let code = 257 + idx as u32;
    put_literal(bw, code);
    if LEN_EXTRA[idx] > 0 {
        bw.put((len - LEN_BASE[idx]) as u32, LEN_EXTRA[idx] as u32);
    }
}

fn put_distance(bw: &mut BitWriter, dist: u16) {
    let mut idx = 0usize;
    for (i, base) in DIST_BASE.iter().enumerate() {
        if dist >= *base {
            idx = i;
        }
    }
    bw.put_code(idx as u32, 5);
    if DIST_EXTRA[idx] > 0 {
        bw.put((dist - DIST_BASE[idx]) as u32, DIST_EXTRA[idx] as u32);
    }
}

/// zlib 兼容的 raw deflate（固定 Huffman + LZ77）。
/// 压缩率不如 zlib（大约 1.6~2.2:1 vs 2.5~3:1），但**输出完全合法**。
pub fn deflate(data: &[u8]) -> Vec<u8> {
    const WINDOW: usize = 32768;
    const MIN_MATCH: usize = 3;
    const MAX_MATCH: usize = 258;
    const HASH_BITS: usize = 15;
    const HASH_SIZE: usize = 1 << HASH_BITS;

    let mut bw = BitWriter::new();
    bw.put(1, 1); // BFINAL
    bw.put(1, 2); // BTYPE = 01 固定 Huffman

    let n = data.len();
    let mut head = vec![u32::MAX; HASH_SIZE];
    let mut prev = vec![u32::MAX; n.max(1)];
    let hash = |d: &[u8], i: usize| -> usize {
        let v = (d[i] as u32) << 16 | (d[i + 1] as u32) << 8 | d[i + 2] as u32;
        ((v.wrapping_mul(0x9E37_79B1)) >> (32 - HASH_BITS)) as usize & (HASH_SIZE - 1)
    };

    let mut i = 0usize;
    while i < n {
        let mut best_len = 0usize;
        let mut best_dist = 0usize;
        if i + MIN_MATCH <= n {
            let h = hash(data, i);
            let mut cand = head[h];
            let mut chain = 0;
            while cand != u32::MAX && chain < 64 {
                let c = cand as usize;
                let dist = i - c;
                if dist > WINDOW {
                    break;
                }
                // 先比最长可能匹配的最后一个字节，失败就跳过
                let maxl = MAX_MATCH.min(n - i);
                if data[c + best_len.min(maxl - 1)] == data[i + best_len.min(maxl - 1)] {
                    let mut l = 0;
                    while l < maxl && data[c + l] == data[i + l] {
                        l += 1;
                    }
                    if l > best_len {
                        best_len = l;
                        best_dist = dist;
                        if l >= MAX_MATCH {
                            break;
                        }
                    }
                }
                cand = prev[c];
                chain += 1;
            }
            prev[i] = head[h];
            head[h] = i as u32;
        }
        if best_len >= MIN_MATCH {
            put_length(&mut bw, best_len as u16);
            put_distance(&mut bw, best_dist as u16);
            // 把匹配区间内的位置也塞进哈希表（否则后面的匹配质量差很多）
            for k in (i + 1)..(i + best_len) {
                if k + MIN_MATCH <= n {
                    let h = hash(data, k);
                    prev[k] = head[h];
                    head[h] = k as u32;
                }
            }
            i += best_len;
        } else {
            put_literal(&mut bw, data[i] as u32);
            i += 1;
        }
    }
    put_literal(&mut bw, 256); // 块结束
    bw.finish()
}

// ---------------------------------------------------------------- 写

struct CdRec {
    name: String,
    method: u16,
    crc: u32,
    csize: u32,
    usize_: u32,
    offset: u32,
    time: u16,
    date: u16,
}

pub struct ZipWriter {
    out: Vec<u8>,
    cd: Vec<CdRec>,
    /// 0 = stored，6 = deflate（zip-6 用 deflate 默认级别；我们只有"压/不压"两档）
    pub method: u16,
    pub time: u16,
    pub date: u16,
}

impl ZipWriter {
    /// `compress`：`zip-0`/`none` → stored；`zip-1..zip-9` → deflate（固定 Huffman）
    pub fn new(compress: &str) -> Result<Self, String> {
        let (method, level_note) = match compress.trim() {
            "" | "zip-0" | "stored" | "none" => (0u16, 0u8),
            "zip-1" | "zip-2" | "zip-3" | "zip-4" | "zip-5" | "zip-6" | "zip-7" | "zip-8" | "zip-9" => {
                (8u16, 1u8)
            }
            other => {
                return Err(format!(
                    "不支持的压缩参数 {other}（可用 zip-0..zip-9；zip-0 = 不压缩，其余都映射到同一个 deflate 编码器）"
                ))
            }
        };
        let _ = level_note;
        let (time, date) = dos_now();
        Ok(ZipWriter {
            out: Vec::new(),
            cd: Vec::new(),
            method,
            time,
            date,
        })
    }

    /// 固定时间戳（`--date`）；非法格式报错而不是静默用当前时间
    pub fn set_date(&mut self, date: &str) -> Result<(), String> {
        // 接受 `YYYY-MM-DDTHH:MM:SS` 或 `YYYY-MM-DDTHH:MM:SS±HH:MM`
        let s = date.trim();
        let year: u16 = s
            .get(0..4)
            .and_then(|x| x.parse().ok())
            .ok_or_else(|| format!("--date 的年份不合法：{date}（要 ISO-8601，如 2022-02-12T12:30:00）"))?;
        let mon: u16 = s
            .get(5..7)
            .and_then(|x| x.parse().ok())
            .ok_or_else(|| format!("--date 的月份不合法：{date}"))?;
        let day: u16 = s
            .get(8..10)
            .and_then(|x| x.parse().ok())
            .ok_or_else(|| format!("--date 的日期不合法：{date}"))?;
        let hh: u16 = s
            .get(11..13)
            .and_then(|x| x.parse().ok())
            .ok_or_else(|| format!("--date 的小时不合法：{date}"))?;
        let mm: u16 = s
            .get(14..16)
            .and_then(|x| x.parse().ok())
            .ok_or_else(|| format!("--date 的分钟不合法：{date}"))?;
        let ss: u16 = s
            .get(17..19)
            .and_then(|x| x.parse().ok())
            .ok_or_else(|| format!("--date 的秒不合法：{date}"))?;
        if !(1980..=2107).contains(&year) || !(1..=12).contains(&mon) || !(1..=31).contains(&day) {
            return Err(format!("--date 的日期超出 zip 能表示的范围（1980..2107）：{date}"));
        }
        self.time = (hh << 11) | (mm << 5) | (ss / 2);
        self.date = ((year - 1980) << 9) | (mon << 5) | day;
        Ok(())
    }

    /// 追加一个条目（名字用 `/` 分隔、不带前导 `/`）
    pub fn add(&mut self, name: &str, data: &[u8]) -> Result<(), String> {
        if name.is_empty() {
            return Err("zip 条目名不能为空".into());
        }
        if data.len() as u64 > 0xFFFF_FFFE {
            return Err(format!("条目 {name} 超过 4GB，本实现不写 zip64"));
        }
        let payload = if self.method == 8 {
            let d = deflate(data);
            // 压不动（>= 原大小）就退回 stored，和 JDK 的 ZipOutputStream 行为一致
            if d.len() < data.len() {
                d
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        let (method, body): (u16, &[u8]) = if payload.is_empty() {
            (0, data)
        } else {
            (8, &payload)
        };
        let crc = crc32(data);
        let offset = self.out.len() as u32;
        let name_bytes = name.as_bytes();
        self.out.extend_from_slice(&0x0403_4B50u32.to_le_bytes());
        self.out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        self.out.extend_from_slice(&0u16.to_le_bytes()); // flags
        self.out.extend_from_slice(&method.to_le_bytes());
        self.out.extend_from_slice(&self.time.to_le_bytes());
        self.out.extend_from_slice(&self.date.to_le_bytes());
        self.out.extend_from_slice(&crc.to_le_bytes());
        self.out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        self.out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        self.out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        self.out.extend_from_slice(name_bytes);
        self.out.extend_from_slice(body);
        self.cd.push(CdRec {
            name: name.to_string(),
            method,
            crc,
            csize: body.len() as u32,
            usize_: data.len() as u32,
            offset,
            time: self.time,
            date: self.date,
        });
        Ok(())
    }

    /// 收尾：写中央目录 + EOCD。返回完整 zip 字节（不带 jmod 前缀）。
    pub fn finish(&mut self) -> Result<Vec<u8>, String> {
        let cd_start = self.out.len() as u32;
        let recs: Vec<CdRec> = self
            .cd
            .iter()
            .map(|r| CdRec {
                name: r.name.clone(),
                method: r.method,
                crc: r.crc,
                csize: r.csize,
                usize_: r.usize_,
                offset: r.offset,
                time: r.time,
                date: r.date,
            })
            .collect();
        for r in &recs {
            let nb = r.name.as_bytes();
            self.out.extend_from_slice(&0x0201_4B50u32.to_le_bytes());
            self.out.extend_from_slice(&20u16.to_le_bytes()); // version made by
            self.out.extend_from_slice(&20u16.to_le_bytes()); // version needed
            self.out.extend_from_slice(&0u16.to_le_bytes()); // flags
            self.out.extend_from_slice(&r.method.to_le_bytes());
            self.out.extend_from_slice(&r.time.to_le_bytes());
            self.out.extend_from_slice(&r.date.to_le_bytes());
            self.out.extend_from_slice(&r.crc.to_le_bytes());
            self.out.extend_from_slice(&r.csize.to_le_bytes());
            self.out.extend_from_slice(&r.usize_.to_le_bytes());
            self.out.extend_from_slice(&(nb.len() as u16).to_le_bytes());
            self.out.extend_from_slice(&0u16.to_le_bytes()); // extra
            self.out.extend_from_slice(&0u16.to_le_bytes()); // comment
            self.out.extend_from_slice(&0u16.to_le_bytes()); // disk
            self.out.extend_from_slice(&0u16.to_le_bytes()); // internal attr
            self.out.extend_from_slice(&0u32.to_le_bytes()); // external attr
            self.out.extend_from_slice(&r.offset.to_le_bytes());
            self.out.extend_from_slice(nb);
        }
        let cd_size = self.out.len() as u32 - cd_start;
        if recs.len() > 0xFFFF {
            return Err("条目数超过 65535，本实现不写 zip64".into());
        }
        self.out.extend_from_slice(&0x0605_4B50u32.to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes());
        self.out.extend_from_slice(&(recs.len() as u16).to_le_bytes());
        self.out.extend_from_slice(&(recs.len() as u16).to_le_bytes());
        self.out.extend_from_slice(&cd_size.to_le_bytes());
        self.out.extend_from_slice(&cd_start.to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes());
        Ok(std::mem::take(&mut self.out))
    }

    /// 加 jmod 的 4 字节头（`4A 4D 01 00` = "JM" + 版本 1.0），返回可直接落盘的完整 jmod。
    /// 注意：zip 的内部偏移是**相对 zip 起点**的，所以读的时候要整体加 `JMOD_HEADER_LEN`
    /// （见 `ZipReader::open` 里的候选偏移试探）。
    pub fn wrap_jmod(zip: Vec<u8>) -> Vec<u8> {
        let mut out = Vec::with_capacity(zip.len() + JMOD_HEADER_LEN);
        out.extend_from_slice(&JMOD_MAGIC);
        out.extend_from_slice(&zip);
        out
    }
}

fn dos_now() -> (u16, u16) {
    // 不用外部依赖拿时间：取系统时间的近似（从 UNIX_EPOCH 起按 UTC 算），
    // 只是给 zip 条目的时间戳一个稳定值，不参与任何判定。
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86400;
    let tod = secs % 86400;
    let (hh, mm, ss) = ((tod / 3600) as u16, ((tod % 3600) / 60) as u16, (tod % 60) as u16);
    // 民用历法换算（Howard Hinnant 的 days_from_civil 逆过程）
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    let y16 = year.clamp(1980, 2107) as u16;
    (
        (hh << 11) | (mm << 5) | (ss / 2),
        ((y16 - 1980) << 9) | ((m as u16) << 5) | d as u16,
    )
}

/// 写 jmod：先占 8 字节头，再写 zip（内部偏移从 0 起算，与 JDK 的 jmod 一致）
pub struct JmodBuilder {
    pub zip: ZipWriter,
}

impl JmodBuilder {
    pub fn new(compress: &str) -> Result<Self, String> {
        Ok(JmodBuilder { zip: ZipWriter::new(compress)? })
    }
    pub fn add_file(&mut self, entry: &str, data: &[u8]) -> Result<(), String> {
        self.zip.add(entry, data)
    }
    pub fn finish(self) -> Result<Vec<u8>, String> {
        let mut zip = self.zip;
        let body = zip.finish()?;
        Ok(ZipWriter::wrap_jmod(body))
    }
}

/// 读一个「名字 → 内容」的映射当条目源（测试与 `jmod create --class-path` 用）
pub fn files_under(
    root: &Path,
    skip_hidden_dirs: bool,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    let files = crate::walk_files(root, skip_hidden_dirs)?;
    let mut out = Vec::with_capacity(files.len());
    for f in files {
        let rel = f
            .strip_prefix(root)
            .map_err(|e| format!("算相对路径失败: {e}"))?
            .to_string_lossy()
            .replace('\\', "/");
        let data = std::fs::read(&f).map_err(|e| format!("读 {} 失败: {e}", f.display()))?;
        out.push((rel, data));
    }
    Ok(out)
}

/// 把小写扩展名映射到 MIME（`jwebserver` 用）
pub fn mime_of(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        "txt" | "md" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "zip" | "jar" | "jmod" => "application/zip",
        "class" => "application/java-vm",
        "woff2" => "font/woff2",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_known() {
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn deflate_roundtrip_via_inflate() {
        // 用我们自己的 inflate（classfile 那份）回读，证明流合法
        for case in [
            b"".to_vec(),
            b"a".to_vec(),
            b"hello hello hello hello hello".to_vec(),
            (0..5000u32).map(|i| (i % 251) as u8).collect::<Vec<u8>>(),
        ] {
            let d = deflate(&case);
            let back = classfile::load::jar::inflate(&d).expect("inflate 失败");
            assert_eq!(back, case);
        }
    }

    #[test]
    fn zip_roundtrip() {
        let mut w = ZipWriter::new("zip-6").unwrap();
        w.add("a.txt", b"hello world").unwrap();
        w.add("dir/b.bin", &[0u8; 1000]).unwrap();
        let bytes = w.finish().unwrap();
        let tmp = std::env::temp_dir().join("vmless_zip_test.zip");
        std::fs::write(&tmp, &bytes).unwrap();
        let r = ZipReader::open(&tmp).unwrap();
        assert_eq!(r.names(), vec!["a.txt".to_string(), "dir/b.bin".to_string()]);
        assert_eq!(r.read(r.get("a.txt").unwrap()).unwrap(), b"hello world");
        assert_eq!(r.read(r.get("dir/b.bin").unwrap()).unwrap().len(), 1000);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn mime_map() {
        assert_eq!(mime_of("index.html"), "text/html; charset=utf-8");
        assert_eq!(mime_of("a.bin"), "application/octet-stream");
    }
}
