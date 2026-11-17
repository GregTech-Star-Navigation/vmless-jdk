//! 写：把「元数据 + 一堆事件」序列化成真 `jfr` 读得懂的 `.jfr`。
//!
//! # 一个 chunk 的字节布局（我们写出来的）
//!
//! ```text
//! [0..68)         chunk 头（最后回填）
//! [68..)          元数据块        （metadataPosition = 68）
//!                 事件块 × N
//! [..)            常量池块        （constantPoolPosition 指这里）
//! ```
//!
//! 常量池放在**最后**，和 HotSpot 一致：写事件时才知道要收哪些常量，
//! 而读侧本来就是「先跳到最后把池读完，再回来读事件」。
//!
//! # 三个必须踩对的点（踩错就是「真 jfr 打不开」）
//!
//! 1. `fileState = 0` —— 非 0 时真 jfr 认为文件还在写，`awaitFinished` 会一直等。
//! 2. `chunkSize` 必须**正好等于文件长度** —— 真 jfr 靠它判断「这是最后一个 chunk」。
//! 3. `ticksPerSecond` 用 1e9（分频 1），事件 ticks 就是相对纳秒，时间戳无损。

use std::path::Path;

use crate::format::chunk::ChunkHeader;
use crate::format::cp::PoolWriter;
use crate::format::event::{write_event, write_value, CpBuilder, EventRecord};
use crate::format::metadata::{collect_strings, write_element, write_pool, Element};
use crate::format::{push_block, write_leb, Cursor, Error, Result, BLOCK_METADATA, HEADER_SIZE};
use crate::model::Metadata;

/// 一个 chunk 的完整内容（写入口）。
#[derive(Debug, Clone)]
pub struct ChunkSpec {
    pub metadata: Metadata,
    pub events: Vec<EventRecord>,
    /// chunk 起点的墙钟纳秒（epoch）。
    pub start_nanos: i64,
    /// chunk 时长（纳秒）—— 真 jfr 的 `Duration:` 就是它。
    pub duration_nanos: i64,
    /// 元数据里的 `metadataId`（只影响多 chunk 复用，单 chunk 随便填）。
    pub metadata_id: i64,
}

impl ChunkSpec {
    pub fn new(metadata: Metadata, start_nanos: i64, duration_nanos: i64) -> ChunkSpec {
        ChunkSpec {
            metadata,
            events: Vec::new(),
            start_nanos,
            duration_nanos,
            metadata_id: 1,
        }
    }
}

/// 序列化整个录制（单个 chunk）。
pub fn build(spec: &ChunkSpec) -> Result<Vec<u8>> {
    // 1) 元数据块
    let root = spec.metadata.to_root();
    let meta_block = encode_metadata_block(&root, spec.metadata_id)?;

    // 2) 常量池：先收集、再登记（写事件时要用 key）
    let mut cp = CpBuilder::new();
    let mut todo: Vec<(i64, crate::model::Value)> = Vec::new();
    for ev in &spec.events {
        let t = match spec.metadata.type_by_id(ev.type_id) {
            Some(t) => t,
            None => {
                return Err(Error::new(format!(
                    "事件类型 id {} 不在元数据里",
                    ev.type_id
                )))
            }
        };
        for i in t.start_index()..t.fields.len() {
            let f = &t.fields[i];
            let v = match ev.values.get(i) {
                Some(v) => v,
                None => {
                    return Err(Error::new(format!(
                        "事件 {} 缺少字段 {} 的值",
                        t.name, f.name
                    )))
                }
            };
            CpBuilder::collect(&spec.metadata, f.type_id, f.dimension, f.constant_pool, v, &mut todo);
        }
    }
    for (tid, v) in &todo {
        cp.intern(*tid, v)?;
    }

    // 3) 事件块
    let mut body = Vec::with_capacity(spec.events.len() * 64 + 4096);
    body.extend_from_slice(&meta_block);
    for ev in &spec.events {
        write_event(&mut body, &spec.metadata, &cp, ev)?;
    }
    let cp_pos = HEADER_SIZE + body.len() as u64;

    // 4) 常量池块（哪怕为空也写一块：真 jfr 会从 constantPoolPosition 顺着链读）
    let pools = cp.pools();
    let cp_block = PoolWriter::write_block(&pools, |out, type_id, v| {
        // 常量池条目内部再遇到 constant_pool 字段就写 key —— 与读侧同一套规则
        let _ = write_value(out, &spec.metadata, &cp, type_id, 0, false, v);
    });
    body.extend_from_slice(&cp_block);

    let chunk_size = HEADER_SIZE + body.len() as u64;
    let header = ChunkHeader {
        offset: 0,
        major: 2,
        minor: 1,
        chunk_size,
        constant_pool_pos: cp_pos,
        metadata_pos: HEADER_SIZE,
        start_nanos: spec.start_nanos,
        duration_nanos: spec.duration_nanos.max(0),
        // 分频 1：ticks 就是纳秒
        start_ticks: 0,
        ticks_per_second: 1_000_000_000,
        file_state: 0,
        flags: 0b10, // 最后一个 chunk
    };
    let mut out = Vec::with_capacity(chunk_size as usize);
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(&body);
    debug_assert_eq!(out.len() as u64, chunk_size);
    Ok(out)
}

