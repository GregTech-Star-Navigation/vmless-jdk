//! 读：把 `.jfr` 拆成「chunk → 元数据 / 常量池 / 事件流」。
//!
//! # 为什么先读常量池再读事件
//!
//! HotSpot 在 **flush/关闭** 的时候才把常量池写到文件尾部（`constantPoolPosition`
//! 通常接近文件末尾）。事件体里只有 key，所以真 `jfr` 的 `ChunkParser` 构造时先按
//! `delta` 链把常量池全读出来，再把游标挪回 chunk 头后面顺序读事件。这里完全照做。
//!
//! # 坏数据的处理
//!
//! 每一个入口都返回 `Result`：截断、magic 不对、版本不支持、偏移越界、
//! 池下标越界、类型未定义、数组长度离谱 —— 全部是**明确错误**，不 panic、不静默吞。
//! 这是 fuzz 5 万例 0 panic 的实现基础。

use crate::bail;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::format::chunk::ChunkHeader;
use crate::format::cp::ConstantPool;
use crate::format::event::{parse_event, parse_value, EventRecord};
use crate::format::metadata::{read_element, read_pool, Element};
use crate::format::{Cursor, Error, Result, BLOCK_CHECKPOINT, BLOCK_METADATA, HEADER_SIZE, MAGIC};
use crate::model::Metadata;

/// 一个 chunk 的元数据 + 常量池。
#[derive(Debug, Clone)]
pub struct ChunkData {
    pub header: ChunkHeader,
    pub metadata: Metadata,
    pub cp: ConstantPool,
}

/// 一个打开的录制文件。
///
/// `data` 是整份文件（≥50MB 是常态）。之所以一次性读进来：JFR 的常量池在文件尾部、
/// 事件在中间，随机访问是格式本身的要求；内存映射在无依赖的前提下不可移植，
/// 而且读进来的峰值内存就是文件大小，可预测。
#[derive(Debug)]
pub struct Recording {
    data: Vec<u8>,
    path: Option<PathBuf>,
    first: ChunkHeader,
    chunks: Vec<ChunkHeader>,
}

impl Recording {
    pub fn open(path: impl AsRef<Path>) -> Result<Recording> {
        let path = path.as_ref();
        let data = std::fs::read(path).map_err(|e| Error::new(format!("读 {} 失败: {}", path.display(), e)))?;
        let mut r = Recording::from_bytes(data)?;
        r.path = Some(path.to_path_buf());
        Ok(r)
    }

