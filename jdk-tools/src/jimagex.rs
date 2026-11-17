//! 完整 jimage（JDK 的 `lib/modules`）索引读取：枚举 / 抽取 / 校验。
//!
//! 为什么不用 `classfile::load::jimage`：那个抽象只暴露「按条目名取字节」，
//! 而 `jimage list/extract/verify` 需要**枚举全部条目**与**读 header 字段**。
//! 格式与那边的解码头一致（同一份 `jdk.internal.jimage.ImageLocation` 位压缩编码），
//! 两边各自独立实现、互为交叉校验。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const MAGIC: u32 = 0xCAFE_DADA;

const ATTR_MODULE: usize = 1;
const ATTR_PARENT: usize = 2;
const ATTR_BASE: usize = 3;
const ATTR_EXTENSION: usize = 4;
const ATTR_OFFSET: usize = 5;
const ATTR_COMPRESSED: usize = 6;
const ATTR_UNCOMPRESSED: usize = 7;
const ATTR_COUNT: usize = 8;

/// 一条索引条目
#[derive(Clone, Debug, Default)]
pub struct Entry {
    /// 完整条目名：`/java.base/java/lang/Object.class`
    pub name: String,
    pub module: String,
    pub parent: String,
    pub base: String,
    pub ext: String,
    /// 数据在文件里的绝对偏移（= index_size + attr[offset]）
    pub abs_offset: u64,
    pub compressed: u64,
    pub uncompressed: u64,
}

impl Entry {
    /// 模块内相对路径：`java/lang/Object.class`（没有模块时就是全名去前导 `/`）
    pub fn rel_path(&self) -> String {
        if self.module.is_empty() {
            return self.name.trim_start_matches('/').to_string();
        }
        match self.name.strip_prefix(&format!("/{}/", self.module)) {
            Some(r) => r.to_string(),
            None => self.name.trim_start_matches('/').to_string(),
        }
    }
}

/// 头部的原始字段（`jimage info` 全都要打出来）
#[derive(Clone, Debug, Default)]
pub struct Header {
    pub magic: u32,
    pub major_version: u16,
    pub minor_version: u16,
    pub flags: u32,
    pub resource_count: u32,
    pub table_length: u32,
    pub locations_size: u32,
    pub strings_size: u32,
    pub index_size: u64,
    pub file_size: u64,
}

pub struct Image {
    pub path: PathBuf,
    pub header: Header,
    index: Vec<u8>,
    entries: Vec<Entry>,
    file: File,
}

fn le32(b: &[u8], o: usize) -> Option<u32> {
    if o + 4 > b.len() {
        return None;
    }
    Some(u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]))
}

