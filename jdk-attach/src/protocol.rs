//! 控制通道的帧协议（客户端 ↔ 目标 VM）。
//!
//! 设计目标（对照 HotSpot 的 attach listener）：
//! - **定长头 + 长度前缀载荷**：读侧先读 8 字节头，按 `len` 预分配，永远不会因为
//!   对端乱发而 OOM（`MAX_PAYLOAD` 兜底）。
//! - **版本号参与握手**：版本不符时服务端仍然能回一个合法帧（`Status::BadVersion`），
//!   客户端因此拿到的是"协议版本不符"而不是"连接被重置"。
//! - **回复可分段**：`FLAG_MORE` 让大载荷（hprof）分块流式返回，单帧不超过 1 MiB。
//!
//! ```text
//!  0        1        2        3        4        5                8
//! +--------+--------+--------+--------+--------+----------------+
//! | magic  | version|   op   | flags  |       payload_len       |
//! +--------+--------+--------+--------+-------------------------+
//! |                     payload (payload_len 字节)               |
//! +-------------------------------------------------------------+
//! ```
//!
//! 回复帧：`op | FLAG_REPLY`，`payload[0]` 是 `Status`，其余是正文。

use std::io::{self, Read, Write};

/// 'V' —— VMless。
pub const MAGIC: u8 = 0x56;
/// 当前协议版本。改动帧格式必须 +1。
pub const VERSION: u8 = 1;
/// 单帧载荷上限（1 MiB）。大结果靠 `FLAG_MORE` 分块。
pub const MAX_PAYLOAD: u32 = 1 << 20;
/// 帧头长度。
pub const HEADER_LEN: usize = 8;

pub const FLAG_REPLY: u8 = 0x80;
/// 还有后续帧。
pub const FLAG_MORE: u8 = 0x01;

/// 请求操作码。所有值必须 `< FLAG_REPLY`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Op {
    /// 握手：载荷 = `token` 的 ASCII 字节。回复 `Status::Ok` 表示令牌正确。
    Hello = 1,
    /// 执行一条 jcmd 风格命令。载荷 = UTF-8 命令行。
    Command = 2,
    /// 堆直方图。载荷 = 空（或 `live` 标志）。回复 = 结构化二进制（见 [`crate::heap::HistoWire`]）。
    HeapHisto = 3,
    /// 堆转储（hprof）。载荷 = 空。回复 = 分块流式 hprof 字节。
    HeapDump = 4,
    /// 类加载器统计。载荷 = 空。回复 = 结构化二进制。
    ClassLoaders = 5,
    /// 活性探测：回复 `Status::Ok` + 一行版本信息。
    Ping = 6,
}

impl Op {
    pub fn from_u8(v: u8) -> Option<Op> {
        Some(match v {
            1 => Op::Hello,
            2 => Op::Command,
            3 => Op::HeapHisto,
            4 => Op::HeapDump,
            5 => Op::ClassLoaders,
            6 => Op::Ping,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Op::Hello => "Hello",
            Op::Command => "Command",
            Op::HeapHisto => "HeapHisto",
            Op::HeapDump => "HeapDump",
            Op::ClassLoaders => "ClassLoaders",
            Op::Ping => "Ping",
        }
    }
}

/// 回复状态。`0` 必须是成功，其余都是错误 —— 客户端按 `status != 0` 判错。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    Ok = 0,
    /// 令牌不对：不是同一个用户，或不是我们的 VM。
    BadToken = 1,
    /// 协议版本不符。正文是服务端支持的主版本号。
    BadVersion = 2,
    /// 不认识的操作码。
    BadOp = 3,
    /// 命令不存在 / 参数不对。
    BadCommand = 4,
    /// 命令执行失败（正文是错误信息）。
    Failed = 5,
    /// 服务端不认识 / 不支持（例如 VM 没接 GC 钩子）。
    Unsupported = 6,
    /// 内部错误。
    Internal = 7,
}

impl Status {
    pub fn from_u8(v: u8) -> Status {
        match v {
            0 => Status::Ok,
            1 => Status::BadToken,
            2 => Status::BadVersion,
            3 => Status::BadOp,
            4 => Status::BadCommand,
            5 => Status::Failed,
            6 => Status::Unsupported,
            _ => Status::Internal,
        }
    }
    pub fn is_ok(self) -> bool {
        self == Status::Ok
    }
}

