//! 共享的「假」native 内存 arena —— **全 VM 只有一份**。
//!
//! 为什么要有这个文件：`Unsafe.allocateMemory` / `Files.readAllBytes` / `sun.nio.ch.*` /
//! FFM 的 `MemorySegment` 都要「拿一个地址去读写一段内存」。本 VM 没有裸指针可用
//! （要过 GC / 多线程 / 不许 UB），所以发出去的地址是**句柄**：`BASE + 块内偏移`。
//! 关键是**所有**用到地址的实现必须落到同一张表上 —— 否则 A 组分配的地址 B 组读不到，
//! 表现就是「`allocateMemory` 成功但 `read` 全 0」。
//!
//! 归属：`nat_intrinsics.rs`（`Unsafe` 那组）与 `nat_os.rs`（文件/网络那组）共用本模块。
//! 模块声明放在 `nat_util.rs` 里（`#[path = "nat_mem.rs"] pub(crate) mod nat_mem;`），
//! 这样不用改 `native_reg.rs` / `vm/mod.rs` —— 那两处是几条并行工作线的公用文件。
//!
//! 用法（在兄弟模块里）：
//!
//! ```text
//! use super::nat_util::nat_mem;
//! let p = nat_mem::alloc(64);          // 拿地址
//! nat_mem::write_bytes(p, b"hi");      // 写
//! let v = nat_mem::read_bytes(p, 2);   // 读
//! nat_mem::free(p);
//! ```
//!
//! 约定（与 HotSpot 一致的部分）：`free(0)` 是无操作；`alloc(0)` 返回一个可 free 的有效地址；
//! 释放过的地址**不复用**（悬垂读写的后果才可预测：读 0、写丢弃）。

use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};

/// 假地址基址。挑一个像 `mmap` 结果的值，日志里一眼认得出是裸内存。
const BASE: u64 = 0x0000_7F00_0000_0000;
/// 块首对齐（`getLong`/`getDouble` 不会跨块）
const ALIGN: u64 = 16;

struct Arena {
    /// 块首地址 → 字节。**BTreeMap** 是为了「地址落在哪个块」能 O(log n) 查出来
    blocks: BTreeMap<u64, Vec<u8>>,
    /// 下一个块的相对偏移。**不回填**：释放过的地址不再复用
    next: u64,
}

static ARENA: LazyLock<Mutex<Arena>> =
    LazyLock::new(|| Mutex::new(Arena { blocks: BTreeMap::new(), next: 0 }));

fn lock() -> std::sync::MutexGuard<'static, Arena> {
    ARENA.lock().unwrap_or_else(|e| e.into_inner())
}

fn bump(a: &mut Arena, n: u64) -> u64 {
    let start = a.next;
    a.next = (start + n.max(1) + ALIGN - 1) / ALIGN * ALIGN + ALIGN;
    BASE + start
}

/// 分配 `n` 字节，返回假地址。`n == 0` 也给有效地址（HotSpot 允许并且要求可 free）。
pub(crate) fn alloc(n: u64) -> u64 {
    let mut a = lock();
    let addr = bump(&mut a, n);
    a.blocks.insert(addr, vec![0u8; n as usize]);
    addr
}

pub(crate) fn free(addr: u64) {
    // `freeMemory(0)` 必须是无操作（JDK 规范允许传 0）
    if addr == 0 {
        return;
    }
    lock().blocks.remove(&addr);
}

pub(crate) fn realloc(addr: u64, n: u64) -> u64 {
    if addr == 0 {
        return alloc(n);
    }
    let mut a = lock();
    let Some(old) = a.blocks.remove(&addr) else { return alloc(n) };
    let new_addr = bump(&mut a, n);
    let mut buf = vec![0u8; n as usize];
    // 缩小时只拷留下的部分；放大时多出来的保持 0（HotSpot 也不初始化）
    let keep = old.len().min(buf.len());
    buf[..keep].copy_from_slice(&old[..keep]);
    a.blocks.insert(new_addr, buf);
    new_addr
}

/// 地址 → (块内偏移, 块长)
fn locate(a: &Arena, addr: u64) -> Option<(usize, usize)> {
    if addr < BASE {
        return None;
    }
    let (&base, buf) = a.blocks.range(..=addr).next_back()?;
    let off = (addr - base) as usize;
    if off > buf.len() {
        return None; // 越过块尾：不算落在这块里
    }
    Some((off, buf.len()))
}

/// 读 `width` 字节（小端）。落不到任何块 → 0；跨出块尾的部分按 0 补
/// （真实内存里那是相邻字节，这里没有「相邻」可言，取 0 至少是确定的）。
pub(crate) fn read(addr: u64, width: usize) -> u64 {
    let a = lock();
    let Some((off, len)) = locate(&a, addr) else { return 0 };
    let base = a.blocks.range(..=addr).next_back().map(|(&b, _)| b).unwrap_or(BASE);
    let Some(buf) = a.blocks.get(&base) else { return 0 };
    let mut out = 0u64;
    for k in 0..width {
        if off + k < len {
            out |= (buf[off + k] as u64) << (8 * k);
        }
    }
    out
}

