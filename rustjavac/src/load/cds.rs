//! CDS 式类表缓存：把「已经取到过的类」按名字存一份到磁盘，下次启动直接按名取字节 / 取引用。
//!
//! ## 为什么要它
//!
//! 冷启动的 462 ms 里，`find`（5421 次文件探测 + 读）94 ms、常量池闭包展开 30 ms ——
//! 而这两件事的结果**每次都一模一样**：java.base 的类集合是固定的，`A 引用 B` 也是固定的。
//! 把它们缓存到磁盘，第二次起就只剩「按名字取出字节 + 解析」。
//!
//! ## 存什么（**只存原始字节，不存解析后的结构**）
//!
//! 直觉上「存解析后的 ClassFile、反序列化比 parse 快」是错的：`ClassFile` 的体积是
//! class 字节的 2.5 倍（5421 个类 = 27 MB 字节 → 60 MB 结构），写出去要 60 MB、读回来还要
//! 逐字段重建 `String`/`Vec`，跟重新 parse 一遍是同一量级的工作量。**存字节**既省磁盘又省内存，
//! 而且字节是权威的 —— 反序列化漏掉一个字段就是静默语义错（这一类 bug 只有端到端比对才能抓）。
//!
//! 除此之外缓存**每个类引用到的类名**（`refs`）：常量池闭包展开时不必再扫一遍常量池
//! （762,850 个槽 = 24 MB 的随机访存，实测 30 ms）。
//!
//! ## 失效与安全
//!
//! 指纹 = VM 版本 + 格式版本 + 每个来源的（路径, mtime, 字节数）。指纹不符 → 整个文件重建。
//! 文件坏了（截断 / 魔数不对 / 版本不符 / 读失败）→ `open` 返回 `None`，**静默走老路径**，
//! 绝不因为缓存坏了而报错。
//!
//! **已知边界**：目录树来源（`VMLESS_JDK=<解包目录>`）的「就地覆写某个 .class」不会改变
//! 目录 mtime，指纹看不到 → 改完 class 请设 `VMLESS_CDS=0` 或删掉缓存文件。
//! （jimage 来源没有这个问题：它是单个文件，size/mtime 直接就是内容指纹。）
//!
//! ## 并发
//!
//! 建缓存：写 `<路径>.tmp<pid>`，写完 `rename` 覆盖 —— 原子替换，多个进程同时建也只是
//! 各自写各自的临时文件，最后一个赢。读：只读，文件句柄 + 定位读（`seek_read`），
//! 多线程并行取字节不需要锁。

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const FMT: u32 = 2;
const MAGIC: &[u8; 8] = b"VMLSCDS1";
/// 头部固定 40 字节（指纹字符串紧跟其后）
const HDR: usize = 40;
/// 一条索引记录 40 字节，小端
const ENT: usize = 40;

/// 缓存语义的「纪元」：**改了类加载 / 闭包的规则就手动把它 +1**。
///
/// 光靠 VM 版本号不够 —— 版本号是本 crate 的版本，而闭包规则（过滤哪些引用、
/// 哪些算 `java.*`）可以不变版本号就改；纪元变了指纹就变，旧缓存自动作废。
pub const VM_EPOCH: &str = concat!(env!("CARGO_PKG_VERSION"), "/1");

/// 一条类索引。
#[derive(Clone, Copy, Default)]
pub struct Ent {
    pub hash: u64,
    pub name_off: u32,
    pub name_len: u32,
    pub data_off: u64,
    pub data_len: u32,
    pub refs_off: u32,
    pub refs_cnt: u32,
}

/// 打开着的类表缓存。
impl std::fmt::Debug for Cds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cds({} 个类, {} 条引用)", self.ents.len(), self.refs.len())
    }
}

pub struct Cds {
    /// 建缓存时的指纹（诊断用）
    pub fp: String,
    file: File,
    /// 按 hash 升序
    pub ents: Vec<Ent>,
    names: Vec<u8>,
    refs: Vec<u32>,
}