/// 一帧。
#[derive(Debug, Clone)]
pub struct Frame {
    pub op: u8,
    pub flags: u8,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn request(op: Op, payload: Vec<u8>) -> Frame {
        Frame { op: op as u8, flags: 0, payload }
    }
    pub fn reply(op: Op, status: Status, body: Vec<u8>) -> Frame {
        let mut payload = Vec::with_capacity(body.len() + 1);
        payload.push(status as u8);
        payload.extend_from_slice(&body);
        Frame { op: (op as u8) | FLAG_REPLY, flags: 0, payload }
    }
    pub fn is_reply(&self) -> bool {
        self.flags & FLAG_REPLY != 0 || self.op & FLAG_REPLY != 0
    }
    pub fn more(&self) -> bool {
        self.flags & FLAG_MORE != 0
    }
    /// 请求帧的操作码（去掉 reply 位）。
    pub fn req_op(&self) -> Option<Op> {
        Op::from_u8(self.op & !FLAG_REPLY)
    }
    pub fn status(&self) -> Status {
        self.payload.first().map(|b| Status::from_u8(*b)).unwrap_or(Status::Internal)
    }
    pub fn body(&self) -> &[u8] {
        self.payload.get(1..).unwrap_or(&[])
    }
    pub fn body_string(&self) -> String {
        String::from_utf8_lossy(self.body()).into_owned()
    }
}

/// 读帧时可能出的错。**全部是显式的**，没有一处靠 panic 表达。
#[derive(Debug)]
pub enum FrameError {
    /// 对端关闭（干净 EOF：一个字节都没读到）。
    Eof,
    /// 读了一半就断了。
    Truncated(&'static str),
    Io(io::Error),
    BadMagic(u8),
    /// 版本不符 —— 仍然带回 `version`，调用方要能据此回一个 BadVersion 帧。
    BadVersion {
        version: u8,
        op: u8,
        flags: u8,
        payload: Vec<u8>,
    },
    /// 载荷长度超过 `MAX_PAYLOAD`。
    TooLarge(u32),
    BadOp(u8),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Eof => write!(f, "对端已关闭连接"),
            FrameError::Truncated(w) => write!(f, "读取{}时连接中断", w),
            FrameError::Io(e) => write!(f, "{}", e),
            FrameError::BadMagic(b) => write!(f, "协议魔数不正确 (0x{:02x}，应为 0x{:02x})", b, MAGIC),
            FrameError::BadVersion { version, .. } => {
                write!(f, "协议版本不符: 对端 {}，本端 {}", version, VERSION)
            }
            FrameError::TooLarge(n) => write!(f, "帧载荷过大 ({} 字节，上限 {})", n, MAX_PAYLOAD),
            FrameError::BadOp(o) => write!(f, "未知操作码 0x{:02x}", o),
        }
    }
}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            FrameError::Eof
        } else {
            FrameError::Io(e)
        }
    }
}

fn read_exact_or(r: &mut impl Read, buf: &mut [u8], what: &'static str) -> Result<(), FrameError> {
    match r.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            // 一个字节都没读到时算干净 EOF
            Err(FrameError::Truncated(what))
        }
        Err(e) => Err(FrameError::Io(e)),
    }
}