pub(crate) fn write(addr: u64, width: usize, v: u64) {
    let mut a = lock();
    let Some((off, len)) = locate(&a, addr) else { return };
    let base = a.blocks.range(..=addr).next_back().map(|(&b, _)| b).unwrap_or(BASE);
    let Some(buf) = a.blocks.get_mut(&base) else { return };
    for k in 0..width {
        if off + k < len {
            buf[off + k] = ((v >> (8 * k)) & 0xFF) as u8;
        }
    }
}

/// 读一段字节（`len` 跨出块尾的部分按 0 补）。`nat_os` 读文件内容/网络包用。
pub(crate) fn read_bytes(addr: u64, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    if len == 0 {
        return out;
    }
    let a = lock();
    if let Some((off, blen)) = locate(&a, addr) {
        let base = a.blocks.range(..=addr).next_back().map(|(&b, _)| b).unwrap_or(BASE);
        if let Some(buf) = a.blocks.get(&base) {
            let n = len.min(blen - off);
            out[..n].copy_from_slice(&buf[off..off + n]);
        }
    }
    out
}

/// 写一段字节（超出块尾的部分丢弃）。返回真正写进去的字节数。
pub(crate) fn write_bytes(addr: u64, data: &[u8]) -> usize {
    if data.is_empty() {
        return 0;
    }
    let mut a = lock();
    let Some((off, blen)) = locate(&a, addr) else { return 0 };
    let base = a.blocks.range(..=addr).next_back().map(|(&b, _)| b).unwrap_or(BASE);
    let Some(buf) = a.blocks.get_mut(&base) else { return 0 };
    let n = data.len().min(blen - off);
    buf[off..off + n].copy_from_slice(&data[..n]);
    n
}

/// 整段填充（`Unsafe.setMemory` 的落地）
pub(crate) fn fill(addr: u64, len: usize, val: u8) -> usize {
    if len == 0 {
        return 0;
    }
    let mut a = lock();
    let Some((off, blen)) = locate(&a, addr) else { return 0 };
    let base = a.blocks.range(..=addr).next_back().map(|(&b, _)| b).unwrap_or(BASE);
    let Some(buf) = a.blocks.get_mut(&base) else { return 0 };
    let n = len.min(blen - off);
    for b in &mut buf[off..off + n] {
        *b = val;
    }
    n
}

/// 段到段拷贝（`Unsafe.copyMemory` / `Arrays.copyOf` 的边界情况）
pub(crate) fn copy_mem(src: u64, dst: u64, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let data = read_bytes(src, len);
    write_bytes(dst, &data)
}

/// 地址所在的块：(块首, 块长)。`nat_os` 做「这个地址是哪个 buffer」的判定时用。
pub(crate) fn base_of(addr: u64) -> Option<(u64, usize)> {
    let a = lock();
    let (&base, buf) = a.blocks.range(..=addr).next_back()?;
    let off = (addr - base) as usize;
    if off > buf.len() {
        return None;
    }
    Some((base, buf.len()))
}

/// 已分配的块数（自检/测试用）
pub(crate) fn block_count() -> usize {
    lock().blocks.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_write_read_roundtrip() {
        let p = alloc(32);
        assert!(p >= BASE);
        assert_eq!(write_bytes(p + 4, b"hello"), 5);
        assert_eq!(read_bytes(p + 4, 5), b"hello".to_vec());
        // 小端整数读写
        write(p + 8, 4, 0x1234_5678);
        assert_eq!(read(p + 8, 4), 0x1234_5678);
        free(p);
        // 释放后读到 0、写入被丢弃
        assert_eq!(read_bytes(p + 4, 5), vec![0u8; 5]);
        assert_eq!(write_bytes(p + 4, b"x"), 0);
    }

    #[test]
    fn realloc_keeps_prefix() {
        let p = alloc(8);
        write_bytes(p, b"abcdefgh");
        let q = realloc(p, 16);
        assert_eq!(read_bytes(q, 8), b"abcdefgh".to_vec());
        assert_eq!(read_bytes(q, 16), [b"abcdefgh".to_vec(), vec![0u8; 8]].concat());
        free(q);
    }

    #[test]
    fn fill_and_copy() {
        let a = alloc(16);
        assert_eq!(fill(a, 16, 7), 16);
        assert_eq!(read_bytes(a, 4), vec![7u8; 4]);
        let b = alloc(16);
        assert_eq!(copy_mem(a, b, 16), 16);
        assert_eq!(read_bytes(b, 4), vec![7u8; 4]);
        free(a);
        free(b);
    }

    #[test]
    fn free_zero_is_noop() {
        free(0);
        assert_eq!(alloc(0) >= BASE, true);
    }
}
