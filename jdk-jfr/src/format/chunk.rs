//! chunk 头：文件里的 68 字节。
//!
//! # 布局（与 `ChunkHeader` 逐字节对齐）
//!
//! ```text
//! 偏移  长度  含义
//! 0     4     'F' 'L' 'R' 0
//! 4     2     major（1 或 2）
//! 6     2     minor
//! 8     8     chunkSize（含头，整个 chunk 的字节数）
//! 16    8     constantPoolPosition（相对 chunk 起点）
//! 24    8     metadataPosition（相对 chunk 起点）
//! 32    8     chunkStartNanos（epoch 纳秒，墙钟）
//! 40    8     durationNanos
//! 48    8     chunkStartTicks（与 ticksPerSecond 配套的单调计数）
//! 56    8     ticksPerSecond
//! 64    1     fileState（0 = 已写完；255 = 正在更新）
//! 65    2     保留
//! 67    1     flags（bit1 = 最后一个 chunk）
//! ```
//!
//! `fileState != 0` 的文件在真 `jfr` 里会被当成「还在录」而**一直等**（`awaitFinished`），
//! 所以写侧必须置 0 —— 这一条踩过就知道有多安静。

use crate::bail;
use super::{Cursor, Error, Result, HEADER_SIZE, MAGIC};

/// 一个 chunk 的头部。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkHeader {
    /// 本 chunk 在文件里的绝对偏移。
    pub offset: u64,
    pub major: u16,
    pub minor: u16,
    pub chunk_size: u64,
    pub constant_pool_pos: u64,
    pub metadata_pos: u64,
    pub start_nanos: i64,
    pub duration_nanos: i64,
    pub start_ticks: i64,
    pub ticks_per_second: i64,
    pub file_state: u8,
    pub flags: u8,
}

impl ChunkHeader {
    /// 从 `data[offset..]` 读一个 chunk 头。
    pub fn read(data: &[u8], offset: u64) -> Result<ChunkHeader> {
        let off = usize::try_from(offset).map_err(|_| Error::new("chunk 偏移超出范围"))?;
        if data.len() < off + HEADER_SIZE as usize {
            bail!(
                "不是完整的 chunk 头：文件 {} 字节，偏移 {} 起不足 {} 字节（截断？）",
                data.len(),
                off,
                HEADER_SIZE
            );
        }
        let mut c = Cursor::at(data, off);
        let magic = c.take(4)?;
        if magic != MAGIC {
            bail!(
                "不是 Flight Recorder 文件：魔数是 {:?}，应以 FLR\\0 开头",
                String::from_utf8_lossy(magic)
            );
        }
        let major = c.be_u16()?;
        let minor = c.be_u16()?;
        if major != 1 && major != 2 {
            bail!(
                "不支持的 .jfr 版本 {}.{}（只支持 1.x / 2.x，与真 jfr 的报错一致）",
                major,
                minor
            );
        }
        let chunk_size = c.be_u64()?;
        let constant_pool_pos = c.be_u64()?;
        let metadata_pos = c.be_u64()?;
        let start_nanos = c.be_u64()? as i64;
        let duration_nanos = c.be_u64()? as i64;
        let start_ticks = c.be_u64()? as i64;
        let ticks_per_second = c.be_u64()? as i64;
        let file_state = c.u8()?;
        c.skip(2)?;
        let flags = c.u8()?;
        Ok(ChunkHeader {
            offset,
            major,
            minor,
            chunk_size,
            constant_pool_pos,
            metadata_pos,
            start_nanos,
            duration_nanos,
            start_ticks,
            ticks_per_second,
            file_state,
            flags,
        })
    }

    /// 序列化成 68 字节。
    pub fn encode(&self) -> [u8; HEADER_SIZE as usize] {
        let mut b = [0u8; HEADER_SIZE as usize];
        b[0..4].copy_from_slice(&MAGIC);
        b[4..6].copy_from_slice(&self.major.to_be_bytes());
        b[6..8].copy_from_slice(&self.minor.to_be_bytes());
        b[8..16].copy_from_slice(&self.chunk_size.to_be_bytes());
        b[16..24].copy_from_slice(&self.constant_pool_pos.to_be_bytes());
        b[24..32].copy_from_slice(&self.metadata_pos.to_be_bytes());
        b[32..40].copy_from_slice(&(self.start_nanos as u64).to_be_bytes());
        b[40..48].copy_from_slice(&(self.duration_nanos as u64).to_be_bytes());
        b[48..56].copy_from_slice(&(self.start_ticks as u64).to_be_bytes());
        b[56..64].copy_from_slice(&(self.ticks_per_second as u64).to_be_bytes());
        b[64] = self.file_state;
        b[67] = self.flags;
        b
    }