/// 名字的散列：FNV-1a 64 + 一次尾混合。只用它排序/二分，命中后仍然逐字节比对名字，
/// 所以碰撞只会多一次比较，不会串类。
#[inline]
pub fn hash_name(s: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in s {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= h >> 29;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^ (h >> 32)
}

fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn rd_u64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

fn eof() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "cds 越过文件尾")
}

/// 定位读：不移动文件游标，多线程可以同时读同一个 `Cds`。
///
/// **必须用 `FILE_FLAG_OVERLAPPED` 打开**：Windows 的 `ReadFile` 对**同步句柄**会无视
/// `OVERLAPPED.Offset`、直接从当前文件指针读 —— 也就是 `seek_read` 会静默退化成「顺序读」。
/// 症状极具误导性：游标停在数据区起点，于是「索引」读出来全是 class 字节（`ca fe ba be`）。
#[cfg(windows)]
fn open_read(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OVERLAPPED)
        .open(path)
}

#[cfg(not(windows))]
fn open_read(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

#[cfg(windows)]
fn pread(f: &File, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
    use std::os::windows::fs::FileExt;
    f.seek_read(buf, off)
}
#[cfg(not(windows))]
fn pread(f: &File, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;
    f.read_at(buf, off)
}

fn read_at(f: &File, buf: &mut [u8], off: u64) -> std::io::Result<()> {
    let mut done = 0usize;
    while done < buf.len() {
        let got = pread(f, &mut buf[done..], off + done as u64);
        match got {
            Ok(0) => return Err(eof()),
            Ok(n) => done += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

impl Cds {
    /// 缓存文件放在哪：`%TEMP%/vmless-cds/<指纹散列>.bin`
    pub fn path_for(fp: &str) -> Option<PathBuf> {
        let dir = std::env::temp_dir().join("vmless-cds");
        let _ = std::fs::create_dir_all(&dir);
        Some(dir.join(format!("{:016x}.bin", hash_name(fp.as_bytes()))))
    }

    /// 读一个已经建好的缓存。任何异常（不存在 / 坏 / 版本不符 / 指纹不符）都返回 `None`。
    pub fn open(path: &Path, want_fp: &str) -> Option<Cds> {
        let f = open_read(path).ok()?;
        let len = f.metadata().ok()?.len();
        let mut hdr = [0u8; HDR];
        // 头部也用定位读：句柄是 OVERLAPPED 打开的，普通 read 在这里不可靠
        read_at(&f, &mut hdr, 0).ok()?;
        if &hdr[0..8] != MAGIC {
            return None;
        }
        if rd_u32(&hdr, 8) != FMT {
            return None;
        }
        let n_ent = rd_u32(&hdr, 12) as usize;
        let n_refs = rd_u32(&hdr, 16) as usize;
        let names_len = rd_u32(&hdr, 20) as usize;
        let _data_off = rd_u64(&hdr, 24);
        let index_off = rd_u64(&hdr, 32);
        let mut fp_len_b = [0u8; 4];
        read_at(&f, &mut fp_len_b, HDR as u64).ok()?;
        let fp_len = u32::from_le_bytes(fp_len_b) as usize;
        if fp_len > 1 << 20 {
            return None;
        }
        let mut fp = vec![0u8; fp_len];
        read_at(&f, &mut fp, HDR as u64 + 4).ok()?;
        let fp = String::from_utf8(fp).ok()?;
        // 指纹必须逐字相同 —— 不同 JDK / 不同 VM 版本 / 不同格式一律不认
        if fp != want_fp {
            return None;
        }
        // 合理上界：坏文件不能让我们一次分配几个 GB
        if n_ent > 500_000
            || n_refs > 20_000_000
            || names_len > 64 << 20
            || _data_off > len
            || index_off > len
        {
            return None;
        }
        let ent_off = index_off;
        let mut ent_buf = vec![0u8; n_ent * ENT];
        read_at(&f, &mut ent_buf, ent_off).ok()?;
        let names_off = ent_off + (n_ent * ENT) as u64;
        let mut names = vec![0u8; names_len];
        read_at(&f, &mut names, names_off).ok()?;
        let refs_off = names_off + names_len as u64;
        let mut refs_buf = vec![0u8; n_refs * 4];
        read_at(&f, &mut refs_buf, refs_off).ok()?;
        let mut ents = Vec::with_capacity(n_ent);
        for i in 0..n_ent {
            let b = &ent_buf[i * ENT..(i + 1) * ENT];
            ents.push(Ent {
                hash: rd_u64(b, 0),
                name_off: rd_u32(b, 8),
                name_len: rd_u32(b, 12),
                data_off: rd_u64(b, 16),
                data_len: rd_u32(b, 24),
                refs_off: rd_u32(b, 28),
                refs_cnt: rd_u32(b, 32),
            });
        }
        let mut refs = Vec::with_capacity(n_refs);
        for i in 0..n_refs {
            refs.push(rd_u32(&refs_buf, i * 4));
        }
        Some(Cds { fp, file: f, ents, names, refs })
    }

    /// 名字 → 索引（同一个 hash 有多条时按名字逐字节比对）
    pub fn idx(&self, name: &str) -> Option<usize> {
        let h = hash_name(name.as_bytes());
        let mut lo = self.ents.partition_point(|e| e.hash < h);
        while lo < self.ents.len() && self.ents[lo].hash == h {
            if self.name_of(lo) == name {
                return Some(lo);
            }
            lo += 1;
        }
        None
    }

    #[inline]
    pub fn name_of(&self, i: usize) -> &str {
        let e = &self.ents[i];
        let s = &self.names[e.name_off as usize..(e.name_off + e.name_len) as usize];
        // 类名只可能是 ASCII（JVMS 4.2.1），正常路径下不会失败
        std::str::from_utf8(s).unwrap_or("")
    }

    /// 第 i 条引用到的名字（索引 → 名字），用来免掉常量池扫描
    #[inline]
    /// 第 i 个类引用到的名字（名字在 names 池里的 (偏移, 长度) 成对存 —— 引用可能指向
    /// **没有进缓存**的类（用户自己的类、缺类），所以不能只存条目下标）
    pub fn ref_ids(&self, i: usize) -> impl Iterator<Item = (u32, &str)> + '_ {
        let e = &self.ents[i];
        let s = e.refs_off as usize;
        (0..e.refs_cnt as usize).map(move |k| {
            let id = self.refs[s + k * 3];
            let off = self.refs[s + k * 3 + 1] as usize;
            let len = self.refs[s + k * 3 + 2] as usize;
            (id, std::str::from_utf8(&self.names[off..off + len]).unwrap_or(""))
        })
    }

    /// 只要名字（不需要条目下标的调用方用）
    pub fn ref_names(&self, i: usize) -> impl Iterator<Item = &str> + '_ {
        self.ref_ids(i).map(|(_, n)| n)
    }

    /// 第 i 个类的原始字节
    pub fn bytes(&self, i: usize) -> Option<Vec<u8>> {
        let e = &self.ents[i];
        if e.data_len == 0 {
            return None;
        }
        let mut buf = vec![0u8; e.data_len as usize];
        read_at(&self.file, &mut buf, e.data_off).ok()?;
        Some(buf)
    }

    /// 批量取字节（并行定位读）。`idxs` 里 `None` 表示这一格不在缓存里。
    pub fn bytes_many(&self, idxs: &[Option<usize>]) -> Vec<Option<Vec<u8>>> {
        let mut out: Vec<Option<Vec<u8>>> = (0..idxs.len()).map(|_| None).collect();
        let jobs: Vec<(usize, usize)> = idxs
            .iter()
            .enumerate()
            .filter_map(|(k, o)| o.map(|i| (k, i)))
            .collect();
        if jobs.is_empty() {
            return out;
        }
        let hw = std::thread::available_parallelism().map(|v| v.get()).unwrap_or(1).min(16);
        let threads = if jobs.len() < 64 { 1 } else { (jobs.len() / 64).min(hw).max(1) };
        let chunk = jobs.len().div_ceil(threads.max(1)).max(1);
        std::thread::scope(|s| {
            let mut hs = Vec::new();
            for part in jobs.chunks(chunk) {
                hs.push(s.spawn(move || {
                    part.iter().map(|&(k, i)| (k, self.bytes(i))).collect::<Vec<_>>()
                }));
            }
            for h in hs {
                for (k, b) in h.join().unwrap_or_default() {
                    out[k] = b;
                }
            }
        });
        out
    }
}

/// 建缓存用的施工器：边跑边把类字节写进临时文件（**不在内存里攒** —— 5421 个类 27 MB，
/// 攒起来就是启动峰值 RSS 上多 27 MB）。
pub struct Writer {
    /// 完工时 `take()` 掉再关句柄 —— Windows 上句柄没关的话 `rename` 会「拒绝访问」
    file: Option<File>,
    tmp: PathBuf,
    dst: PathBuf,
    fp: String,
    ents: Vec<Ent>,
    names: Vec<u8>,
    name_pos: std::collections::HashMap<String, u32>,
    refs: Vec<u32>,
    data_pos: u64,
    seen: HashSet<String>,
    ok: bool,
}

impl Writer {
    /// 打开一个施工器。目标文件已存在（旧的/别人的）也没关系：最后 rename 覆盖。
    pub fn new(fp: &str) -> Option<Writer> {
        let dst = Cds::path_for(fp)?;
        let tmp = dst.with_extension(format!("tmp{}", std::process::id()));
        let mut file = File::create(&tmp).ok()?;
        // 先写一个占位头，收工时 seek 回 0 重写（这样类字节可以直接顺序写出）。
        // 占位区必须**连指纹一起**让出来：类字节从 `HDR + 4 + fp.len()` 之后开始。
        let head = HDR + 4 + fp.len();
        file.write_all(&vec![0u8; head]).ok()?;
        Some(Writer {
            file: Some(file),
            tmp,
            dst,
            fp: fp.to_string(),
            ents: Vec::new(),
            names: Vec::new(),
            name_pos: std::collections::HashMap::new(),
            refs: Vec::new(),
            data_pos: (HDR + 4 + fp.len()) as u64,
            seen: HashSet::new(),
            ok: true,
        })
    }

    /// 已经收进来的类数（诊断用）
    pub fn len(&self) -> usize {
        self.ents.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ents.is_empty()
    }

    fn intern(&mut self, name: &str) -> u32 {
        if let Some(&p) = self.name_pos.get(name) {
            return p;
        }
        let p = self.names.len() as u32;
        self.names.extend_from_slice(name.as_bytes());
        self.name_pos.insert(name.to_string(), p);
        p
    }

    /// 追加一个类（类名 + 原始字节 + 引用类名）。重复类名忽略。
    pub fn add(&mut self, name: &str, bytes: &[u8], refs: &[String]) -> bool {
        if bytes.is_empty() || !self.seen.insert(name.to_string()) {
            return false;
        }
        let data_off = self.data_pos;
        let pad = (4 - (bytes.len() % 4)) % 4;
        {
            let Some(f) = self.file.as_mut() else { return false };
            if f.write_all(bytes).is_err() {
                self.ok = false;
                return false;
            }
            if pad > 0 && f.write_all(&[0u8; 4][..pad]).is_err() {
                self.ok = false;
                return false;
            }
        }
        self.data_pos += (bytes.len() + pad) as u64;
        let name_off = self.intern(name);
        let name_len = name.len() as u32;
        let refs_off = self.refs.len() as u32;
        let mut cnt = 0u32;
        for r in refs {
            if r.is_empty() || r.starts_with('[') || r.contains('<') || r.contains(';') {
                continue;
            }
            let off = self.intern(r);
            self.refs.push(off);
            self.refs.push(r.len() as u32);
            cnt += 1;
        }
        self.ents.push(Ent {
            hash: hash_name(name.as_bytes()),
            name_off,
            name_len,
            data_off,
            data_len: bytes.len() as u32,
            refs_off,
            refs_cnt: cnt,
        });
        true
    }

    /// 收工：写索引 + 重写头 + 原子 rename。任何一步失败都只删临时文件、**不报错**。
    pub fn finish(mut self) -> Option<PathBuf> {
        let mut file = self.file.take()?;
        if !self.ok || self.ents.is_empty() {
            return None;
        }
        self.ents.sort_by_key(|e| e.hash);
        // 引用从 (名字偏移, 长度) 变成 (目标条目下标, 名字偏移, 长度)：目标在缓存里的，
        // 启动时连「按名字查一次表」都省掉（59k 次二分查找 ≈ 20 ms）。
        {
            let names = std::mem::take(&mut self.names);
            let ents = &self.ents;
            let lookup = |name: &[u8]| -> u32 {
                let h = hash_name(name);
                let mut lo = ents.partition_point(|e| e.hash < h);
                while lo < ents.len() && ents[lo].hash == h {
                    let e = &ents[lo];
                    if &names[e.name_off as usize..(e.name_off + e.name_len) as usize] == name {
                        return lo as u32;
                    }
                    lo += 1;
                }
                u32::MAX
            };
            let old = std::mem::take(&mut self.refs);
            let mut out: Vec<u32> = Vec::with_capacity(old.len() / 2 * 3);
            for p in old.chunks(2) {
                let (off, len) = (p[0] as usize, p[1] as usize);
                out.push(lookup(&names[off..off + len]));
                out.push(p[0]);
                out.push(p[1]);
            }
            for e in self.ents.iter_mut() {
                e.refs_off = e.refs_off / 2 * 3;
            }
            self.refs = out;
            self.names = names;
        }
        let index_off = self.data_pos;
        let mut all: Vec<u8> = Vec::with_capacity(self.ents.len() * ENT);
        for e in &self.ents {
            all.extend_from_slice(&e.hash.to_le_bytes());
            all.extend_from_slice(&e.name_off.to_le_bytes());
            all.extend_from_slice(&e.name_len.to_le_bytes());
            all.extend_from_slice(&e.data_off.to_le_bytes());
            all.extend_from_slice(&e.data_len.to_le_bytes());
            all.extend_from_slice(&e.refs_off.to_le_bytes());
            all.extend_from_slice(&e.refs_cnt.to_le_bytes());
            all.extend_from_slice(&0u32.to_le_bytes()); // 保留
        }
        all.extend_from_slice(&self.names);
        for r in &self.refs {
            all.extend_from_slice(&r.to_le_bytes());
        }
        let mut hdr = Vec::with_capacity(HDR + 4 + self.fp.len());
        hdr.extend_from_slice(MAGIC);
        hdr.extend_from_slice(&FMT.to_le_bytes());
        hdr.extend_from_slice(&(self.ents.len() as u32).to_le_bytes());
        hdr.extend_from_slice(&(self.refs.len() as u32).to_le_bytes());
        hdr.extend_from_slice(&(self.names.len() as u32).to_le_bytes());
        // data_off（类字节从哪里开始）/ index_off（索引从哪里开始）
        hdr.extend_from_slice(&((HDR + 4 + self.fp.len()) as u64).to_le_bytes());
        hdr.extend_from_slice(&index_off.to_le_bytes());
        hdr.extend_from_slice(&(self.fp.len() as u32).to_le_bytes());
        hdr.extend_from_slice(self.fp.as_bytes());
        if file.seek(SeekFrom::Start(index_off)).is_err() || file.write_all(&all).is_err() {
            return None;
        }
        if file.seek(SeekFrom::Start(0)).is_err() || file.write_all(&hdr).is_err() {
            return None;
        }
        if file.flush().is_err() {
            return None;
        }
        drop(file); // 句柄必须先关，Windows 才允许 rename
        std::fs::rename(&self.tmp, &self.dst).ok()?;
        Some(self.dst.clone())
    }
}

impl Drop for Writer {
    /// 没走完 `finish`（加载出错 / 提前 return）就删掉临时文件，不留垃圾。
    fn drop(&mut self) {
        self.file.take();
        let _ = std::fs::remove_file(&self.tmp);
    }
}

/// 从老缓存里把每个类的字节读出来、重新收进 `w`（重建并集时用）
pub fn copy_entries(old: &Cds, w: &mut Writer) {
    for i in 0..old.ents.len() {
        let Some(b) = old.bytes(i) else { continue };
        let refs: Vec<String> = old.ref_names(i).map(|s| s.to_string()).collect();
        w.add(old.name_of(i), &b, &refs);
    }
}
