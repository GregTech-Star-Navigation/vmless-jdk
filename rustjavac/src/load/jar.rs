//! `.jar` / `.zip` 来源：自己解析 zip（中央目录 + 本地头），deflate 用本文件的 inflate 解。
//!
//! 不支持（明确报错，不静默）：zip64、加密条目、除 stored(0)/deflate(8) 以外的方法。
//! 大小一律以**中央目录**为准（带 data descriptor 的条目本地头里的 size 是 0）。

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const SIG_EOCD: u32 = 0x0605_4b50;
const SIG_CEN: u32 = 0x0201_4b50;
const SIG_LOC: u32 = 0x0403_4b50;

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

#[derive(Clone, Copy, Debug)]
struct ZipEntry {
    method: u16,
    comp_size: u64,
    uncomp_size: u64,
    local_off: u64,
}

#[derive(Debug)]
pub struct JarSource {
    path: PathBuf,
    file: File,
    entries: HashMap<String, ZipEntry>,
}

impl JarSource {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        let mut file = File::open(&path).map_err(|e| format!("打开 {} 失败: {e}", path.display()))?;
        let len = file
            .metadata()
            .map_err(|e| format!("取 {} 大小失败: {e}", path.display()))?
            .len();
        // EOCD 在文件尾，最多带 64KB 注释
        let tail_len = len.min(66_000 + 22) as usize;
        let mut tail = vec![0u8; tail_len];
        file.seek(SeekFrom::Start(len - tail_len as u64))
            .map_err(|e| format!("定位 {} 失败: {e}", path.display()))?;
        file.read_exact(&mut tail)
            .map_err(|e| format!("读 {} 失败: {e}", path.display()))?;
        let mut eocd = None;
        let mut i = tail_len;
        while i >= 22 {
            i -= 1;
            if i + 4 <= tail_len && u32le(&tail, i) == SIG_EOCD {
                eocd = Some(i);
                break;
            }
        }
        let Some(e) = eocd else {
            return Err(format!("{} 里找不到 zip 中央目录结尾记录（不是 zip/jar？）", path.display()));
        };
        let total = u16le(&tail, e + 10) as u64;
        let cd_size = u32le(&tail, e + 12) as u64;
        let cd_off = u32le(&tail, e + 16) as u64;
        if total == 0xFFFF || cd_off == 0xFFFF_FFFF || cd_size == 0xFFFF_FFFF {
            return Err(format!("{} 是 zip64，本实现不支持", path.display()));
        }
        let mut cd = vec![0u8; cd_size as usize];
        file.seek(SeekFrom::Start(cd_off))
            .map_err(|e| format!("定位中央目录失败: {e}"))?;
        file.read_exact(&mut cd)
            .map_err(|e| format!("读中央目录失败: {e}"))?;

