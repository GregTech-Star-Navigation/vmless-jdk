//! 类集合的收集（目录 / 模块目录树 / jar / jimage）与字节码层面的扫描原语。
//!
//! `jnativescan`、`jdeprscan`、`jlink`、`javadoc`（不接受字节码，但共用遍历）、
//! `serialver`（按名字找类）都从这里取数据。

use classfile::{ClassFile, CpEntry, Member};
use std::path::{Path, PathBuf};

use crate::svuid::ACC_NATIVE;

/// 一个类条目：所属模块（没有模块层时为空）、相对路径、字节
#[derive(Clone)]
pub struct ClassEntry {
    pub module: String,
    /// 模块内相对路径（`java/lang/Object.class`），没有模块层时含全部前缀
    pub entry: String,
    pub bytes: Vec<u8>,
}

impl ClassEntry {
    /// 内部类名（`java/lang/Object`）
    pub fn internal_name(&self) -> String {
        self.entry
            .trim_start_matches('/')
            .strip_suffix(".class")
            .unwrap_or(&self.entry)
            .to_string()
    }
    /// 二进制类名（`java.lang.Object`）
    pub fn binary_name(&self) -> String {
        self.internal_name().replace('/', ".")
    }
}

/// 输入的类型（判据：目录 / jimage / zip）
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum InputKind {
    Dir,
    ModuleTree,
    JImage,
    Zip,
}

fn is_jimage(p: &Path) -> bool {
    use std::io::Read;
    match std::fs::File::open(p) {
        Ok(mut f) => {
            let mut m = [0u8; 4];
            matches!(f.read_exact(&mut m), Ok(())) && m == [0xDA, 0xDA, 0xFE, 0xCA]
        }
        Err(_) => false,
    }
}

fn is_zip(p: &Path) -> bool {
    use std::io::Read;
    match std::fs::File::open(p) {
        Ok(mut f) => {
            let mut m = [0u8; 4];
            matches!(f.read_exact(&mut m), Ok(()))
                && ((m[0] == b'P' && m[1] == b'K')
                    || m == [0x44, 0x4F, 0x4D, 0x4A] // 反过来的 JMOD（小端序读取兜底）
                    || (m[0] == 0x4A && m[1] == 0x4D))
        }
        Err(_) => false,
    }
}

/// 目录是不是「模块目录树」（顶层每个子目录是一个模块，且里面有 module-info.class）
pub fn is_module_tree(root: &Path) -> bool {
    let ok = |p: &Path| p.join("module-info.class").is_file();
    if ok(root) {
        return false; // 自己就是模块根
    }
    match std::fs::read_dir(root) {
        Ok(rd) => {
            let mut any = false;
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() && ok(&p) {
                    any = true;
                    break;
                }
            }
            any
        }
        Err(_) => false,
    }
}

pub fn kind_of(input: &Path) -> Result<InputKind, String> {
    if input.is_dir() {
        Ok(if is_module_tree(input) { InputKind::ModuleTree } else { InputKind::Dir })
    } else if input.is_file() {
        if is_jimage(input) {
            Ok(InputKind::JImage)
        } else if is_zip(input) {
            Ok(InputKind::Zip)
        } else {
            Err(format!(
                "{} 既不是目录，也不是 jimage / jar / jmod（魔数不匹配）",
                input.display()
            ))
        }
    } else {
        Err(format!("路径不存在: {}", input.display()))
    }
}

