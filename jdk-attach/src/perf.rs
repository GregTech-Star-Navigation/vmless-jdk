//! perfdata（性能计数器）—— **与 HotSpot 的二进制格式完全兼容**。
//!
//! 为什么选这个格式而不是自造：`jps` / `jstat` 读的就是它。写出去的字节只要对，
//! **真 jps / 真 jstat 就能直接看见我们的 VM**，这比"我们的工具读我们的文件"强得多
//! —— 它把"列名对齐"这件事从"我抄了表头"变成"真工具算出来的数就是这些列"。
//!
//! # 二进制布局（HotSpot v2.0，字段偏移已用真 JDK 25 的 perfdata 文件逐字节核对）
//!
//! ```text
//! Prologue（32 字节）
//!   0  i32  magic        = 0xcafec0c0
//!   4  i8   byte_order   = 1 (little endian)
//!   5  i8   major        = 2
//!   6  i8   minor        = 0
//!   7  i8   accessible   = 1
//!   8  i32  used         = 数据区已用字节数
//!  12  i32  overflow     = 0
//!  16  i64  mod_time     = 最后修改时间（毫秒）
//!  24  i32  entry_offset = 32
//!  28  i32  num_entries
//!
//! PerfDataEntry（20 字节头 + 名字 + 值）
//!   0  i32  entry_length
//!   4  i32  name_offset  = 20
//!   8  i32  vector_length
//!  12  i8   data_type     'J' 长整 / 'I' int / 'B' 字节（字符串也用它）
//!  13  i8   flags         bit0 = supported
//!  14  i8   data_units    1=none 2=bytes 3=ticks 4=events 5=string 6=hertz
//!  15  i8   data_var      1=constant 2=monotonic 3=variable
//!  16  i32  data_offset   （从本 entry 起算）
//!  20  ...  名字（NUL 结尾）
//! ```
//!
//! 关键点：**值的位置由 `data_offset` 决定，读者只信这个字段**（已从
//! `sun.management.counter.perf.PerfDataEntry$EntryFieldOffset` 反汇编确认：读取端
//! 用的就是 0/4/8/12/13/14/15/16 这组偏移）。所以这里统一把值对齐到 8 字节，
//! 既简单又满足任何对齐假设。
//!
//! # 为什么值是"原地覆写"而不是重建文件
//!
//! 客户端是 **mmap** 这个文件的。重建（truncate + 重写）会让并发读者看到半个文件。
//! 所以布局在创建时定死，刷新只覆写值区 —— 读者最坏看到"新旧值混在一起"，
//! 但结构永远合法。HotSpot 也是这么做的。

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const MAGIC: u32 = 0xcafe_c0c0;
/// 磁盘上的魔数**字节序列**。
///
/// 坑：HotSpot 把 magic 存成大端（真文件前 4 字节是 `ca fe c0 c0`），而其余字段全是
/// 小端；读端用 `ByteBuffer.getInt(0)`（默认大端）校验，所以按小端写会得到
/// `Bad Magic: c0c0feca`。
pub const MAGIC_BYTES: [u8; 4] = [0xca, 0xfe, 0xc0, 0xc0];
pub const PROLOGUE_LEN: usize = 32;
pub const ENTRY_HDR_LEN: usize = 20;
/// 文件最小长度，与 HotSpot 一致（64 KiB）。
pub const MIN_FILE_SIZE: usize = 64 * 1024;

/// 计量单位。数值就是文件里的 `data_units`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Units {
    None = 1,
    Bytes = 2,
    Ticks = 3,
    Events = 4,
    Str = 5,
    Hertz = 6,
}

/// 变化性。数值就是文件里的 `data_var`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Var {
    Constant = 1,
    Monotonic = 2,
    Variable = 3,
}

/// 数据类型。数值就是文件里的 `data_type`（ASCII）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PType {
    Bool,
    Char,
    Float,
    Double,
    Byte,
    Short,
    Int,
    Long,
}

impl PType {
    pub fn code(self) -> u8 {
        (match self {
            PType::Bool => b'Z',
            PType::Char => b'C',
            PType::Float => b'F',
            PType::Double => b'D',
            PType::Byte => b'B',
            PType::Short => b'S',
            PType::Int => b'I',
            PType::Long => b'J',
        }) as u8
    }
    pub fn from_code(c: u8) -> Option<PType> {
        Some(match c {
            b'Z' => PType::Bool,
            b'C' => PType::Char,
            b'F' => PType::Float,
            b'D' => PType::Double,
            b'B' => PType::Byte,
            b'S' => PType::Short,
            b'I' => PType::Int,
            b'J' => PType::Long,
            _ => return None,
        })
    }
}

