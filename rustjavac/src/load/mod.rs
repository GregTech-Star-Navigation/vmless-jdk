//! 统一的类来源抽象：目录树 / `.jar`(zip) / jimage（JDK 的 `lib/modules`）。
//!
//! 对外只有一个入口：`ClassPath::find(内部类名)` → 类文件字节。
//! 名字一律是 JVM 内部形式（`java/lang/Object`），带不带 `.class` 后缀都认。
//! 按 classpath 顺序查找，第一个命中的来源胜出；结果（含未命中）缓存在内存里。
//!
//! 只用 std，不加依赖：zip 的中央目录与 deflate 解压都在 `jar.rs` 里自己实现，
//! jimage 的索引解码在 `jimage.rs` 里按 `jdk.internal.jimage.ImageLocation` 的
//! 位压缩格式解（`kind << 3 | (len-1)`）。

pub mod dir;
pub mod jar;
pub mod jimage;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 一个类来源。三种格式各自独立，`find` 拿的是**条目路径**（`java/lang/Object.class`）。
#[derive(Debug)]
pub enum ClassSource {
    Dir(dir::DirSource),
    Jar(jar::JarSource),
    JImage(jimage::JImageSource),
}

impl ClassSource {
    /// 打开一个来源。按**内容魔数**认类型，不看扩展名：
    /// zip 的 `PK\x03\x04`、jimage 的 `DA DA FE CA`（`0xCAFEDADA` 的小端写法）。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let p = path.as_ref();
        if p.is_dir() {
            return Ok(ClassSource::Dir(dir::DirSource::open(p)?));
        }
        let mut f = std::fs::File::open(p).map_err(|e| format!("打开 {} 失败: {e}", p.display()))?;
        let mut magic = [0u8; 4];
        use std::io::Read;
        let n = f.read(&mut magic).map_err(|e| format!("读 {} 失败: {e}", p.display()))?;
        if n >= 4 {
            if magic == [0xDA, 0xDA, 0xFE, 0xCA] {
                return Ok(ClassSource::JImage(jimage::JImageSource::open(p)?));
            }
            if magic[0] == b'P' && magic[1] == b'K' {
                return Ok(ClassSource::Jar(jar::JarSource::open(p)?));
            }
        }
        // 魔数认不出来时退回按扩展名（空文件/畸形输入更容易看懂报错）
        match p.extension().and_then(|s| s.to_str()).unwrap_or("") {
            "jar" | "zip" => Ok(ClassSource::Jar(jar::JarSource::open(p)?)),
            "jimage" => Ok(ClassSource::JImage(jimage::JImageSource::open(p)?)),
            _ => Err(format!(
                "{} 既不是目录，也不是 jar/zip 或 jimage（魔数 {magic:02x?}）",
                p.display()
            )),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            ClassSource::Dir(_) => "dir",
            ClassSource::Jar(_) => "jar",
            ClassSource::JImage(_) => "jimage",
        }
    }

    /// 条目路径 → 字节。`Ok(None)` = 这里没有这个条目。
    pub fn find(&self, entry: &str) -> Result<Option<Vec<u8>>, String> {
        match self {
            ClassSource::Dir(s) => s.find(entry),
            ClassSource::Jar(s) => s.find(entry),
            ClassSource::JImage(s) => s.find(entry),
        }
    }
}

/// 按顺序排好的类来源列表 = classpath。
#[derive(Debug, Default)]
pub struct ClassPath {
    entries: Vec<(PathBuf, ClassSource)>,
    /// 内部类名 → 字节（未命中记 `None`，避免同一个缺类反复查盘）
    cache: RefCell<HashMap<String, Option<Vec<u8>>>>,
}

/// 内部类名 → 条目路径：`java/lang/Object` / `java/lang/Object.class` → `java/lang/Object.class`
fn to_entry(class: &str) -> String {
    let n = class.trim_start_matches('/');
    if n.ends_with(".class") {
        n.to_string()
    } else {
        format!("{n}.class")
    }
}

impl ClassPath {
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一个来源（目录 / jar / jimage，自动识别）
    pub fn push(&mut self, path: impl AsRef<Path>) -> Result<(), String> {
        let p = path.as_ref();
        let src = ClassSource::open(p)?;
        self.entries.push((p.to_path_buf(), src));
        self.cache.borrow_mut().clear();
        Ok(())
    }

    /// `a.jar:b.jar:dir`（Windows 上 `;` 也认）→ ClassPath
    ///
    /// **Windows 上只能按 `;` 切**：`D:/x/classes` 里的 `:` 是盘符，按 `:` 切会把
    /// 绝对路径切成 `D` + `/x/classes`（`打开 D 失败`）。
    pub fn from_spec(spec: &str) -> Result<Self, String> {
        let mut cp = ClassPath::new();
        let seps: &[char] = if cfg!(windows) { &[';'] } else { &[':', ';'] };
        for part in spec.split(seps) {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            cp.push(part)?;
        }
        if cp.is_empty() {
            return Err(format!("classpath 为空: {spec:?}"));
        }
        Ok(cp)
    }

    /// 把「jimage 解包出来的目录」（`img/java.base/java/lang/Object.class`）当来源：
    /// 每个模块子目录各算一层，根目录本身也算一层（没有模块层的普通目录树直接可用）。
    pub fn push_module_tree(&mut self, root: impl AsRef<Path>) -> Result<(), String> {
        let root = root.as_ref();
        if !root.is_dir() {
            return Err(format!("{} 不是目录", root.display()));
        }
        self.push(root)?;
        let mut mods: Vec<PathBuf> = std::fs::read_dir(root)
            .map_err(|e| format!("读目录 {} 失败: {e}", root.display()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        mods.sort();
        for m in mods {
            self.push(m)?;
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn sources(&self) -> impl Iterator<Item = (&Path, &'static str)> {
        self.entries.iter().map(|(p, s)| (p.as_path(), s.kind()))
    }

    /// 内部类名 → 类文件字节（第一个命中的来源胜出）
    pub fn find(&self, class: &str) -> Result<Option<Vec<u8>>, String> {
        if let Some(hit) = self.cache.borrow().get(class) {
            return Ok(hit.clone());
        }
        let entry = to_entry(class);
        let mut got = None;
        for (path, src) in &self.entries {
            match src.find(&entry) {
                Ok(Some(bytes)) => {
                    got = Some(bytes);
                    break;
                }
                Ok(None) => {}
                // 找到了但解不出来（例如 deflate）：直接报出来，不要静默跳过
                Err(e) => return Err(format!("{} 里的 {entry}: {e}", path.display())),
            }
        }
        self.cache.borrow_mut().insert(class.to_string(), got.clone());
        Ok(got)
    }

    /// 哪个来源提供的这个类（诊断用）
    pub fn which(&self, class: &str) -> Option<&Path> {
        let entry = to_entry(class);
        self.entries
            .iter()
            .find(|(_, s)| matches!(s.find(&entry), Ok(Some(_))))
            .map(|(p, _)| p.as_path())
    }
}