        let mut entries = HashMap::with_capacity(total as usize);
        let mut p = 0usize;
        while p + 46 <= cd.len() {
            if u32le(&cd, p) != SIG_CEN {
                break;
            }
            let method = u16le(&cd, p + 10);
            let comp_size = u32le(&cd, p + 20) as u64;
            let uncomp_size = u32le(&cd, p + 24) as u64;
            let name_len = u16le(&cd, p + 28) as usize;
            let extra_len = u16le(&cd, p + 30) as usize;
            let comment_len = u16le(&cd, p + 32) as usize;
            let local_off = u32le(&cd, p + 42) as u64;
            let name_end = p + 46 + name_len;
            if name_end > cd.len() {
                break;
            }
            let name = String::from_utf8_lossy(&cd[p + 46..name_end]).into_owned();
            if !name.ends_with('/') {
                entries.insert(name, ZipEntry { method, comp_size, uncomp_size, local_off });
            }
            p = name_end + extra_len + comment_len;
        }
        Ok(JarSource { path, file, entries })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// 所有条目名（诊断/测试用；`ClassPath` 按需查找不依赖它）
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.entries.keys().cloned().collect();
        v.sort();
        v
    }

    /// 条目路径 → 字节
    pub fn find(&self, entry: &str) -> Result<Option<Vec<u8>>, String> {
        let Some(e) = self.entries.get(entry).copied() else {
            return Ok(None);
        };
        let mut f = &self.file;
        // 本地头里的 name/extra 长度决定数据起点（中央目录里的 extra 是另一种）
        let mut loc = [0u8; 30];
        f.seek(SeekFrom::Start(e.local_off))
            .map_err(|e| format!("定位 {entry} 的本地头失败: {e}"))?;
        f.read_exact(&mut loc)
            .map_err(|e| format!("读 {entry} 的本地头失败: {e}"))?;
        if u32le(&loc, 0) != SIG_LOC {
            return Err(format!("{entry} 的本地头签名不对"));
        }
        let data_off =
            e.local_off + 30 + u16le(&loc, 26) as u64 + u16le(&loc, 28) as u64;
        f.seek(SeekFrom::Start(data_off))
            .map_err(|e| format!("定位 {entry} 的数据失败: {e}"))?;
        let mut raw = vec![0u8; e.comp_size as usize];
        f.read_exact(&mut raw)
            .map_err(|err| format!("读 {entry}（{} 字节）失败: {err}", e.comp_size))?;
        match e.method {
            0 => Ok(Some(raw)),
            8 => {
                let out = inflate(&raw)?;
                if e.uncomp_size != 0 && out.len() as u64 != e.uncomp_size {
                    return Err(format!(
                        "{entry} 解压后 {} 字节，中央目录说 {} 字节",
                        out.len(),
                        e.uncomp_size
                    ));
                }
                Ok(Some(out))
            }
            m => Err(format!("{entry} 用了不支持的压缩方法 {m}（只认 stored/deflate）")),
        }
    }

    /// `META-INF/MANIFEST.MF` 里的 `Main-Class`（续行按规范拼接）
    pub fn main_class(&self) -> Result<Option<String>, String> {
        let Some(bytes) = self.find("META-INF/MANIFEST.MF")? else {
            return Ok(None);
        };
        let text = String::from_utf8_lossy(&bytes);
        let mut lines: Vec<String> = Vec::new();
        for line in text.split("\r\n").flat_map(|l| l.split('\n')) {
            if let Some(cont) = line.strip_prefix(' ') {
                // 续行：接在上一行后面
                if let Some(last) = lines.last_mut() {
                    last.push_str(cont);
                    continue;
                }
            }
            lines.push(line.to_string());
        }
        for l in lines {
            if let Some((k, v)) = l.split_once(':') {
                if k.eq_ignore_ascii_case("Main-Class") {
                    let v = v.trim().to_string();
                    if !v.is_empty() {
                        return Ok(Some(v));
                    }
                }
            }
        }
        Ok(None)
    }
}

// ------------------------------------------------------------------ inflate

/// DEFLATE 解压（RFC 1951）：stored / 固定 Huffman / 动态 Huffman。
/// 逐位解码，类文件这个体量（几十 KB）够快，也不依赖任何 crate。
pub fn inflate(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut br = Bits { d: data, p: 0, buf: 0, cnt: 0 };
    let mut out: Vec<u8> = Vec::with_capacity(data.len() * 4);
    loop {
        let last = br.bits(1)?;
        let kind = br.bits(2)?;
        match kind {
            0 => {
                br.align();
                let len = br.byte()? as usize | ((br.byte()? as usize) << 8);
                let nlen = br.byte()? as usize | ((br.byte()? as usize) << 8);
                if len != (!nlen & 0xFFFF) {
                    return Err("stored 块的 LEN/NLEN 对不上".into());
                }
                for _ in 0..len {
                    let b = br.byte()?;
                    out.push(b);
                }
            }
            1 | 2 => {
                let (lit, dist) = if kind == 1 {
                    (Huff::fixed_lit(), Huff::fixed_dist())
                } else {
                    dynamic_tables(&mut br)?
                };
                loop {
                    let sym = lit.decode(&mut br)? as usize;
                    match sym {
                        0..=255 => out.push(sym as u8),
                        256 => break,
                        _ => {
                            let li = sym - 257;
                            if li >= LEN_BASE.len() {
                                return Err(format!("非法的长度码 {sym}"));
                            }
                            let len = LEN_BASE[li] + br.bits(LEN_EXTRA[li] as u32)? as usize;
                            let ds = dist.decode(&mut br)? as usize;
                            if ds >= DIST_BASE.len() {
                                return Err(format!("非法的距离码 {ds}"));
                            }
                            let d = DIST_BASE[ds] + br.bits(DIST_EXTRA[ds] as u32)? as usize;
                            if d > out.len() || d == 0 {
                                return Err("deflate 回溯距离越界".into());
                            }
                            let start = out.len() - d;
                            for i in 0..len {
                                let b = out[start + i];
                                out.push(b);
                            }
                        }
                    }
                }
            }
            _ => return Err("非法的 deflate 块类型 3".into()),
        }
        if last == 1 {
            break;
        }
    }
    Ok(out)
}

