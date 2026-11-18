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
//!
//! # 两种块
//!
//! | 种类 | 字节在哪 | 谁用 |
//! | --- | --- | --- |
//! | `Owned(Vec<u8>)` | arena 自己持有 | `Unsafe.allocateMemory`、各种中转缓冲 |
//! | `Mapped{ptr,len}` | **外部真内存**（`MapViewOfFile` 的视图） | `FileChannel.map` / `MemorySegment` 的文件映射 |
//!
//! 为什么要 `Mapped`：`MappedByteBuffer` 的语义是「写进去的东西立刻在映射里」——
//! `force()`、另一个通道读文件、`MappedByteBuffer.load()` 都依赖这一点。如果映射只是
//! 「拷一份到 arena」，`force()` 之后另一个通道读到的还是旧内容（或者得靠我们额外把
//! 整块写回文件，而文件可能被别的进程同时在改）。所以映射块必须**直接指向真映射**：
//! 对 arena 假地址的每一次读写都翻译成对真映射的读写，一个字节都不缓存。
//!
//! 代价是 `Mapped` 块的读写要穿过裸指针（`unsafe`）。这是**故意的**：那块内存就是
//! 内核给的映射视图，生命周期由 `unmap0` 管（在块被注销之后才 `UnmapViewOfFile`），
//! 本模块不负责释放它 —— 所以这里既不会 UB，也不会泄漏。

use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};

/// 假地址基址。挑一个像 `mmap` 结果的值，日志里一眼认得出是裸内存。
const BASE: u64 = 0x0000_7F00_0000_0000;
/// 块首对齐（`getLong`/`getDouble` 不会跨块）
const ALIGN: u64 = 16;

/// 一块地址空间背后的字节从哪来
enum Block {
    /// arena 自己持有一份
    Owned(Vec<u8>),
    /// 外部真内存（文件映射视图）：`ptr` 由 `unmap0` 负责释放
    Mapped { ptr: usize, len: usize },
}

impl Block {
    fn len(&self) -> usize {
        match self {
            Block::Owned(v) => v.len(),
            Block::Mapped { len, .. } => *len,
        }
    }
    fn as_slice(&self) -> &[u8] {
        match self {
            Block::Owned(v) => v,
            // SAFETY: `ptr`/`len` 来自 `MapViewOfFile` 成功返回的视图，且块被注销之前
            // 调用方保证不 `UnmapViewOfFile`（见 `unmap0`）。只读切片不跨线程持有。
            Block::Mapped { ptr, len } => unsafe { std::slice::from_raw_parts(*ptr as *const u8, *len) },
        }
    }
    fn as_mut_slice(&mut self) -> &mut [u8] {
        match self {
            Block::Owned(v) => v,
            // SAFETY: 同上。`&mut self` 说明这块此刻只有我们在写。
            Block::Mapped { ptr, len } => unsafe {
                std::slice::from_raw_parts_mut(*ptr as *mut u8, *len)
            },
        }
    }
}

struct Arena {
    /// 块首地址 → 字节。**BTreeMap** 是为了「地址落在哪个块」能 O(log n) 查出来
    blocks: BTreeMap<u64, Block>,
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
    a.blocks.insert(addr, Block::Owned(vec![0u8; n as usize]));
    addr
}

/// 把一块**外部真内存**挂进 arena，返回它的假地址。
///
/// 只登记地址，不拷贝：对返回地址的读写直接落到 `ptr` 指向的真内存上。
/// 配对的是 [`unmapped`]（注销）+ 调用方自己的 `UnmapViewOfFile`。
///
/// `len` 必须是 `ptr` 那块内存的实际可用长度 —— 越界读写会被 `locate()` 拦掉，
/// 所以它同时是安全边界。
pub(crate) fn alloc_mapped(ptr: usize, len: usize) -> u64 {
    alloc_mapped_aligned(ptr, len, ALIGN)
}

/// 同上，但要求返回地址按 `align` 对齐。
///
/// 为什么要 `align`：`MappedMemoryUtils` 用 `address & (pageSize-1)` 反推映射基址
/// （`mappingOffset`），基址必须落在一页的开头，否则它算出来的 `force0`/`load0`
/// 区间会往前越过映射开头。文件映射因此要求 64K（分配粒度）对齐。
pub(crate) fn alloc_mapped_aligned(ptr: usize, len: usize, align: u64) -> u64 {
    let mut a = lock();
    let align = align.max(ALIGN);
    a.next = (a.next + align - 1) / align * align;
    let addr = bump(&mut a, len as u64);
    a.blocks.insert(addr, Block::Mapped { ptr, len });
    addr
}

/// 注销一块映射（不释放真内存 —— 那是调用方 `UnmapViewOfFile` 的事）。
///
/// 必须在 `UnmapViewOfFile` **之前**调用：注销之后 arena 不再认得这个地址，
/// 任何残留的读写都变成「落不到块 → 读 0 / 写丢弃」，而不是访问已解除映射的指针。
pub(crate) fn unmapped(addr: u64) -> bool {
    matches!(lock().blocks.remove(&addr), Some(Block::Mapped { .. }))
}