/// 一个计数器的值。
#[derive(Debug, Clone, PartialEq)]
pub enum PerfValue {
    Long(i64),
    Int(i32),
    Short(i16),
    Byte(i8),
    Bool(bool),
    Char(u8),
    Float(f32),
    Double(f64),
    Str(String),
}

impl PerfValue {
    pub fn ty(&self) -> PType {
        match self {
            PerfValue::Long(_) => PType::Long,
            PerfValue::Int(_) => PType::Int,
            PerfValue::Short(_) => PType::Short,
            PerfValue::Byte(_) => PType::Byte,
            PerfValue::Bool(_) => PType::Bool,
            PerfValue::Char(_) => PType::Char,
            PerfValue::Float(_) => PType::Float,
            PerfValue::Double(_) => PType::Double,
            // 字符串用 'B' + Units::Str，与 HotSpot 一致
            PerfValue::Str(_) => PType::Byte,
        }
    }
    pub fn units_hint(&self) -> Units {
        match self {
            PerfValue::Str(_) => Units::Str,
            _ => Units::None,
        }
    }
    /// 编码后的固定字节数（字符串返回其长度 + 1）。
    fn encoded_len(&self) -> usize {
        match self {
            PerfValue::Long(_) | PerfValue::Double(_) => 8,
            PerfValue::Int(_) | PerfValue::Float(_) => 4,
            PerfValue::Short(_) => 2,
            PerfValue::Byte(_) | PerfValue::Bool(_) | PerfValue::Char(_) => 1,
            PerfValue::Str(s) => s.as_bytes().len() + 1,
        }
    }
    fn encode_into(&self, dst: &mut [u8], cap: usize) {
        for b in dst.iter_mut() {
            *b = 0;
        }
        match self {
            PerfValue::Long(v) => dst[..8].copy_from_slice(&v.to_le_bytes()),
            PerfValue::Double(v) => dst[..8].copy_from_slice(&v.to_le_bytes()),
            PerfValue::Int(v) => dst[..4].copy_from_slice(&v.to_le_bytes()),
            PerfValue::Float(v) => dst[..4].copy_from_slice(&v.to_le_bytes()),
            PerfValue::Short(v) => dst[..2].copy_from_slice(&v.to_le_bytes()),
            PerfValue::Byte(v) => dst[0] = *v as u8,
            PerfValue::Bool(v) => dst[0] = *v as u8,
            PerfValue::Char(v) => dst[0] = *v,
            PerfValue::Str(s) => {
                let b = s.as_bytes();
                let n = b.len().min(cap.saturating_sub(1));
                dst[..n].copy_from_slice(&b[..n]);
                // cap-1 处的 0 已经是 NUL 结束符
            }
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        Some(match self {
            PerfValue::Long(v) => *v,
            PerfValue::Int(v) => *v as i64,
            PerfValue::Short(v) => *v as i64,
            PerfValue::Byte(v) => *v as i64,
            PerfValue::Bool(v) => *v as i64,
            PerfValue::Char(v) => *v as i64,
            PerfValue::Float(v) => *v as i64,
            PerfValue::Double(v) => *v as i64,
            PerfValue::Str(_) => return None,
        })
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            PerfValue::Float(v) => Some(*v as f64),
            PerfValue::Double(v) => Some(*v),
            PerfValue::Str(_) => None,
            other => other.as_i64().map(|v| v as f64),
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            PerfValue::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// 一个待写入的计数器。
#[derive(Debug, Clone)]
pub struct Sample {
    pub name: String,
    pub value: PerfValue,
    pub units: Units,
    pub var: Var,
    pub supported: bool,
}

impl Sample {
    pub fn long(name: &str, v: i64, units: Units, var: Var) -> Sample {
        Sample { name: name.to_string(), value: PerfValue::Long(v), units, var, supported: true }
    }
    pub fn string(name: &str, v: impl Into<String>, var: Var) -> Sample {
        Sample {
            name: name.to_string(),
            value: PerfValue::Str(v.into()),
            units: Units::Str,
            var,
            supported: true,
        }
    }
}

#[derive(Debug, Clone)]
struct Slot {
    name: String,
    ty: PType,
    units: Units,
    var: Var,
    supported: bool,
    /// `vector_length`：字符串 = 槽容量（≥ 长度+1），标量 = 0。
    ///
    /// 必须是 >0，否则读端报
    /// `Unexpected type code encountered: ... type_code = byte (0x42)` ——
    /// 读端只认 "字节数组"（vl>0）这一种 'B'。
    vec_len: i32,
    /// 值在**整个文件**中的绝对偏移
    data_off: usize,
    /// 值的字节容量（字符串按这个上限截断）
    cap: usize,
    entry_off: usize,
    entry_len: usize,
}

fn align8(n: usize) -> usize {
    (n + 7) & !7usize
}

/// 打开/创建后的 perfdata 文件句柄。
pub struct PerfFile {
    path: PathBuf,
    slots: Vec<Slot>,
    index: BTreeMap<String, usize>,
    file_size: usize,
    created_ms: u64,
    file: File,
    /// Windows 上真工具靠这个具名共享内存读数据，不能提前 drop
    _shm: crate::sys::NamedMapping,
}

impl PerfFile {
    /// 按第一份样本确定布局并写出文件。
    ///
    /// 之后只能 [`Self::update`] —— 名字/类型/容量都固定了。
    pub fn create(path: &Path, samples: &[Sample], created_ms: u64) -> std::io::Result<PerfFile> {
        let mut slots: Vec<Slot> = Vec::with_capacity(samples.len());
        let mut index = BTreeMap::new();
        let mut off = PROLOGUE_LEN;
        for s in samples.iter() {
            if index.contains_key(&s.name) {
                // 重名：只保留第一条（HotSpot 用 TreeMap，重名会互相覆盖）
                continue;
            }
            let name_bytes = s.name.as_bytes().len() + 1;
            let data_off_rel = align8(ENTRY_HDR_LEN + name_bytes);
            // 字符串留 25% + 64 字节的余量：同一个进程里 classpath 之类长度会变
            let cap = match &s.value {
                PerfValue::Str(v) => {
                    let base = v.as_bytes().len() + 1;
                    align8(base + base / 4 + 64)
                }
                other => other.encoded_len(),
            };
            let cap = cap.max(8);
            let entry_len = align8(data_off_rel + cap);
            let vec_len = match &s.value {
                PerfValue::Str(_) => cap as i32,
                _ => 0,
            };
            index.insert(s.name.clone(), slots.len());
            slots.push(Slot {
                vec_len,
                name: s.name.clone(),
                ty: s.value.ty(),
                units: s.units,
                var: s.var,
                supported: s.supported && !matches!(s.value, PerfValue::Str(_)),
                data_off: off + data_off_rel,
                cap,
                entry_off: off,
                entry_len,
            });
            off += entry_len;
        }
        let used = off - PROLOGUE_LEN;
        let file_size = align8(off).max(MIN_FILE_SIZE);

        let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
        file.set_len(file_size as u64)?;
        // Windows: 把同一份数据暴露成 `hsperfdata_<user>_<pid>` 具名共享内存，
        // 否则真 jps/jstat 报 `Could not attach to <pid>`（它们只 OpenFileMapping）。
        let shm_name = crate::sys::perf_shm_name(
            &crate::registry::user_name(),
            path.file_name()
                .and_then(|n| n.to_str())
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0),
        );
        let shm = crate::sys::create_named_mapping(&file, &shm_name, file_size)?;
        let mut pf = PerfFile {
            path: path.to_path_buf(),
            slots,
            index,
            file_size,
            created_ms,
            file,
            _shm: shm,
        };
        pf.write_header(used)?;
        pf.write_entry_headers()?;
        pf.update(samples)?;
        Ok(pf)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn counter_count(&self) -> usize {
        self.slots.len()
    }

    fn write_header(&mut self, used: usize) -> std::io::Result<()> {
        let now = crate::registry::now_ms();
        let mut h = [0u8; PROLOGUE_LEN];
        h[0..4].copy_from_slice(&MAGIC_BYTES);
        h[4] = 1; // little endian
        h[5] = 2; // major
        h[6] = 0; // minor
        h[7] = 1; // accessible
        h[8..12].copy_from_slice(&(used as i32).to_le_bytes());
        h[12..16].copy_from_slice(&0i32.to_le_bytes());
        h[16..24].copy_from_slice(&(now as i64).to_le_bytes());
        h[24..28].copy_from_slice(&(PROLOGUE_LEN as i32).to_le_bytes());
        h[28..32].copy_from_slice(&(self.slots.len() as i32).to_le_bytes());
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&h)
    }

    /// 原地覆写所有值，最后更新 mod_time。
    ///
    /// 样本里没有的名字保持旧值；名字不认识的样本被忽略（不 panic、不打断刷新）。
    pub fn update(&mut self, samples: &[Sample]) -> std::io::Result<()> {
        let mut buf: Vec<u8> = Vec::new();
        // 每个 slot 单独 seek 写，避免整文件读写（64 KiB 也不大，但这样更省）
        for (i, s) in samples.iter().enumerate() {
            let _ = i;
            let slot = match self.index.get(&s.name).and_then(|i| self.slots.get(*i)) {
                Some(s) => s.clone(),
                None => continue,
            };
            if slot.ty != s.value.ty() {
                // 类型变了：忽略（布局已定死，改了会破坏读者）
                continue;
            }
            buf.clear();
            buf.resize(slot.cap, 0u8);
            s.value.encode_into(&mut buf, slot.cap);
            self.file.seek(SeekFrom::Start(slot.data_off as u64))?;
            self.file.write_all(&buf)?;
        }
        // mod_time 单独写，读者据此判断"有新样本"（与 HotSpot 一致：最后更新）
        let now = crate::registry::now_ms() as i64;
        self.file.seek(SeekFrom::Start(16))?;
        self.file.write_all(&now.to_le_bytes())?;
        self.file.flush()
    }

    /// 一次性把所有结构（名字/类型/units）也重写 —— 只在创建时用内部调用。
    fn write_entry_headers(&mut self) -> std::io::Result<()> {
        for slot in self.slots.clone() {
            let mut hdr = [0u8; ENTRY_HDR_LEN];
            hdr[0..4].copy_from_slice(&(slot.entry_len as i32).to_le_bytes());
            hdr[4..8].copy_from_slice(&(ENTRY_HDR_LEN as i32).to_le_bytes());
            hdr[8..12].copy_from_slice(&slot.vec_len.to_le_bytes());
            hdr[12] = slot.ty.code();
            hdr[13] = if slot.supported { 1 } else { 0 };
            hdr[14] = slot.units as u8;
            hdr[15] = slot.var as u8;
            let rel = slot.data_off - slot.entry_off;
            hdr[16..20].copy_from_slice(&(rel as i32).to_le_bytes());
            self.file.seek(SeekFrom::Start(slot.entry_off as u64))?;
            self.file.write_all(&hdr)?;
            self.file.seek(SeekFrom::Start((slot.entry_off + ENTRY_HDR_LEN) as u64))?;
            self.file.write_all(slot.name.as_bytes())?;
            self.file.write_all(&[0u8])?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 读
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum PerfError {
    Io(std::io::Error),
    NotFound,
    BadMagic(u32),
    BadVersion(u8, u8),
    Truncated,
    NotAccessible,
}

impl std::fmt::Display for PerfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PerfError::Io(e) => write!(f, "{}", e),
            PerfError::NotFound => write!(f, "perfdata 文件不存在"),
            PerfError::BadMagic(m) => write!(f, "perfdata 魔数不符 (0x{:08x})", m),
            PerfError::BadVersion(a, b) => write!(f, "perfdata 版本不符 ({}.{})", a, b),
            PerfError::Truncated => write!(f, "perfdata 文件被截断"),
            PerfError::NotAccessible => write!(f, "perfdata 标记为不可访问"),
        }
    }
}

impl From<std::io::Error> for PerfError {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::NotFound {
            PerfError::NotFound
        } else {
            PerfError::Io(e)
        }
    }
}

/// 一份 perfdata 快照（名字 → 值）。顺序是 BTreeMap 的字典序，
/// 正好就是 `jcmd <pid> PerfCounter.print` 的输出顺序。
#[derive(Debug, Clone, Default)]
pub struct PerfData {
    pub counters: BTreeMap<String, PerfValue>,
    pub units: BTreeMap<String, Units>,
    pub var: BTreeMap<String, Var>,
    pub supported: BTreeMap<String, bool>,
    pub mod_time_ms: u64,
}

impl PerfData {
    pub fn get_i64(&self, name: &str) -> Option<i64> {
        self.counters.get(name).and_then(|v| v.as_i64())
    }
    pub fn get_f64(&self, name: &str) -> Option<f64> {
        self.counters.get(name).and_then(|v| v.as_f64())
    }
    pub fn get_str(&self, name: &str) -> Option<&str> {
        self.counters.get(name).and_then(|v| v.as_str())
    }
    /// 取计数器并换算成纳秒。缺失返回 `None`。
    pub fn ticks_ns(&self, name: &str) -> Option<i64> {
        let v = self.get_i64(name)?;
        let hz = self.get_i64("sun.os.hrt.frequency").unwrap_or(1_000_000_000);
        if hz <= 0 {
            return Some(v);
        }
        // 我们的 VM 直接写纳秒，hrt.frequency = 1e9
        Some((v as i128 * 1_000_000_000i128 / hz as i128) as i64)
    }
    pub fn len(&self) -> usize {
        self.counters.len()
    }
    pub fn is_empty(&self) -> bool {
        self.counters.is_empty()
    }
}

/// 判断一个文件是不是合法的 perfdata（`jps` 用它过滤目录里别的东西）。
pub fn looks_like_perf_file(path: &Path) -> bool {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut b = [0u8; 8];
    if f.read_exact(&mut b).is_err() {
        return false;
    }
    [b[0], b[1], b[2], b[3]] == MAGIC_BYTES
}

/// 读一份 perfdata。
pub fn read(path: &Path) -> Result<PerfData, PerfError> {
    let mut f = File::open(path)?;
    let mut data = Vec::new();
    f.read_to_end(&mut data)?;
    parse(&data)
}

/// 从内存字节解析（单元测试与 jstatd 复用）。
pub fn parse(d: &[u8]) -> Result<PerfData, PerfError> {
    if d.len() < PROLOGUE_LEN {
        return Err(PerfError::Truncated);
    }
    if d[0..4] != MAGIC_BYTES {
        return Err(PerfError::BadMagic(u32::from_be_bytes([d[0], d[1], d[2], d[3]])));
    }
    let major = d[5];
    let minor = d[6];
    if major != 2 {
        return Err(PerfError::BadVersion(major, minor));
    }
    if d[7] == 0 {
        return Err(PerfError::NotAccessible);
    }
    let mod_time = i64::from_le_bytes([d[16], d[17], d[18], d[19], d[20], d[21], d[22], d[23]]);
    let entry_off = i32::from_le_bytes([d[24], d[25], d[26], d[27]]);
    let n_entries = i32::from_le_bytes([d[28], d[29], d[30], d[31]]);
    if entry_off < 0 || n_entries < 0 {
        return Err(PerfError::Truncated);
    }
    let mut out = PerfData { mod_time_ms: mod_time.max(0) as u64, ..Default::default() };
    let mut off = entry_off as usize;
    for _ in 0..n_entries {
        if off + ENTRY_HDR_LEN > d.len() {
            return Err(PerfError::Truncated);
        }
        let entry_len = i32::from_le_bytes([d[off], d[off + 1], d[off + 2], d[off + 3]]);
        let name_off = i32::from_le_bytes([d[off + 4], d[off + 5], d[off + 6], d[off + 7]]);
        let vlen = i32::from_le_bytes([d[off + 8], d[off + 9], d[off + 10], d[off + 11]]);
        let ty = PType::from_code(d[off + 12]);
        let flags = d[off + 13];
        let units = d[off + 14];
        let var = d[off + 15];
        let data_off = i32::from_le_bytes([d[off + 16], d[off + 17], d[off + 18], d[off + 19]]);
        if entry_len <= 0 || name_off < 0 || data_off < 0 {
            return Err(PerfError::Truncated);
        }
        let nstart = off + name_off as usize;
        if nstart >= d.len() {
            return Err(PerfError::Truncated);
        }
        let name_end = match d[nstart..].iter().position(|b| *b == 0) {
            Some(p) => nstart + p,
            None => return Err(PerfError::Truncated),
        };
        let name = String::from_utf8_lossy(&d[nstart..name_end]).into_owned();
        let vstart = off + data_off as usize;
        // 关键：`'B'` + `units==STRING` 是字符串（HotSpot 的约定），不是字节。
        // 判定必须看 units，不能只看类型编码 —— 否则 `sun.rt.javaCommand` 会读成一个 Byte。
        let value = match ty {
            None => None,
            Some(PType::Byte) if units == Units::Str as u8 => {
                if vstart >= d.len() {
                    None
                } else {
                    let end = d[vstart..]
                        .iter()
                        .position(|b| *b == 0)
                        .map(|p| vstart + p)
                        .unwrap_or(d.len());
                    Some(PerfValue::Str(String::from_utf8_lossy(&d[vstart..end]).into_owned()))
                }
            }
            Some(t) => decode(t, d, vstart, vlen),
        };
        if let Some(v) = value {
            out.units.insert(name.clone(), units_from_u8(units));
            out.var.insert(name.clone(), var_from_u8(var));
            out.supported.insert(name.clone(), flags & 1 != 0);
            out.counters.insert(name, v);
        }
        off += entry_len as usize;
    }
    Ok(out)
}

fn units_from_u8(v: u8) -> Units {
    match v {
        2 => Units::Bytes,
        3 => Units::Ticks,
        4 => Units::Events,
        5 => Units::Str,
        6 => Units::Hertz,
        _ => Units::None,
    }
}

fn var_from_u8(v: u8) -> Var {
    match v {
        1 => Var::Constant,
        2 => Var::Monotonic,
        _ => Var::Variable,
    }
}

fn decode(t: PType, d: &[u8], at: usize, vector_len: i32) -> Option<PerfValue> {
    let need = |n: usize| -> Option<&[u8]> {
        if at + n <= d.len() {
            Some(&d[at..at + n])
        } else {
            None
        }
    };
    Some(match t {
        PType::Long => {
            let b = need(8)?;
            PerfValue::Long(i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
        }
        PType::Double => {
            let b = need(8)?;
            PerfValue::Double(f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
        }
        PType::Int => {
            let b = need(4)?;
            PerfValue::Int(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        }
        PType::Float => {
            let b = need(4)?;
            PerfValue::Float(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        }
        PType::Short => {
            let b = need(2)?;
            PerfValue::Short(i16::from_le_bytes([b[0], b[1]]))
        }
        PType::Byte => {
            let b = need(1)?;
            if vector_len > 1 {
                PerfValue::Byte(b[0] as i8)
            } else {
                // 单字节：既可能是数值，也可能是 "B"+"string" 的字符串
                PerfValue::Byte(b[0] as i8)
            }
        }
        PType::Bool => {
            let b = need(1)?;
            PerfValue::Bool(b[0] != 0)
        }
        PType::Char => {
            let b = need(1)?;
            PerfValue::Char(b[0])
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples() -> Vec<Sample> {
        vec![
            Sample::long("java.threads.live", 7, Units::None, Var::Variable),
            Sample::long("sun.gc.collector.0.invocations", 42, Units::Events, Var::Monotonic),
            Sample::string("sun.rt.javaCommand", "Main -X 1", Var::Constant),
        ]
    }

    #[test]
    fn create_and_read_back() {
        let dir = std::env::temp_dir().join("vmless-attach-perf-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("perf-1");
        let mut pf = PerfFile::create(&path, &samples(), 123).unwrap();

        let pd = read(&path).unwrap();
        assert_eq!(pd.get_i64("java.threads.live"), Some(7));
        assert_eq!(pd.get_i64("sun.gc.collector.0.invocations"), Some(42));
        assert_eq!(pd.get_str("sun.rt.javaCommand"), Some("Main -X 1"));
        assert_eq!(pd.units.get("sun.gc.collector.0.invocations"), Some(&Units::Events));

        // 原地更新
        let mut s2 = samples();
        s2[0] = Sample::long("java.threads.live", 99, Units::None, Var::Variable);
        pf.update(&s2).unwrap();
        let pd2 = read(&path).unwrap();
        assert_eq!(pd2.get_i64("java.threads.live"), Some(99));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut d = vec![0u8; 64];
        d[0..4].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        match parse(&d) {
            Err(PerfError::BadMagic(_)) => {}
            _ => panic!("expected BadMagic"),
        }
    }

    #[test]
    fn rejects_short_file() {
        match parse(&[0u8; 4]) {
            Err(PerfError::Truncated) => {}
            _ => panic!("expected Truncated"),
        }
    }

    #[test]
    fn string_truncation_is_safe() {
        let dir = std::env::temp_dir().join("vmless-attach-perf-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("perf-2");
        let mut pf = PerfFile::create(&path, &samples(), 0).unwrap();
        let long: String = "x".repeat(4000);
        let mut s2 = samples();
        s2[2] = Sample::string("sun.rt.javaCommand", long.clone(), Var::Constant);
        pf.update(&s2).unwrap();
        let pd = read(&path).unwrap();
        let got = pd.get_str("sun.rt.javaCommand").unwrap();
        assert!(got.len() < 4000 && !got.is_empty(), "len={}", got.len());
        assert!(long.starts_with(got));
        let _ = std::fs::remove_file(&path);
    }
}
