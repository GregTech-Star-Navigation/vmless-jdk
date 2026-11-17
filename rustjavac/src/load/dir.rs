//! 目录树来源：`-cp out/` 这种把包名当子目录的布局（原来的行为）。

use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct DirSource {
    root: PathBuf,
}

impl DirSource {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, String> {
        let root = root.as_ref();
        if !root.is_dir() {
            return Err(format!("{} 不是目录", root.display()));
        }
        Ok(DirSource { root: root.to_path_buf() })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 条目路径 → 字节。拒绝 `..` 与绝对路径（classpath 不该把查找带出根目录）。
    pub fn find(&self, entry: &str) -> Result<Option<Vec<u8>>, String> {
        if entry.starts_with('/') || entry.contains("..") || entry.contains(':') {
            return Err(format!("非法条目路径: {entry}"));
        }
        let p = self.root.join(entry.replace('/', std::path::MAIN_SEPARATOR_STR));
        match std::fs::read(&p) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("读 {} 失败: {e}", p.display())),
        }
    }
}