    pub fn from_bytes(data: Vec<u8>) -> Result<Recording> {
        // 最小长度先卡住：连头都不完整就不必往下走
        if data.len() < HEADER_SIZE as usize {
            if data.len() < 4 || data[..data.len().min(4)] != MAGIC[..data.len().min(4)] {
                bail!("不是 Flight Recorder 文件（文件只有 {} 字节）", data.len());
            }
            bail!("不是完整的 chunk 头（文件只有 {} 字节，需要 68）", data.len());
        }
        let first = ChunkHeader::read(&data, 0)?;
        if !first.is_finished() {
            bail!(
                "chunk 还在写入中（fileState={}），真 jfr 会一直等下去；请用录制完成后的文件",
                first.file_state
            );
        }
        if first.metadata_pos == 0 {
            bail!("chunk 没有元数据（metadataPosition=0），文件不完整");
        }
        let mut chunks = vec![first];
        loop {
            let last = *chunks.last().unwrap_or(&first);
            if last.is_final() || last.end() >= data.len() as u64 {
                break;
            }
            let end = last.end();
            if end <= last.offset || end > data.len() as u64 {
                bail!(
                    "chunk 声明的大小越界：offset={} size={} 但文件只有 {} 字节",
                    last.offset,
                    last.chunk_size,
                    data.len()
                );
            }
            let next = ChunkHeader::read(&data, end)?;
            if next.metadata_pos == 0 {
                bail!("第 {} 个 chunk 没有元数据", chunks.len() + 1);
            }
            chunks.push(next);
            if chunks.len() > 100_000 {
                bail!("chunk 数量异常（>100000），数据可能损坏");
            }
        }
        Ok(Recording {
            data,
            path: None,
            first,
            chunks,
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn file_size(&self) -> u64 {
        self.data.len() as u64
    }

    pub fn chunks(&self) -> &[ChunkHeader] {
        &self.chunks
    }

    pub fn first_header(&self) -> &ChunkHeader {
        &self.first
    }

    /// 整份录制的时长（各 chunk 之和）。
    pub fn duration_nanos(&self) -> i64 {
        self.chunks
            .iter()
            .fold(0i64, |acc, c| acc.saturating_add(c.duration_nanos))
    }

    /// 第一个 chunk 的元数据（`jfr metadata` 用）。
    pub fn chunk_data(&self, idx: usize) -> Result<ChunkData> {
        let h = match self.chunks.get(idx) {
            Some(h) => *h,
            None => bail!("chunk 下标 {} 越界（共 {} 个）", idx, self.chunks.len()),
        };
        self.load_chunk(h)
    }

    /// 读一个 chunk 的元数据块 + 常量池。
    pub fn load_chunk(&self, h: ChunkHeader) -> Result<ChunkData> {
        let metadata = self.read_metadata(&h)?;
        let mut cp = ConstantPool::new();
        if let Some(sid) = metadata.type_id_by_name(crate::model::TYPE_STRING) {
            cp.set_string_type_id(sid);
        }
        self.read_constant_pools(&h, &metadata, &mut cp)?;
        Ok(ChunkData { header: h, metadata, cp })
    }

    fn read_metadata(&self, h: &ChunkHeader) -> Result<Metadata> {
        let abs = h.metadata_abs();
        let mut c = Cursor::at(&self.data, usize::try_from(abs).map_err(|_| Error::new("偏移过大"))?);
        let size = c.leb()?;
        if size <= 0 {
            bail!("元数据块大小非法：{}", size);
        }
        let type_id = c.leb()?;
        if type_id != BLOCK_METADATA {
            bail!(
                "元数据块在最偏移 {} 处，但类型 id 是 {}（应为 0）",
                abs,
                type_id
            );
        }
        let _ts = c.leb()?;
        let _dur = c.leb()?;
        let _metadata_id = c.leb()?;
        let pool = read_pool(&mut c)?;
        let root = read_element(&mut c, &pool, 0)?;
        metadata_from_root(&root)
    }

    fn read_constant_pools(
        &self,
        h: &ChunkHeader,
        meta: &Metadata,
        cp: &mut ConstantPool,
    ) -> Result<()> {
        // 常量池是一串**倒着**链接的块：头里的 constantPoolPosition 指向**最后**一块，
        // 每块的 `delta` 是「到上一块」的相对偏移 —— 负数！
        // 实测真文件：最后一块 delta = -193564，一路指回最早那块（delta = 0 收尾）。
        // 一开始按「正 delta 往后走」实现会立刻在 EOF 上撞墙，这个方向不能想当然。
        let mut pos = h.cp_abs();
        if pos == h.offset {
            return Ok(()); // 没有常量池
        }
        let mut blocks: Vec<Vec<(i64, i64, crate::model::Value)>> = Vec::new();
        let mut guard = 0usize;
        loop {
            guard += 1;
            if guard > 10_000 {
                bail!("常量池链过长（>10000 块），数据可能损坏");
            }
            let mut c = Cursor::at(
                &self.data,
                usize::try_from(pos).map_err(|_| Error::new("常量池偏移过大"))?,
            );
            let size = c.leb()?;
            if size <= 0 {
                bail!("常量池块大小非法：{}（偏移 {}）", size, pos);
            }
            let type_id = c.leb()?;
            if type_id != BLOCK_CHECKPOINT {
                bail!(
                    "偏移 {} 处期望 checkpoint（id=1），实际 id={}",
                    pos,
                    type_id
                );
            }
            let _ts = c.leb()?;
            let _dur = c.leb()?;
            let delta = c.leb()?;
            let _flags = c.u8()?;
            let pool_count = c.leb()?;
            if pool_count < 0 || pool_count as usize > c.remaining() {
                bail!("常量池条目类型数不合理：{}", pool_count);
            }
            let mut entries = Vec::new();
            for _ in 0..pool_count {
                let tid = c.leb()?;
                let count = c.leb()?;
                if count < 0 || count as usize > c.remaining() {
                    bail!("常量池 {} 的条目数不合理：{}", tid, count);
                }
                for _ in 0..count {
                    let key = c.leb()?;
                    let v = parse_value(&mut c, meta, cp, tid, 0, false, 0)
                        .map_err(|e| Error::new(format!("常量池 {} 键 {}: {}", tid, key, e)))?;
                    entries.push((tid, key, v));
                }
            }
            blocks.push(entries);
            if delta == 0 {
                break;
            }
            let next = if delta > 0 {
                pos.saturating_add(delta as u64)
            } else {
                match pos.checked_sub(delta.unsigned_abs()) {
                    Some(n) => n,
                    None => bail!("常量池链的 delta {} 越过了文件头（偏移 {}）", delta, pos),
                }
            };
            if next == pos || next >= self.data.len() as u64 {
                bail!("常量池链跳出文件：{} → {}（delta={}）", pos, next, delta);
            }
            if next > pos && pos != h.cp_abs() {
                // 正向链只允许出现在「头里指的不是最后一块」的畸形文件里，不做特殊处理
            }
            pos = next;
        }
        // 链是从新到旧走的；值以**新**块为准，所以要按旧 → 新插入。
        for entries in blocks.into_iter().rev() {
            for (tid, key, v) in entries {
                cp.insert(tid, key, v);
            }
        }
        // **不展开**池内部的引用：展开会把共享结构复制成每事件一份（内存翻十倍），
        // 而且会丢掉「这条引用用的是哪个键」（`RecordedClassLoader.getId()` 要它）。
        // 引用按需 `deref`（单层，调用方递归时自有深度保护）。
        Ok(())
    }

    /// 逐块遍历（`jfr summary` 用：**不解析值**，只看类型 id，所以很快）。
    ///
    /// 返回 `(type_id, size)` 序列；`jdk.Metadata` / `jdk.Checkpoint` 也会出现，
    /// 与真 jfr 的统计口径一致。
    pub fn for_each_block<F>(&self, mut f: F) -> Result<()>
    where
        F: FnMut(i64, u64),
    {
        for h in &self.chunks {
            let mut pos = h.event_start();
            let end = h.end().min(self.data.len() as u64);
            while pos < end {
                let mut c = Cursor::at(&self.data, pos as usize);
                let size = c.leb()?;
                if size <= 0 {
                    bail!("偏移 {} 处的事件块大小非法：{}", pos, size);
                }
                if size as u64 > end - pos {
                    bail!(
                        "偏移 {} 处的事件块大小 {} 越过了 chunk 末尾（剩 {} 字节）",
                        pos,
                        size,
                        end - pos
                    );
                }
                let type_id = c.leb()?;
                f(type_id, size as u64);
                pos += size as u64;
            }
        }
        Ok(())
    }

    /// 事件流（惰性解析：50MB 文件不会一次性全展开成值）。
    pub fn events(&self) -> Result<EventStream<'_>> {
        let first = self.load_chunk(self.first)?;
        Ok(EventStream {
            rec: self,
            chunk_idx: 0,
            pos: self.first.event_start(),
            cur: first,
            done: false,
            pending_err: None,
        })
    }
}

fn metadata_from_root(root: &Element) -> Result<Metadata> {
    Metadata::from_root(root)
}

/// 一条事件 + 它所属 chunk 的头（算时间要用）。
#[derive(Debug, Clone)]
pub struct LoadedEvent {
    pub chunk_index: usize,
    pub header: ChunkHeader,
    pub event: EventRecord,
}

/// 事件迭代器。遇到错误时**把错误交出去然后停**（不 panic，也不无限重试）。
pub struct EventStream<'a> {
    rec: &'a Recording,
    chunk_idx: usize,
    pos: u64,
    cur: ChunkData,
    done: bool,
    pending_err: Option<Error>,
}

impl<'a> Iterator for EventStream<'a> {
    type Item = Result<LoadedEvent>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if let Some(e) = self.pending_err.take() {
            self.done = true;
            return Some(Err(e));
        }
        loop {
            let end = self.cur.header.end().min(self.rec.data.len() as u64);
            while self.pos < end {
                let pos = self.pos;
                let mut c = Cursor::at(&self.rec.data, pos as usize);
                let size = match c.leb() {
                    Ok(v) => v,
                    Err(e) => return Some(self.fail(e)),
                };
                if size <= 0 || size as u64 > end - pos {
                    return Some(self.fail(Error::new(format!(
                        "偏移 {} 处的事件块大小非法：{}",
                        pos, size
                    ))));
                }
                let type_id = match c.leb() {
                    Ok(v) => v,
                    Err(e) => return Some(self.fail(e)),
                };
                self.pos = pos + size as u64;
                if type_id == BLOCK_METADATA || type_id == BLOCK_CHECKPOINT {
                    continue; // 元数据 / flush 检查点：跳过
                }
                if !self.cur.metadata.is_event(type_id) {
                    continue; // 不是事件类型（或元数据里没有）：跳过
                }
                let ev = parse_event(&mut c, &self.cur.metadata, &self.cur.cp, type_id)
                    .map_err(|e| Error::new(format!("偏移 {} 处的事件: {}", pos, e)));
                return match ev {
                    Ok(event) => Some(Ok(LoadedEvent {
                        chunk_index: self.chunk_idx,
                        header: self.cur.header,
                        event,
                    })),
                    Err(e) => Some(self.fail(e)),
                };
            }
            // 换下一个 chunk
            self.chunk_idx += 1;
            let h = match self.rec.chunks.get(self.chunk_idx) {
                Some(h) => *h,
                None => {
                    self.done = true;
                    return None;
                }
            };
            match self.rec.load_chunk(h) {
                Ok(cd) => {
                    self.pos = h.event_start();
                    self.cur = cd;
                }
                Err(e) => return Some(self.fail(e)),
            }
        }
    }
}