const LEN_BASE: [usize; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
    131, 163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [usize; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
    13, 13,
];

struct Bits<'a> {
    d: &'a [u8],
    p: usize,
    buf: u32,
    cnt: u32,
}

impl Bits<'_> {
    fn bit(&mut self) -> Result<u32, String> {
        if self.cnt == 0 {
            let Some(&b) = self.d.get(self.p) else {
                return Err("deflate 数据提前结束".into());
            };
            self.p += 1;
            self.buf = b as u32;
            self.cnt = 8;
        }
        let v = self.buf & 1;
        self.buf >>= 1;
        self.cnt -= 1;
        Ok(v)
    }
    fn bits(&mut self, n: u32) -> Result<u32, String> {
        let mut v = 0;
        for i in 0..n {
            v |= self.bit()? << i;
        }
        Ok(v)
    }
    fn byte(&mut self) -> Result<u8, String> {
        let v = self.bits(8)?;
        Ok(v as u8)
    }
    fn align(&mut self) {
        self.cnt = 0;
    }
}

/// 规范 Huffman 表：`counts[len]` 个长度为 len 的码，`syms` 按码序排好的符号
struct Huff {
    counts: [u16; 16],
    syms: Vec<u16>,
}

impl Huff {
    fn new(lengths: &[u8]) -> Result<Self, String> {
        let mut counts = [0u16; 16];
        for &l in lengths {
            if l as usize > 15 {
                return Err("Huffman 码长超过 15".into());
            }
            counts[l as usize] += 1;
        }
        counts[0] = 0;
        let mut left: i32 = 1;
        for len in 1..16 {
            left <<= 1;
            left -= counts[len] as i32;
            if left < 0 {
                return Err("Huffman 表超额（非法码长分布）".into());
            }
        }
        let mut offs = [0u16; 16];
        for len in 1..15 {
            offs[len + 1] = offs[len] + counts[len];
        }
        let mut syms = vec![0u16; lengths.len()];
        for (s, &l) in lengths.iter().enumerate() {
            if l != 0 {
                syms[offs[l as usize] as usize] = s as u16;
                offs[l as usize] += 1;
            }
        }
        Ok(Huff { counts, syms })
    }

    fn from_lengths(f: impl Fn(usize) -> u8, n: usize) -> Self {
        let lens: Vec<u8> = (0..n).map(f).collect();
        Huff::new(&lens).expect("固定 Huffman 表一定合法")
    }

    fn fixed_lit() -> Self {
        Huff::from_lengths(|i| if i < 144 { 8 } else if i < 256 { 9 } else if i < 280 { 7 } else { 8 }, 288)
    }
    fn fixed_dist() -> Self {
        Huff::from_lengths(|_| 5, 32)
    }

    fn decode(&self, br: &mut Bits) -> Result<u16, String> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..16 {
            code |= br.bit()? as i32;
            let count = self.counts[len] as i32;
            if code - first < count {
                let i = (index + (code - first)) as usize;
                return self
                    .syms
                    .get(i)
                    .copied()
                    .ok_or_else(|| "Huffman 解码下标越界".to_string());
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err("Huffman 解码失败（码不存在）".into())
    }
}

const CLEN_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

fn dynamic_tables(br: &mut Bits) -> Result<(Huff, Huff), String> {
    let hlit = br.bits(5)? as usize + 257;
    let hdist = br.bits(5)? as usize + 1;
    let hclen = br.bits(4)? as usize + 4;
    let mut clen = [0u8; 19];
    for i in 0..hclen {
        clen[CLEN_ORDER[i]] = br.bits(3)? as u8;
    }
    let clh = Huff::new(&clen)?;
    let mut lens = vec![0u8; hlit + hdist];
    let mut i = 0;
    while i < lens.len() {
        let sym = clh.decode(br)?;
        match sym {
            0..=15 => {
                lens[i] = sym as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err("code-length 16 出现在开头".into());
                }
                let prev = lens[i - 1];
                let n = 3 + br.bits(2)? as usize;
                for _ in 0..n {
                    if i >= lens.len() {
                        return Err("code-length 重复越界".into());
                    }
                    lens[i] = prev;
                    i += 1;
                }
            }
            17 => {
                let n = 3 + br.bits(3)? as usize;
                i = i.saturating_add(n);
            }
            18 => {
                let n = 11 + br.bits(7)? as usize;
                i = i.saturating_add(n);
            }
            _ => return Err(format!("非法的 code-length 符号 {sym}")),
        }
    }
    let lit = Huff::new(&lens[..hlit])?;
    let dist = Huff::new(&lens[hlit..])?;
    Ok((lit, dist))
}