fn encode_metadata_block(root: &Element, metadata_id: i64) -> Result<Vec<u8>> {
    let mut strings = Vec::new();
    let mut seen = std::collections::HashMap::new();
    collect_strings(root, &mut strings, &mut seen);

    let mut payload = Vec::new();
    write_leb(&mut payload, 0); // 时间戳（流式才有意义）
    write_leb(&mut payload, 0); // 时长
    write_leb(&mut payload, metadata_id);
    let lookup = write_pool(&mut payload, &strings);
    write_element(&mut payload, root, &lookup)?;

    let mut out = Vec::new();
    push_block(&mut out, BLOCK_METADATA, &payload);
    Ok(out)
}

/// 写到文件。
pub fn write_file(path: impl AsRef<Path>, spec: &ChunkSpec) -> Result<u64> {
    let bytes = build(spec)?;
    let path = path.as_ref();
    std::fs::write(path, &bytes)
        .map_err(|e| Error::new(format!("写 {} 失败: {}", path.display(), e)))?;
    Ok(bytes.len() as u64)
}

/// 我们写出来的字节流自检：用**自己的读器**再读一遍，确认块结构闭合。
///
/// 真 jfr 能不能读是最终判据，但这个自检能在 CI 里跑、能把「写到一半越界」
/// 这类问题挡在提交之前。
pub fn verify(bytes: &[u8]) -> Result<(usize, usize)> {
    let rec = crate::reader::Recording::from_bytes(bytes.to_vec())?;
    let cd = rec.chunk_data(0)?;
    let mut events = 0usize;
    let n = cd.metadata.types.len();
    let mut c = Cursor::at(bytes, HEADER_SIZE as usize);
    // 头部之后应当是元数据块
    let _size = c.leb()?;
    let tid = c.leb()?;
    if tid != BLOCK_METADATA {
        return Err(Error::new("第一个块不是元数据块"));
    }
    let mut s = rec.events()?;
    while let Some(ev) = s.next() {
        match ev {
            Ok(_) => events += 1,
            Err(e) => return Err(e),
        }
    }
    Ok((n, events))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::event::EventRecord;
    use crate::model::Value;
    use crate::recorder::builtin_metadata;

    #[test]
    fn write_then_read_roundtrip() {
        let meta = builtin_metadata();
        let tid = meta.type_id_by_name("jdk.ThreadStart").unwrap();
        let types = &meta.types;
        let t = types.iter().find(|t| t.id == tid).unwrap();
        let mut values = vec![Value::Null; t.fields.len()];
        values[0] = Value::Int(1000);
        let mut spec = ChunkSpec::new(meta, 1_700_000_000_000_000_000, 5_000_000_000);
        spec.events.push(EventRecord {
            type_id: tid,
            start_ticks: 1000,
            duration_ticks: 0,
            values,
        });
        let bytes = build(&spec).unwrap();
        let (tn, en) = verify(&bytes).unwrap();
        assert!(tn > 3);
        assert_eq!(en, 1);
    }
}