impl<'a> EventStream<'a> {
    fn fail(&mut self, e: Error) -> Result<LoadedEvent> {
        self.done = true;
        Err(e)
    }
}

/// `jfr summary` 的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryRow {
    pub name: String,
    pub count: u64,
    pub size: u64,
}

/// `jfr summary` 的全部信息。
#[derive(Debug, Clone)]
pub struct Summary {
    pub major: u16,
    pub minor: u16,
    pub chunks: usize,
    pub start_nanos: i64,
    pub duration_nanos: i64,
    pub rows: Vec<SummaryRow>,
    /// 名字列宽（真 jfr 用它对齐表头与分隔线）。
    pub name_width: usize,
}

impl Recording {
    /// 统计各事件类型的条数与字节数。
    ///
    /// 口径与 `Summary.printInformation` 逐条对齐：
    /// - 元数据（id 0）与 checkpoint（id 1）也计入，名字是 `jdk.Metadata` / `jdk.Checkpoint`；
    /// - 元数据里声明过的事件类型即使一条都没有也出现在表里（count=0）；
    /// - 未知 type id 归到 `"<id> (missing event metadata)"`；
    /// - 排序：count 降序，count 相同按名字升序（Java 侧是「先按名字、再按条数」两次稳定排序）。
    pub fn summary(&self) -> Result<Summary> {
        let prefix = if self.first.major == 1 {
            "com.oracle.jdk."
        } else {
            "jdk."
        };
        let mut rows: Vec<SummaryRow> = Vec::new();
        let mut index: HashMap<i64, usize> = HashMap::new();
        // 真 jfr 的列宽来自「元数据里声明的事件类型名」的最长者，
        // 再与表头 " Event Type" 取大 —— jdk.Metadata / jdk.Checkpoint 不参与。
        let mut name_width = 0usize;

        push_row(&mut rows, &mut index, 0, format!("{}Metadata", prefix));
        push_row(&mut rows, &mut index, 1, format!("{}Checkpoint", prefix));

        // 先把每个 chunk 声明的事件类型登记进去（count=0 也要出现在表里）
        for h in &self.chunks {
            let meta = self.read_metadata(h)?;
            for id in &meta.event_type_ids {
                if !index.contains_key(id) {
                    let name = meta.type_name(*id);
                    name_width = name_width.max(name.chars().count());
                    push_row(&mut rows, &mut index, *id, name);
                }
            }
        }

        self.for_each_block(|type_id, size| {
            let idx = match index.get(&type_id) {
                Some(i) => *i,
                None => {
                    let name = format!("{} (missing event metadata)", type_id);
                    let i = push_row(&mut rows, &mut index, type_id, name);
                    index.insert(type_id, i);
                    i
                }
            };
            if let Some(r) = rows.get_mut(idx) {
                r.count += 1;
                r.size += size;
            }
        })?;

        // 稳定排序：先名字升序，再 count 降序 → 最终 count 降序、同 count 名字升序
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows.sort_by(|a, b| b.count.cmp(&a.count));

        Ok(Summary {
            major: self.first.major,
            minor: self.first.minor,
            chunks: self.chunks.len(),
            start_nanos: self.first.start_nanos,
            duration_nanos: self.duration_nanos(),
            rows,
            name_width,
        })
    }
}

/// 往汇总表里加一行，返回它的下标。
fn push_row(
    rows: &mut Vec<SummaryRow>,
    index: &mut HashMap<i64, usize>,
    id: i64,
    name: String,
) -> usize {
    index.insert(id, rows.len());
    rows.push(SummaryRow {
        name,
        count: 0,
        size: 0,
    });
    rows.len() - 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_garbage_are_errors() {
        assert!(Recording::from_bytes(vec![]).is_err());
        assert!(Recording::from_bytes(b"FLR".to_vec()).is_err());
        assert!(Recording::from_bytes(vec![0u8; 200]).is_err());
        let mut b = vec![0u8; 68];
        b[..4].copy_from_slice(&MAGIC);
        // 版本 2.1 但 chunkSize/元数据位置全是 0
        b[4..6].copy_from_slice(&2u16.to_be_bytes());
        b[6..8].copy_from_slice(&1u16.to_be_bytes());
        assert!(Recording::from_bytes(b).is_err());
    }
}