impl Image {
    /// 打开并解出**全部**索引条目（`lib/modules` 约 3 万条，几十毫秒）。
    /// 只把索引区读进内存（约 1~2 MB 级），资源数据按需 seek，不整份读 743MB。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        let mut file = File::open(&path).map_err(|e| format!("打开 {} 失败: {e}", path.display()))?;
        let file_size = file
            .metadata()
            .map_err(|e| format!("读 {} 的文件信息失败: {e}", path.display()))?
            .len();
        let mut head = [0u8; 28];
        file.read_exact(&mut head)
            .map_err(|e| format!("读 {} 的头部失败（文件只有 {file_size} 字节）: {e}", path.display()))?;
        let magic = le32(&head, 0).unwrap_or(0);
        if magic != MAGIC {
            return Err(format!(
                "{} 不是 jimage 文件（magic = {magic:#010x}，应为 {MAGIC:#010x}）",
                path.display()
            ));
        }
        let major_version = u16::from_le_bytes([head[6], head[7]]);
        let minor_version = u16::from_le_bytes([head[4], head[5]]);
        let flags = le32(&head, 8).unwrap_or(0);
        let resource_count = le32(&head, 12).unwrap_or(0);
        let table_length = le32(&head, 16).unwrap_or(0);
        let locations_size = le32(&head, 20).unwrap_or(0);
        let strings_size = le32(&head, 24).unwrap_or(0);
        let index_size = 28u64
            + table_length as u64 * 8
            + locations_size as u64
            + strings_size as u64;
        if index_size > file_size {
            return Err(format!(
                "{} 的索引区声称 {index_size} 字节，但文件只有 {file_size} 字节（被截断的 jimage）",
                path.display()
            ));
        }
        // 索引区 = header 之后的部分：redirect[] + offsets[] + locations[] + strings[]
        let body = (index_size - 28) as usize;
        let mut index = vec![0u8; body];
        file.read_exact(&mut index)
            .map_err(|e| format!("读 {} 的索引区（{body} 字节）失败: {e}", path.display()))?;
        let mut img = Image {
            path,
            header: Header {
                magic,
                major_version,
                minor_version,
                flags,
                resource_count,
                table_length,
                locations_size,
                strings_size,
                index_size,
                file_size,
            },
            index,
            entries: Vec::new(),
            file,
        };
        img.entries = img.build_entries()?;
        Ok(img)
    }

    fn offsets_at(&self) -> usize {
        self.header.table_length as usize * 4
    }
    fn locations_at(&self) -> usize {
        self.header.table_length as usize * 8
    }
    fn strings_at(&self) -> usize {
        self.header.table_length as usize * 8 + self.header.locations_size as usize
    }

    fn string(&self, o: usize) -> String {
        let strings_size = self.header.strings_size as usize;
        if o >= strings_size {
            return String::new();
        }
        let base = self.strings_at();
        let s = base + o;
        let end = base + strings_size;
        if s >= self.index.len() || end > self.index.len() {
            return String::new();
        }
        let mut e = s;
        while e < end && self.index[e] != 0 {
            e += 1;
        }
        String::from_utf8_lossy(&self.index[s..e]).into_owned()
    }

    fn decode(&self, mut p: usize) -> Option<[u64; ATTR_COUNT]> {
        let mut attr = [0u64; ATTR_COUNT];
        loop {
            let data = *self.index.get(p)?;
            p += 1;
            if data <= 7 {
                break;
            }
            let kind = (data >> 3) as usize;
            let len = (data & 7) as usize + 1;
            let mut v = 0u64;
            for _ in 0..len {
                v = (v << 8) | (*self.index.get(p)? as u64);
                p += 1;
            }
            if kind == 0 || kind >= ATTR_COUNT {
                return None;
            }
            attr[kind] = v;
        }
        Some(attr)
    }

    fn build_entries(&self) -> Result<Vec<Entry>, String> {
        let n = self.header.table_length as usize;
        let mut out = Vec::with_capacity(n);
        let offs = self.offsets_at();
        let locs = self.locations_at();
        for i in 0..n {
            let off = match le32(&self.index, offs + i * 4) {
                Some(v) => v as usize,
                None => {
                    return Err(format!("第 {i} 条的 location 偏移超出索引区（索引不完整）"));
                }
            };
            let attr = match self.decode(locs + off) {
                Some(a) => a,
                None => {
                    return Err(format!("第 {i} 条的 location 记录解码失败（索引被破坏）"));
                }
            };
            if attr[ATTR_BASE] == 0 {
                continue;
            }
            let module = if attr[ATTR_MODULE] == 0 {
                String::new()
            } else {
                self.string(attr[ATTR_MODULE] as usize)
            };
            let parent = if attr[ATTR_PARENT] == 0 {
                String::new()
            } else {
                self.string(attr[ATTR_PARENT] as usize)
            };
            let base = self.string(attr[ATTR_BASE] as usize);
            let ext = if attr[ATTR_EXTENSION] == 0 {
                String::new()
            } else {
                self.string(attr[ATTR_EXTENSION] as usize)
            };
            let mut name = String::new();
            if !module.is_empty() {
                name.push('/');
                name.push_str(&module);
                name.push('/');
            }
            if !parent.is_empty() {
                name.push_str(&parent);
                name.push('/');
            }
            name.push_str(&base);
            if !ext.is_empty() {
                name.push('.');
                name.push_str(&ext);
            }
            out.push(Entry {
                name,
                module,
                parent,
                base,
                ext,
                abs_offset: self.header.index_size + attr[ATTR_OFFSET],
                compressed: attr[ATTR_COMPRESSED],
                uncompressed: attr[ATTR_UNCOMPRESSED],
            });
        }
        Ok(out)
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// 条目原始字节。压缩条目（`jlink --compress`）明确报错，不静默返回空。
    pub fn read(&mut self, e: &Entry) -> Result<Vec<u8>, String> {
        if e.compressed != 0 {
            return Err(format!(
                "条目 {} 是压缩存储的（compressed={}），本实现只支持未压缩条目",
                e.name, e.compressed
            ));
        }
        if e.uncompressed > 512 * 1024 * 1024 {
            return Err(format!("条目 {} 声称 {} 字节，拒绝读入", e.name, e.uncompressed));
        }
        if e.abs_offset + e.uncompressed > self.header.file_size {
            return Err(format!(
                "条目 {} 的数据区间 [{}, {}) 超出文件大小 {}",
                e.name,
                e.abs_offset,
                e.abs_offset + e.uncompressed,
                self.header.file_size
            ));
        }
        self.file
            .seek(SeekFrom::Start(e.abs_offset))
            .map_err(|e2| format!("定位 {} 失败: {e2}", e.name))?;
        let mut buf = vec![0u8; e.uncompressed as usize];
        self.file
            .read_exact(&mut buf)
            .map_err(|e2| format!("读 {} 失败: {e2}", e.name))?;
        Ok(buf)
    }

    /// 每条索引的偏移/长度是否落在文件内、模块名是否非空、绝对偏移是否单调不重叠
    /// （`jimage verify` 的物理校验部分）。返回 (检查条数, 问题列表)。
    pub fn verify_layout(&self) -> (usize, Vec<String>) {
        let mut problems = Vec::new();
        let data_start = self.header.index_size;
        let mut checked = 0usize;
        let mut ranges: Vec<(u64, u64, &str)> = Vec::new();
        for e in &self.entries {
            checked += 1;
            if e.abs_offset < data_start {
                problems.push(format!(
                    "{}: 数据偏移 {} 落在索引区内（数据区起点 {}）",
                    e.name, e.abs_offset, data_start
                ));
                continue;
            }
            if e.compressed != 0 {
                problems.push(format!(
                    "{}: 压缩条目（compressed={}），本实现无法校验其内容",
                    e.name, e.compressed
                ));
                continue;
            }
            let end = e.abs_offset.saturating_add(e.uncompressed);
            if end > self.header.file_size {
                problems.push(format!(
                    "{}: 数据区间 [{}, {}) 超出文件大小 {}",
                    e.name, e.abs_offset, end, self.header.file_size
                ));
                continue;
            }
            ranges.push((e.abs_offset, end, e.name.as_str()));
        }
        // 区间重叠 = 索引自相矛盾
        ranges.sort_by_key(|r| r.0);
        for w in ranges.windows(2) {
            let (a_start, a_end, a_name) = w[0];
            let (b_start, b_name) = (w[1].0, w[1].2);
            if a_end > b_start {
                problems.push(format!(
                    "{a_name}（{}..{}）与 {b_name}（{b_start}..）数据区间重叠",
                    a_start, a_end
                ));
            }
        }
        (checked, problems)
    }
}

/// 从 jimage 或「模块目录树」打开一个类来源，用于 jnativescan / jdeprscan 之类
/// 需要遍历的场合。返回 (模块名, 类条目相对路径, 字节) 的收集器。
pub enum ClassInput {
    Image(Image),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_basics() {
        assert!(crate::glob_match("/java.base/**", "/java.base/java/lang/Object.class"));
        assert!(crate::glob_match("/java.base/*.class", "/java.base/module-info.class"));
        assert!(!crate::glob_match("/java.base/*.class", "/java.base/java/lang/Object.class"));
        assert!(crate::glob_match("*.class", "module-info.class"));
        assert!(!crate::glob_match("*.class", "a/b.class"));
    }
}