/// `addr` 是不是一块映射；是则返回 `(块首, 长度, 真指针)`。
/// `force0` / `unmap0` 用它把 arena 假地址换回真映射。
pub(crate) fn mapped_at(addr: u64) -> Option<(u64, usize, usize)> {
    let a = lock();
    let (&base, b) = a.blocks.range(..=addr).next_back()?;
    if (addr - base) as usize > b.len() {
        return None;
    }
    match b {
        Block::Mapped { ptr, len } => Some((base, *len, *ptr + (addr - base) as usize)),
        Block::Owned(_) => None,
    }
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
    buf[..keep].copy_from_slice(&old.as_slice()[..keep]);
    a.blocks.insert(new_addr, Block::Owned(buf));
    new_addr
}

/// 地址 → (块内偏移, 块长)
fn locate(a: &Arena, addr: u64) -> Option<(usize, usize)> {
    if addr < BASE {
        return None;
    }
    let (&base, b) = a.blocks.range(..=addr).next_back()?;
    let off = (addr - base) as usize;
    if off > b.len() {
        return None; // 越过块尾：不算落在这块里
    }
    Some((off, b.len()))
}

/// 地址所在块的块首（`locate` 已经确认落在块里）
fn block_base(a: &Arena, addr: u64) -> Option<u64> {
    a.blocks.range(..=addr).next_back().map(|(&b, _)| b)
}

/// 读 `width` 字节（小端）。落不到任何块 → 0；跨出块尾的部分按 0 补
/// （真实内存里那是相邻字节，这里没有「相邻」可言，取 0 至少是确定的）。
pub(crate) fn read(addr: u64, width: usize) -> u64 {
    let a = lock();
    let Some((off, len)) = locate(&a, addr) else { return 0 };
    let Some(base) = block_base(&a, addr) else { return 0 };
    let Some(b) = a.blocks.get(&base) else { return 0 };
    let buf = b.as_slice();
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
    let Some(base) = block_base(&a, addr) else { return };
    let Some(b) = a.blocks.get_mut(&base) else { return };
    let buf = b.as_mut_slice();
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
        if let Some(base) = block_base(&a, addr) {
            if let Some(b) = a.blocks.get(&base) {
                let n = len.min(blen - off);
                out[..n].copy_from_slice(&b.as_slice()[off..off + n]);
            }
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
    let Some(base) = block_base(&a, addr) else { return 0 };
    let Some(b) = a.blocks.get_mut(&base) else { return 0 };
    let n = data.len().min(blen - off);
    b.as_mut_slice()[off..off + n].copy_from_slice(&data[..n]);
    n
}

/// 整段填充（`Unsafe.setMemory` 的落地）
pub(crate) fn fill(addr: u64, len: usize, val: u8) -> usize {
    if len == 0 {
        return 0;
    }
    let mut a = lock();
    let Some((off, blen)) = locate(&a, addr) else { return 0 };
    let Some(base) = block_base(&a, addr) else { return 0 };
    let Some(b) = a.blocks.get_mut(&base) else { return 0 };
    let n = len.min(blen - off);
    for x in &mut b.as_mut_slice()[off..off + n] {
        *x = val;
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
    let (&base, b) = a.blocks.range(..=addr).next_back()?;
    let off = (addr - base) as usize;
    if off > b.len() {
        return None;
    }
    Some((base, b.len()))
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

    /// 映射块：读写必须**直接落到外部真内存**，arena 里不留副本。
    #[test]
    fn mapped_block_aliases_real_memory() {
        let mut real = [0u8; 16];
        let addr = alloc_mapped(real.as_mut_ptr() as usize, real.len());
        // 假地址上写 → 真内存立刻可见
        assert_eq!(write_bytes(addr + 2, b"abc"), 3);
        assert_eq!(&real[2..5], b"abc");
        // 真内存改 → 假地址读得到
        real[8] = 0x5A;
        assert_eq!(read(addr + 8, 1), 0x5A);
        // 边界：块外的部分读 0、写丢弃（不能越界踩到真内存）
        assert_eq!(write_bytes(addr + 15, b"zz"), 1);
        assert_eq!(read_bytes(addr + 16, 2), vec![0u8; 2]);
        // 登记信息能换回真指针
        let (base, len, ptr) = mapped_at(addr + 2).expect("mapped_at");
        assert_eq!(base, addr);
        assert_eq!(len, 16);
        assert_eq!(ptr, real.as_ptr() as usize + 2);
        // 注销后不再认这个地址
        assert!(unmapped(addr));
        assert!(mapped_at(addr).is_none());
        assert_eq!(read_bytes(addr, 4), vec![0u8; 4]);
    }
}
