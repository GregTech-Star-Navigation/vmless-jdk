//! 属性表的读写。

use super::read::Reader;
use super::{Attribute, ClassFile, CpEntry, Result};

pub(crate) fn read_attributes(r: &mut Reader<'_>) -> Result<Vec<Attribute>> {
    let count = r.u16()? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let name_index = r.u16()?;
        let len = r.u32()? as usize;
        out.push(Attribute { name_index, info: r.bytes(len)?.to_vec() });
    }
    Ok(out)
}

/// `BootstrapMethods` 属性里的一项：引导方法句柄 + 它的静态参数（都是常量池索引）
#[derive(Clone, Debug, PartialEq)]
pub struct BootstrapMethod {
    pub method_ref: u16,
    pub args: Vec<u16>,
}

/// 解析 `BootstrapMethods` 属性（`invokedynamic` 与 `CONSTANT_Dynamic` 都靠它）。
///
/// 每个类**最多一个**该属性；没有就是空表（那时任何 indy 都解析不出引导方法，
/// 由调用点报 `BootstrapMethodError`）。属性体格式：
/// `u2 num_bootstrap_methods` + 每项 `u2 method_ref + u2 num_args + u2 args[num_args]`。
pub fn bootstrap_methods(cf: &ClassFile) -> Result<Vec<BootstrapMethod>> {
    let name = cf.constant_pool.iter().enumerate().find_map(|(i, e)| match e {
        Some(CpEntry::Utf8(..)) if cf.utf8(i as u16) == Some(&b"BootstrapMethods"[..]) => Some(i as u16),
        _ => None,
    });
    let Some(name) = name else { return Ok(Vec::new()) };
    let attr = cf.attributes.iter().find(|a| a.name_index == name);
    let Some(attr) = attr else { return Ok(Vec::new()) };
    let mut r = Reader::new(&attr.info);
    let count = r.u16()? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let method_ref = r.u16()?;
        let n = r.u16()? as usize;
        let mut args = Vec::with_capacity(n);
        for _ in 0..n {
            args.push(r.u16()?);
        }
        out.push(BootstrapMethod { method_ref, args });
    }
    Ok(out)
}

pub(crate) fn write_attributes(out: &mut Vec<u8>, attrs: &[Attribute]) {
    out.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
    for a in attrs {
        out.extend_from_slice(&a.name_index.to_be_bytes());
        out.extend_from_slice(&(a.info.len() as u32).to_be_bytes());
        out.extend_from_slice(&a.info);
    }
}

// ------------------------------------------------------------------ module-info

/// `Module` 属性的 `requires` 一项
#[derive(Clone, Debug, PartialEq)]
pub struct RequiresDesc {
    /// 模块名（点分）
    pub module: String,
    pub flags: u16,
    /// `requires` 的版本串（很少非空）
    pub version: Option<String>,
}

/// `Module` 属性的 `exports` / `opens` 一项
#[derive(Clone, Debug, PartialEq)]
pub struct ExportDesc {
    /// 包名（**点分**，`java.lang.invoke`）
    pub package: String,
    pub flags: u16,
    /// 非空 = qualified（只对这些模块开放）
    pub to: Vec<String>,
}

/// `Module` 属性的 `provides` 一项
#[derive(Clone, Debug, PartialEq)]
pub struct ProvidesDesc {
    pub service: String,
    pub with: Vec<String>,
}

/// `module-info.class` 里的模块描述符（`Module` + `Module*` 属性合并的结果）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModuleInfo {
    pub name: String,
    pub flags: u16,
    pub version: Option<String>,
    pub requires: Vec<RequiresDesc>,
    pub exports: Vec<ExportDesc>,
    pub opens: Vec<ExportDesc>,
    pub uses: Vec<String>,
    pub provides: Vec<ProvidesDesc>,
    /// `ModulePackages`：模块里的全部包（点分）
    pub packages: Vec<String>,
    /// `ModuleMainClass`
    pub main_class: Option<String>,
    /// `ModuleTarget`：目标平台串（`windows-amd64` 之类）
    pub target: Option<String>,
    /// `ModuleResolution` 的 flags 原值
    pub resolution: Option<u16>,
}

// 访问标志（JVMS 4.7.25）
pub const ACC_MODULE_OPEN: u16 = 0x0020;
pub const ACC_MODULE_SYNTHETIC: u16 = 0x1000;
pub const ACC_MODULE_MANDATED: u16 = 0x8000;
pub const ACC_REQ_TRANSITIVE: u16 = 0x0020;
pub const ACC_REQ_STATIC_PHASE: u16 = 0x0040;

impl ModuleInfo {
    /// `open module`：所有包都 opened（没有 `opens` 列表）
    pub fn is_open(&self) -> bool {
        self.flags & ACC_MODULE_OPEN != 0
    }

    /// `exports` 里**非限定**的那部分（对所有人开放）
    pub fn exports_all(&self) -> impl Iterator<Item = &ExportDesc> {
        self.exports.iter().filter(|e| e.to.is_empty())
    }

    pub fn requires_names(&self) -> impl Iterator<Item = &str> {
        self.requires.iter().map(|r| r.module.as_str())
    }
}

/// 常量池里 `CONSTANT_Module`/`CONSTANT_Package` 的 Utf8 名（内部形式：`java/lang`）
fn internal_at(cf: &ClassFile, idx: u16) -> Option<String> {
    let n = match cf.constant_pool.get(idx as usize)?.as_ref()? {
        CpEntry::Module(n) | CpEntry::Package(n) => *n,
        _ => return None,
    };
    cf.utf8(n).map(|b| String::from_utf8_lossy(b).into_owned())
}