/// 读一个帧。**不做版本检查以外的语义校验**，调用方决定怎么处理。
pub fn read_frame(r: &mut impl Read) -> Result<Frame, FrameError> {
    let mut head = [0u8; HEADER_LEN];
    let mut got = 0usize;
    let mut spins = 0u32;
    while got < HEADER_LEN {
        match r.read(&mut head[got..]) {
            Ok(0) => {
                if got == 0 {
                    return Err(FrameError::Eof);
                }
                return Err(FrameError::Truncated("帧头"));
            }
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            // 读超时/暂时无数据：**不是**致命错误 —— 调用方用 socket 的 read timeout
            // 当整体截止时间，这里重试到它真的超时为止。spin 计数兜底防止死循环。
            Err(e)
                if (e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut)
                    && spins < 10_000 =>
            {
                spins += 1;
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            Err(e) => return Err(FrameError::Io(e)),
        }
    }
    if head[0] != MAGIC {
        return Err(FrameError::BadMagic(head[0]));
    }
    let version = head[1];
    let op = head[2];
    let flags = head[3];
    let len = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
    if len > MAX_PAYLOAD {
        return Err(FrameError::TooLarge(len));
    }
    let mut payload = vec![0u8; len as usize];
    if len > 0 {
        read_exact_or(r, &mut payload, "载荷").map_err(|e| match e {
            FrameError::Truncated(_) => FrameError::Truncated("载荷"),
            other => other,
        })?;
    }
    if version != VERSION {
        return Err(FrameError::BadVersion { version, op, flags, payload });
    }
    Ok(Frame { op, flags, payload })
}

/// 写一个帧（自动切块：超过 `MAX_PAYLOAD` 的载荷按 `FLAG_MORE` 分片）。
pub fn write_frame(w: &mut impl Write, f: &Frame) -> io::Result<()> {
    if f.payload.len() <= MAX_PAYLOAD as usize {
        return write_one(w, f.op, f.flags & !FLAG_MORE, &f.payload);
    }
    let mut rest = &f.payload[..];
    while !rest.is_empty() {
        let n = rest.len().min(MAX_PAYLOAD as usize);
        let (chunk, tail) = rest.split_at(n);
        let more = !tail.is_empty();
        write_one(w, f.op, if more { f.flags | FLAG_MORE } else { f.flags & !FLAG_MORE }, chunk)?;
        rest = tail;
    }
    Ok(())
}

fn write_one(w: &mut impl Write, op: u8, flags: u8, payload: &[u8]) -> io::Result<()> {
    let len = payload.len() as u32;
    let mut head = [0u8; HEADER_LEN];
    head[0] = MAGIC;
    head[1] = VERSION;
    head[2] = op;
    head[3] = flags;
    head[4..8].copy_from_slice(&len.to_le_bytes());
    w.write_all(&head)?;
    w.write_all(payload)?;
    w.flush()
}

// ---------------------------------------------------------------------------
// 结构化载荷的小工具（避免为了几个字段引入序列化依赖）
// ---------------------------------------------------------------------------

/// 紧凑写缓冲。
#[derive(Default)]
pub struct Buf(pub Vec<u8>);

impl Buf {
    pub fn new() -> Buf {
        Buf(Vec::new())
    }
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn i64(&mut self, v: i64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn str(&mut self, s: &str) -> &mut Self {
        let b = s.as_bytes();
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
        self
    }
    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

/// 紧凑读缓冲。读越界返回 `None`，调用方转成 `Status::Failed`。
pub struct Cur<'a> {
    pub d: &'a [u8],
    pub p: usize,
}

impl<'a> Cur<'a> {
    pub fn new(d: &'a [u8]) -> Cur<'a> {
        Cur { d, p: 0 }
    }
    pub fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.p + n > self.d.len() {
            return None;
        }
        let s = &self.d[self.p..self.p + n];
        self.p += n;
        Some(s)
    }
    pub fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    pub fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn i64(&mut self) -> Option<i64> {
        self.take(8).map(|b| {
            i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
        })
    }
    pub fn u64(&mut self) -> Option<u64> {
        self.i64().map(|v| v as u64)
    }
    pub fn str(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        if n > 1 << 20 {
            return None;
        }
        let b = self.take(n)?;
        Some(String::from_utf8_lossy(b).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_small() {
        let f = Frame::request(Op::Command, b"VM.version".to_vec());
        let mut out = Vec::new();
        write_frame(&mut out, &f).unwrap();
        let back = read_frame(&mut out.as_slice()).unwrap();
        assert_eq!(back.op, Op::Command as u8);
        assert_eq!(back.payload, b"VM.version");
    }

    #[test]
    fn roundtrip_chunked_and_reassembled() {
        // 3 MiB 载荷会被切成 3 帧
        let big = vec![7u8; 3 << 20];
        let f = Frame {
            op: Op::HeapDump as u8 | FLAG_REPLY,
            flags: 0,
            payload: big.clone(),
        };
        let mut out = Vec::new();
        write_frame(&mut out, &f).unwrap();
        let mut cur = out.as_slice();
        let mut acc = Vec::new();
        let mut frames = 0;
        loop {
            let fr = read_frame(&mut cur).unwrap();
            frames += 1;
            acc.extend_from_slice(&fr.payload);
            if !fr.more() {
                break;
            }
        }
        assert_eq!(frames, 3);
        assert_eq!(acc, big);
    }

    #[test]
    fn bad_magic_is_reported() {
        let mut d = Vec::new();
        d.extend_from_slice(&[0x99, 1, 1, 0, 0, 0, 0, 0]);
        match read_frame(&mut d.as_slice()) {
            Err(FrameError::BadMagic(0x99)) => {}
            other => panic!("expected BadMagic, got {:?}", other.map(|f| f.op)),
        }
    }

    #[test]
    fn wrong_version_keeps_payload() {
        let mut d = Vec::new();
        d.extend_from_slice(&[MAGIC, 9, Op::Hello as u8, 0, 2, 0, 0, 0]);
        d.extend_from_slice(b"hi");
        match read_frame(&mut d.as_slice()) {
            Err(FrameError::BadVersion { version, payload, .. }) => {
                assert_eq!(version, 9);
                assert_eq!(payload, b"hi");
            }
            _ => panic!("expected BadVersion"),
        }
    }

    #[test]
    fn oversized_is_rejected_before_alloc() {
        let mut d = Vec::new();
        d.extend_from_slice(&[MAGIC, 1, 1, 0]);
        d.extend_from_slice(&(MAX_PAYLOAD + 1).to_le_bytes());
        match read_frame(&mut d.as_slice()) {
            Err(FrameError::TooLarge(n)) => assert_eq!(n, MAX_PAYLOAD + 1),
            _ => panic!("expected TooLarge"),
        }
    }

    #[test]
    fn truncated_payload() {
        let mut d = Vec::new();
        d.extend_from_slice(&[MAGIC, 1, 1, 0, 100, 0, 0, 0]);
        d.extend_from_slice(b"abc");
        match read_frame(&mut d.as_slice()) {
            Err(FrameError::Truncated(_)) => {}
            _ => panic!("expected Truncated"),
        }
    }

    #[test]
    fn eof_is_clean() {
        match read_frame(&mut std::io::empty()) {
            Err(FrameError::Eof) => {}
            _ => panic!("expected Eof"),
        }
    }
}