/// 收集类条目。`include` 非空时按 glob 过滤（匹配 `/<module>/<entry>` 与 `<entry>`）。只收 `.class`。
pub fn collect_classes(input: &Path, include: &[String]) -> Result<(InputKind, Vec<ClassEntry>), String> {
    let kind = kind_of(input)?;
    let mut out = Vec::new();
    let matches = |module: &str, entry: &str| -> bool {
        if include.is_empty() {
            return true;
        }
        let full = if module.is_empty() {
            format!("/{entry}")
        } else {
            format!("/{module}/{entry}")
        };
        crate::any_match(include, &full) || crate::any_match(include, entry)
    };
    match kind {
        InputKind::JImage => {
            let mut img = crate::jimagex::Image::open(input)?;
            // 借用 entries 时要可变读，先拍一份名字
            let list: Vec<(String, String)> = img
                .entries()
                .iter()
                .map(|e| (e.module.clone(), e.rel_path()))
                .collect();
            for (i, (module, entry)) in list.iter().enumerate() {
                if !entry.ends_with(".class") || !matches(module, entry) {
                    continue;
                }
                let e = img.entries()[i].clone();
                let bytes = img.read(&e)?;
                out.push(ClassEntry {
                    module: module.clone(),
                    entry: entry.clone(),
                    bytes,
                });
            }
        }
        InputKind::Zip => {
            let z = crate::zipx::ZipReader::open(input)?;
            for e in &z.entries {
                let name = &e.name;
                if !name.ends_with(".class") || name.starts_with("classes/") && false {
                    continue;
                }
                // jmod 的类在 classes/ 下；jar 的类在根
                let rel = if let Some(r) = name.strip_prefix("classes/") {
                    r.to_string()
                } else if name.starts_with("META-INF/") {
                    continue;
                } else {
                    name.clone()
                };
                if !matches("", &rel) {
                    continue;
                }
                let bytes = z.read(e)?;
                out.push(ClassEntry { module: String::new(), entry: rel, bytes });
            }
        }
        InputKind::ModuleTree => {
            let mut mods: Vec<PathBuf> = std::fs::read_dir(input)
                .map_err(|e| format!("读目录 {} 失败: {e}", input.display()))?
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            mods.sort();
            for m in mods {
                let name = m
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                for f in crate::walk_files(&m, false)? {
                    if f.extension().and_then(|s| s.to_str()) != Some("class") {
                        continue;
                    }
                    let rel = f
                        .strip_prefix(&m)
                        .map_err(|e| format!("算相对路径失败: {e}"))?
                        .to_string_lossy()
                        .replace('\\', "/");
                    if !matches(&name, &rel) {
                        continue;
                    }
                    let bytes = std::fs::read(&f).map_err(|e| format!("读 {} 失败: {e}", f.display()))?;
                    out.push(ClassEntry { module: name.clone(), entry: rel, bytes });
                }
            }
        }
        InputKind::Dir => {
            for f in crate::walk_files(input, false)? {
                if f.extension().and_then(|s| s.to_str()) != Some("class") {
                    continue;
                }
                let rel = f
                    .strip_prefix(input)
                    .map_err(|e| format!("算相对路径失败: {e}"))?
                    .to_string_lossy()
                    .replace('\\', "/");
                if !matches("", &rel) {
                    continue;
                }
                let bytes = std::fs::read(&f).map_err(|e| format!("读 {} 失败: {e}", f.display()))?;
                out.push(ClassEntry { module: String::new(), entry: rel, bytes });
            }
        }
    }
    Ok((kind, out))
}

/// 只收集 module-info.class（`jlink` 的模块解析用），返回 (模块名, 字节)
pub fn collect_module_infos(input: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let kind = kind_of(input)?;
    let mut out = Vec::new();
    match kind {
        InputKind::Dir => {
            // 目录里的 module-info：要么在根，要么在子目录（模块树）
            let root_info = input.join("module-info.class");
            if root_info.is_file() {
                let bytes = std::fs::read(&root_info)
                    .map_err(|e| format!("读 {} 失败: {e}", root_info.display()))?;
                let name = crate::modinfo::module_name_of(&bytes)?;
                out.push((name, bytes));
            }
            let mut subs: Vec<PathBuf> = std::fs::read_dir(input)
                .map_err(|e| format!("读目录 {} 失败: {e}", input.display()))?
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir() && p.join("module-info.class").is_file())
                .collect();
            subs.sort();
            for s in subs {
                let f = s.join("module-info.class");
                let bytes = std::fs::read(&f).map_err(|e| format!("读 {} 失败: {e}", f.display()))?;
                let name = crate::modinfo::module_name_of(&bytes)?;
                out.push((name, bytes));
            }
        }
        InputKind::ModuleTree => {
            for (name, _) in collect_module_infos_as_dir(input)? {
                out.push((name, vec![]));
            }
        }
        InputKind::JImage => {
            let mut img = crate::jimagex::Image::open(input)?;
            let hits: Vec<usize> = img
                .entries()
                .iter()
                .enumerate()
                .filter(|(_, e)| e.rel_path() == "module-info.class")
                .map(|(i, _)| i)
                .collect();
            for i in hits {
                let e = img.entries()[i].clone();
                let bytes = img.read(&e)?;
                out.push((e.module.clone(), bytes));
            }
        }
        InputKind::Zip => {
            let z = crate::zipx::ZipReader::open(input)?;
            for e in &z.entries {
                if e.name == "classes/module-info.class" || e.name == "module-info.class" {
                    let bytes = z.read(e)?;
                    out.push((crate::modinfo::module_name_of(&bytes)?, bytes));
                }
            }
        }
    }
    Ok(out)
}

