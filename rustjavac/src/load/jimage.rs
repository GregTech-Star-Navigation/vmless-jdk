//! jimage 来源：JDK 的 `lib/modules`（打包好的模块镜像）。
//!
//! 格式（小端）：
//! ```text
//! header 28 字节：magic=0xCAFEDADA | version | flags | resource_count
//!                 | table_length | locations_size | strings_size
//! redirect[table_length]  u4   哈希桶 → 表下标（本实现不用，改为线性建表）
//! offsets [table_length]  u4   第 i 条的 location 记录在 locations 里的偏移
//! locations[locations_size]    压缩编码的属性流
//! strings [strings_size]       NUL 结尾的字符串池（模块名/目录/文件名/扩展名）
//! ```
//! location 记录是**位压缩的属性流**（见 `jdk.internal.jimage.ImageLocation`）：
//! ```text
//! 循环：data = u1；data <= 7 结束
//!       kind = data >> 3，len = (data & 7) + 1，值 = 后 len 字节按大端拼的整数
//! kind: 1=module 2=parent 3=base 4=extension 5=offset 6=compressed 7=uncompressed
//! ```
//! 类名 = `/模块/父目录/文件名.扩展名`（父目录与扩展名可空）。
//! 资源数据在 `index_size + offset` 处，长度 = uncompressed；
//! `compressed != 0` 表示该条目用 jlink 的 `--compress` 压缩过，本实现不认，明确报错。

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const MAGIC: u32 = 0xCAFE_DADA;
/// `String::new()` 之类在 strings 池里的偏移 0 是空串，用 0 当"没有这个属性"
const ATTR_MODULE: usize = 1;
const ATTR_PARENT: usize = 2;
const ATTR_BASE: usize = 3;
const ATTR_EXTENSION: usize = 4;
const ATTR_OFFSET: usize = 5;
const ATTR_COMPRESSED: usize = 6;
const ATTR_UNCOMPRESSED: usize = 7;
const ATTR_COUNT: usize = 8;

#[derive(Clone, Copy, Default, Debug)]
struct Loc {
    attr: [u64; ATTR_COUNT],
}