/// 点分形式（`java/lang` → `java.lang`）
fn dotted(s: &str) -> String {
    s.replace('/', ".")
}

/// `CONSTANT_Class` 项 → 点分类名
fn class_dotted(cf: &ClassFile, idx: u16) -> Option<String> {
    let n = match cf.constant_pool.get(idx as usize)?.as_ref()? {
        CpEntry::Class(n) => *n,
        _ => return None,
    };
    cf.utf8(n).map(|b| dotted(&String::from_utf8_lossy(b)))
}

fn utf8_at(cf: &ClassFile, idx: u16) -> Option<String> {
    if idx == 0 {
        return None;
    }
    cf.utf8(idx).map(|b| String::from_utf8_lossy(b).into_owned())
}

fn attr_body<'a>(cf: &'a ClassFile, name: &str) -> Option<&'a [u8]> {
    let name_index = cf.constant_pool.iter().enumerate().find_map(|(i, e)| match e {
        Some(CpEntry::Utf8(..)) if cf.utf8(i as u16) == Some(name.as_bytes()) => Some(i as u16),
        _ => None,
    })?;
    cf.attributes.iter().find(|a| a.name_index == name_index).map(|a| a.info.as_slice())
}

/// 解析 `module-info.class` 的模块描述符；不是模块描述符就返回 `None`。
///
/// `Module` 属性（JVMS 4.7.25）：
/// ```text
/// u2 module_name_index | u2 module_flags | u2 module_version_index
/// u2 requires_count  { u2 requires_index | u2 requires_flags | u2 requires_version_index }
/// u2 exports_count   { u2 exports_index | u2 exports_flags | u2 exports_to_count | u2 exports_to_index[] }
/// u2 opens_count     { 同上 }
/// u2 uses_count      { u2 uses_index }
/// u2 provides_count  { u2 provides_index | u2 provides_with_count | u2 provides_with_index[] }
/// ```
/// `exports_index`/`provides_index` 是 `CONSTANT_Package`/`CONSTANT_Class`，
/// 里面存的是**内部形式**（`java/lang`），这里统一转成点分。
pub fn module_info(cf: &ClassFile) -> Result<Option<ModuleInfo>> {
    let Some(body) = attr_body(cf, "Module") else { return Ok(None) };
    let mut r = Reader::new(body);
    let mut mi = ModuleInfo::default();
    let name_index = r.u16()?;
    mi.flags = r.u16()?;
    let version_index = r.u16()?;
    mi.name = internal_at(cf, name_index).map(|s| dotted(&s)).unwrap_or_default();
    mi.version = utf8_at(cf, version_index);

    let read_module_list = |r: &mut Reader<'_>, cf: &ClassFile, n: usize| -> Result<Vec<String>> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let i = r.u16()?;
            out.push(internal_at(cf, i).map(|s| dotted(&s)).unwrap_or_default());
        }
        Ok(out)
    };

    let n = r.u16()? as usize;
    mi.requires.reserve(n);
    for _ in 0..n {
        let module = internal_at(cf, r.u16()?).map(|s| dotted(&s)).unwrap_or_default();
        let flags = r.u16()?;
        let version = utf8_at(cf, r.u16()?);
        mi.requires.push(RequiresDesc { module, flags, version });
    }

    for is_opens in [false, true] {
        let n = r.u16()? as usize;
        let mut list = Vec::with_capacity(n);
        for _ in 0..n {
            let pkg = internal_at(cf, r.u16()?).map(|s| dotted(&s)).unwrap_or_default();
            let flags = r.u16()?;
            let cnt = r.u16()? as usize;
            let to = read_module_list(&mut r, cf, cnt)?;
            list.push(ExportDesc { package: pkg, flags, to });
        }
        if is_opens {
            mi.opens = list;
        } else {
            mi.exports = list;
        }
    }

    let n = r.u16()? as usize;
    let mut uses = Vec::with_capacity(n);
    for _ in 0..n {
        uses.push(class_dotted(cf, r.u16()?).unwrap_or_default());
    }
    mi.uses = uses;

    let n = r.u16()? as usize;
    mi.provides.reserve(n);
    for _ in 0..n {
        let service = class_dotted(cf, r.u16()?).unwrap_or_default();
        let cnt = r.u16()? as usize;
        let mut with = Vec::with_capacity(cnt);
        for _ in 0..cnt {
            with.push(class_dotted(cf, r.u16()?).unwrap_or_default());
        }
        mi.provides.push(ProvidesDesc { service, with });
    }

    // ---- ModulePackages ----
    if let Some(body) = attr_body(cf, "ModulePackages") {
        let mut r = Reader::new(body);
        let n = r.u16()? as usize;
        let mut pkgs = Vec::with_capacity(n);
        for _ in 0..n {
            pkgs.push(internal_at(cf, r.u16()?).map(|s| dotted(&s)).unwrap_or_default());
        }
        mi.packages = pkgs;
    }
    // ---- ModuleMainClass ----
    if let Some(body) = attr_body(cf, "ModuleMainClass") {
        let mut r = Reader::new(body);
        mi.main_class = class_dotted(cf, r.u16()?);
    }
    // ---- ModuleTarget ----
    if let Some(body) = attr_body(cf, "ModuleTarget") {
        let mut r = Reader::new(body);
        mi.target = utf8_at(cf, r.u16()?);
    }
    // ---- ModuleResolution ----
    if let Some(body) = attr_body(cf, "ModuleResolution") {
        let mut r = Reader::new(body);
        mi.resolution = Some(r.u16()?);
    }
    Ok(Some(mi))
}
