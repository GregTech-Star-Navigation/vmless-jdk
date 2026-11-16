//! 统计与诊断：每次 GC 的耗时、回收量、晋升量、各代占用。

/// 一次 GC 的类型
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GcKind {
    Minor,
    Major,
    Full,
}

/// 累计统计（`Heap::stats()` 返回快照）
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub minor_count: u64,
    pub major_count: u64,
    pub full_count: u64,
    /// 最近一次 GC 停顿（纳秒）
    pub last_pause_ns: u128,
    /// 历史最大停顿
    pub max_pause_ns: u128,
    pub total_pause_ns: u128,
    /// 复制阶段单个分片的最大耗时（衡量「最大单步」）
    pub max_step_ns: u128,
    pub promoted_objects: u64,
    pub promoted_bytes: u64,
    pub collected_objects: u64,
    pub collected_bytes: u64,
    pub copied_bytes: u64,
    /// 以下为「此刻」的值，`stats()` 时刷新
    pub eden_used: u32,
    pub young_used: u32,
    /// 老年代存活字节（不含空闲槽）
    pub old_used: u32,
    pub old_live: usize,
    pub los_live: usize,
    pub old_free_bytes: usize,
    pub los_free_bytes: usize,
    pub capacity: usize,
    pub growth_events: u64,
    pub shrink_events: u64,
    /// 当前 eden 软上限（GC 触发点）
    pub eden_limit: u32,
    pub tenuring_threshold: u32,
    pub card_dirty: usize,
    pub satb_len: usize,
}

impl Stats {
    pub fn minor_pause_ms(&self) -> f64 {
        self.last_pause_ns as f64 / 1e6
    }
    pub fn max_pause_ms(&self) -> f64 {
        self.max_pause_ns as f64 / 1e6
    }
    pub fn max_step_ms(&self) -> f64 {
        self.max_step_ns as f64 / 1e6
    }
}

/// 环形历史里的一条 GC 记录
#[derive(Clone, Copy, Debug)]
pub struct GcRecord {
    pub kind: GcKind,
    pub pause_ns: u128,
    pub collected: u64,
    pub promoted: u64,
    pub young_before: u32,
    pub old_before: u32,
    pub old_after: u32,
}