#[derive(Debug)]
pub struct JImageSource {
    path: PathBuf,
    file: File,
    /// 索引区（header 之后到 strings 结束）总长，资源数据的偏移基准
    index_size: u64,
    table_length: usize,
    locations_size: usize,
    strings_size: usize,
    /// 索引区原文：header 之后的部分（redirect/offsets/locations/strings）
    index: Vec<u8>,
    /// 条目名 → location。第一次查找时建（3 万条，几十毫秒），之后不再解。
    entries: RefCell<Option<HashMap<String, Loc>>>,
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn err<T>(msg: impl Into<String>) -> Result<T, String> {
    Err(msg.into())
}

impl JImageSource {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        let mut file = File::open(&path).map_err(|e| format!("打开 {} 失败: {e}", path.display()))?;
        let mut head = [0u8; 28];
        file.read_exact(&mut head)
            .map_err(|e| format!("读 {} 的头部失败: {e}", path.display()))?;
        let magic = le32(&head, 0);
        if magic != MAGIC {
            return err(format!(
                "{} 不是 jimage：magic = {magic:#010x}（应为 {MAGIC:#010x}）",
                path.display()
            ));
        }
        let table_length = le32(&head, 16) as usize;
        let locations_size = le32(&head, 20) as usize;
        let strings_size = le32(&head, 24) as usize;
        let index_size = 28 + table_length * 8 + locations_size + strings_size;
        let body = index_size.saturating_sub(28);
        let mut index = vec![0u8; body];
        file.read_exact(&mut index)
            .map_err(|e| format!("读 {} 的索引失败（可能被截断）: {e}", path.display()))?;
        Ok(JImageSource {
            path,
            file,
            index_size: index_size as u64,
            table_length,
            locations_size,
            strings_size,
            index,
            entries: RefCell::new(None),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 资源条目数（不含 `module-info` 之类的目录条目就是全部条目数）
    pub fn entry_count(&self) -> usize {
        self.table_length
    }

    fn offsets_at(&self) -> usize {
        self.table_length * 4
    }

    fn locations_at(&self) -> usize {
        self.table_length * 8
    }

    fn strings_at(&self) -> usize {
        self.table_length * 8 + self.locations_size
    }

    /// strings 池里偏移 o 开始的 NUL 结尾字符串
    fn string(&self, o: usize) -> String {
        let base = self.strings_at();
        if o >= self.strings_size {
            return String::new();
        }
        let s = base + o;
        let mut e = s;
        while e < base + self.strings_size && self.index[e] != 0 {
            e += 1;
        }
        // 路径里只会出现 ASCII；非 ASCII 名字按 MUTF-8 收，lossy 足够定位
        String::from_utf8_lossy(&self.index[s..e]).into_owned()
    }

    /// 解一条 location 记录（属性流，见文件头注释）
    fn decode(&self, mut p: usize) -> Loc {
        let mut loc = Loc::default();
        loop {
            let Some(&data) = self.index.get(p) else { break };
            p += 1;
            if data <= 7 {
                break; // ATTR_END
            }
            let kind = (data >> 3) as usize;
            let len = (data & 7) as usize + 1;
            let mut v = 0u64;
            for _ in 0..len {
                v = (v << 8) | (self.index.get(p).copied().unwrap_or(0) as u64);
                p += 1;
            }
            if kind < ATTR_COUNT {
                loc.attr[kind] = v;
            }
        }
        loc
    }

    /// 条目全名：`/模块/父目录/文件名.扩展名`
    fn full_name(&self, loc: &Loc) -> String {
        let mut s = String::new();
        let module = if loc.attr[ATTR_MODULE] == 0 {
            String::new()
        } else {
            self.string(loc.attr[ATTR_MODULE] as usize)
        };
        if !module.is_empty() {
            s.push('/');
            s.push_str(&module);
            s.push('/');
        }
        if loc.attr[ATTR_PARENT] != 0 {
            s.push_str(&self.string(loc.attr[ATTR_PARENT] as usize));
            s.push('/');
        }
        s.push_str(&self.string(loc.attr[ATTR_BASE] as usize));
        if loc.attr[ATTR_EXTENSION] != 0 {
            s.push('.');
            s.push_str(&self.string(loc.attr[ATTR_EXTENSION] as usize));
        }
        s
    }

    fn build(&self) -> HashMap<String, Loc> {
        let locs_at = self.locations_at();
        let mut map = HashMap::with_capacity(self.table_length);
        for i in 0..self.table_length {
            let off = le32(&self.index, self.offsets_at() + i * 4) as usize;
            let loc = self.decode(locs_at + off);
            if loc.attr[ATTR_BASE] == 0 {
                continue;
            }
            let full = self.full_name(&loc);
            // 带模块的全名（`/java.base/java/lang/Object.class`）
            map.insert(full.clone(), loc);
            // 不带模块的名字（`java/lang/Object.class`）：java.base 优先，
            // 同名类只会在拆分打包的 JDK 里出现（比如 java.base 与某个模块共用名字）
            if let Some(rest) = full.strip_prefix('/') {
                if let Some(slash) = rest.find('/') {
                    let module = &rest[..slash];
                    let short = &rest[slash + 1..];
                    let prefer = module == "java.base";
                    if prefer || !map.contains_key(short) {
                        map.insert(short.to_string(), loc);
                    }
                }
            }
        }
        map
    }

    fn with_entries<T>(&self, f: impl FnOnce(&HashMap<String, Loc>) -> T) -> T {
        if self.entries.borrow().is_none() {
            let built = self.build();
            *self.entries.borrow_mut() = Some(built);
        }
        let guard = self.entries.borrow();
        f(guard.as_ref().unwrap())
    }

    /// 条目路径（`java/lang/Object.class`，也可以带前导 `/` 和模块名）→ 字节
    pub fn find(&self, entry: &str) -> Result<Option<Vec<u8>>, String> {
        let key = entry.trim_start_matches('/').to_string();
        let loc = self.with_entries(|m| m.get(&key).copied());
        let Some(loc) = loc else { return Ok(None) };
        let comp = loc.attr[ATTR_COMPRESSED];
        let uncomp = loc.attr[ATTR_UNCOMPRESSED];
        if comp != 0 {
            return err(format!(
                "jimage 条目 {entry} 是压缩存储的（compressed={comp}）—— 本实现只支持未压缩条目"
            ));
        }
        if uncomp > 512 * 1024 * 1024 {
            return err(format!("jimage 条目 {entry} 的大小离谱（{uncomp} 字节）"));
        }
        let mut f = &self.file;
        f.seek(SeekFrom::Start(self.index_size + loc.attr[ATTR_OFFSET]))
            .map_err(|e| format!("定位 {entry} 失败: {e}"))?;
        let mut buf = vec![0u8; uncomp as usize];
        f.read_exact(&mut buf)
            .map_err(|e| format!("读 {entry}（{uncomp} 字节）失败: {e}"))?;
        Ok(Some(buf))
    }
}