    /// `ticksPerSecond / 1e9` —— 与 `TimeConverter` **完全一致**。
    ///
    /// 注意是**浮点除法**：`TimeConverter.divisor` 是 `double`，而
    /// `getTicksPerSecond()` 的返回类型也是 `double`。一开始按整数除法写成
    /// `tps / 1_000_000_000`（Windows 上 QPC 频率 2.3GHz → 得到 2），
    /// 结果时间戳随着录制时长**线性漂移**：2 秒的文件末尾差了 6ms。
    /// 这种「看着只差一点点」的偏差在 diff 里很难一眼归因。
    pub fn divisor(&self) -> f64 {
        let d = self.ticks_per_second as f64 / 1_000_000_000.0;
        if d <= 0.0 {
            1.0
        } else {
            d
        }
    }

    /// ticks → 绝对纳秒（`TimeConverter.convertTimestamp`）。
    pub fn timestamp_nanos(&self, ticks: i64) -> i64 {
        let delta = ticks.wrapping_sub(self.start_ticks) as f64;
        self.start_nanos.wrapping_add((delta / self.divisor()) as i64)
    }

    /// ticks → 时长纳秒（`TimeConverter.convertTimespan`）。
    pub fn timespan_nanos(&self, ticks: i64) -> i64 {
        (ticks as f64 / self.divisor()) as i64
    }

    pub fn is_final(&self) -> bool {
        self.flags & 0b10 != 0
    }

    pub fn is_finished(&self) -> bool {
        self.file_state == 0
    }

    /// 本 chunk 的结束偏移（不含）。
    pub fn end(&self) -> u64 {
        self.offset.saturating_add(self.chunk_size)
    }

    /// 事件区起点：头之后。
    pub fn event_start(&self) -> u64 {
        self.offset.saturating_add(HEADER_SIZE)
    }

    /// 常量池 / 元数据的绝对偏移。
    pub fn cp_abs(&self) -> u64 {
        self.offset.saturating_add(self.constant_pool_pos)
    }

    pub fn metadata_abs(&self) -> u64 {
        self.offset.saturating_add(self.metadata_pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ChunkHeader {
        ChunkHeader {
            offset: 0,
            major: 2,
            minor: 1,
            chunk_size: 1234,
            constant_pool_pos: 1000,
            metadata_pos: 68,
            start_nanos: 1_700_000_000_000_000_000,
            duration_nanos: 5_000_000_000,
            start_ticks: 42,
            ticks_per_second: 1_000_000_000,
            file_state: 0,
            flags: 2,
        }
    }

    #[test]
    fn header_roundtrip() {
        let h = sample();
        let b = h.encode();
        let g = ChunkHeader::read(&b, 0).unwrap();
        assert_eq!(h, g);
        assert!(g.is_final());
        assert!(g.is_finished());
        assert_eq!(g.end(), 1234);
    }

    #[test]
    fn bad_magic_is_error() {
        let mut b = sample().encode();
        b[0] = b'X';
        assert!(ChunkHeader::read(&b, 0).is_err());
    }

    #[test]
    fn truncated_header_is_error() {
        let b = sample().encode();
        for n in 0..HEADER_SIZE as usize {
            assert!(ChunkHeader::read(&b[..n], 0).is_err(), "n={}", n);
        }
    }

    #[test]
    fn bad_version_is_error() {
        let mut h = sample();
        h.major = 3;
        let b = h.encode();
        assert!(ChunkHeader::read(&b, 0).is_err());
    }

    #[test]
    fn time_conversion_matches_java_integer_division() {
        let mut h = sample();
        // Windows QPC 频率
        h.ticks_per_second = 2_300_000_000;
        h.start_ticks = 1000;
        h.start_nanos = 1_000_000;
        assert!((h.divisor() - 2.3).abs() < 1e-9);
        assert_eq!(h.timestamp_nanos(1004), 1_000_001);
        assert_eq!(h.timespan_nanos(100), 43);
        // 时钟频率非法（0）时钳到 1.0，避免除零
        h.ticks_per_second = 0;
        assert_eq!(h.divisor(), 1.0);
        // 小于 1e9 但合法：Java 的 `double` 除法会给 1e-6 这样的除数，这里同样精确
        h.ticks_per_second = 1_000;
        assert!((h.divisor() - 1e-6).abs() < 1e-12);
    }
}