fn collect_module_infos_as_dir(input: &Path) -> Result<Vec<(String, () )>, String> {
    let mut out = Vec::new();
    let mut subs: Vec<PathBuf> = std::fs::read_dir(input)
        .map_err(|e| format!("读目录 {} 失败: {e}", input.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("module-info.class").is_file())
        .collect();
    subs.sort();
    for s in subs {
        out.push((s.file_name().and_then(|x| x.to_str()).unwrap_or("").to_string(), ()));
    }
    Ok(out)
}

/// 类的静态 native 方法（`ACC_NATIVE` 且 `ACC_STATIC`）
pub fn native_methods(cf: &ClassFile) -> Result<Vec<(String, String, bool)>, String> {
    let mut out = Vec::new();
    for m in &cf.methods {
        if m.access_flags & ACC_NATIVE == 0 {
            continue;
        }
        let name = cf
            .utf8(m.name_index)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .ok_or_else(|| "方法名索引越界（class 文件损坏）".to_string())?;
        let desc = cf
            .utf8(m.descriptor_index)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .ok_or_else(|| "方法描述符索引越界（class 文件损坏）".to_string())?;
        out.push((name, desc, m.access_flags & crate::svuid::ACC_STATIC != 0));
    }
    Ok(out)
}

/// 类里对「外部成员」的全部引用（常量池里的 Fieldref/Methodref/InterfaceMethodref）。
/// 返回 (所有者内部名, 成员名, 描述符, 是方法)
pub fn member_refs(cf: &ClassFile) -> Vec<(String, String, String, bool)> {
    let mut out = Vec::new();
    let utf8 = |i: u16| -> Option<String> {
        cf.utf8(i).map(|b| String::from_utf8_lossy(b).into_owned())
    };
    let class_of = |i: u16| -> Option<String> {
        match cf.constant_pool.get(i as usize) {
            Some(Some(CpEntry::Class(n))) => utf8(*n),
            _ => None,
        }
    };
    for e in cf.constant_pool.iter().flatten() {
        let (ci, nti, is_method) = match e {
            CpEntry::FieldRef(c, nt) => (*c, *nt, false),
            CpEntry::MethodRef(c, nt) => (*c, *nt, true),
            CpEntry::InterfaceMethodRef(c, nt) => (*c, *nt, true),
            _ => continue,
        };
        let owner = match class_of(ci) {
            Some(v) => v,
            None => continue,
        };
        let (name, desc) = match cf.constant_pool.get(nti as usize) {
            Some(Some(CpEntry::NameAndType(n, d))) => match (utf8(*n), utf8(*d)) {
                (Some(a), Some(b)) => (a, b),
                _ => continue,
            },
            _ => continue,
        };
        out.push((owner, name, desc, is_method));
    }
    out
}

/// 常量池里被引用的所有类（内部名），`jdeprscan` 的「类型级 deprecated」检查用
pub fn class_refs(cf: &ClassFile) -> Vec<String> {
    let mut out = Vec::new();
    for e in cf.constant_pool.iter().flatten() {
        if let CpEntry::Class(n) = e {
            if let Some(b) = cf.utf8(*n) {
                out.push(String::from_utf8_lossy(b).into_owned());
            }
        }
    }
    out
}

/// 成员是否 `ACC_NATIVE`
pub fn is_native(m: &Member) -> bool {
    m.access_flags & ACC_NATIVE != 0
}

/// 安全解析：把坏 class 的错误信息带上条目名
pub fn parse_entry(e: &ClassEntry) -> Result<ClassFile, String> {
    classfile::parse(&e.bytes).map_err(|err| format!("解析 {} 失败: {err}", e.entry))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_detection_errors() {
        assert!(kind_of(Path::new("D:/definitely/not/here")).is_err());
    }
}
