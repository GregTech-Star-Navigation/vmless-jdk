//! `module-info.class` 的 `Module` 属性解析（jlink 的 requires 图、jmod describe 都要）。
//!
//! 属性布局（JVMS §4.7.25）：
//! ```text
//! Module_attribute {
//!   u2 attribute_name_index; u4 attribute_length;
//!   u2 module_name_index; u2 module_flags; u2 module_version_index;
//!   u2 requires_count;  { u2 requires_index, u2 requires_flags, u2 requires_version_index } *
//!   u2 exports_count;   { u2 exports_index, u2 exports_flags, u2 exports_to_count, u2 * } *
//!   u2 opens_count;     { 同上 } *
//!   u2 uses_count;      { u2 uses_index } *
//!   u2 provides_count;  { u2 provides_index, u2 provides_with_count, u2 * } *
//! }
//! ```

use classfile::{ClassFile, CpEntry};

pub const ACC_MODULE: u16 = 0x8000;
/// module_flags 里的 ACC_OPEN
pub const ACC_OPEN: u16 = 0x0020;
/// requires_flags 里的 ACC_TRANSITIVE / ACC_STATIC_PHASE / ACC_SYNTHETIC / ACC_MANDATED
pub const ACC_TRANSITIVE: u16 = 0x0020;
pub const ACC_STATIC_PHASE: u16 = 0x0040;

#[derive(Clone, Debug, Default)]
pub struct Requires {
    pub name: String,
    pub flags: u16,
    pub version: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Export {
    pub package: String,
    pub flags: u16,
    pub to: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Provides {
    pub service: String,
    pub with: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ModuleInfo {
    pub name: String,
    pub flags: u16,
    pub version: Option<String>,
    pub requires: Vec<Requires>,
    pub exports: Vec<Export>,
    pub opens: Vec<Export>,
    pub uses: Vec<String>,
    pub provides: Vec<Provides>,
    pub main_class: Option<String>,
    pub packages: Vec<String>,
}

impl ModuleInfo {
    pub fn is_open(&self) -> bool {
        self.flags & ACC_OPEN != 0
    }
    /// 把 `java.base` 之类的模块名 → 内部名形式（`module-info.class` 里存的是 `java/base`）
    pub fn internal_to_module(n: &str) -> String {
        n.replace('/', ".")
    }
}

struct Rd<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Rd<'a> {
    fn u2(&mut self) -> Result<u16, String> {
        if self.p + 2 > self.b.len() {
            return Err("module-info 属性被截断（读 u2 越界）".into());
        }
        let v = u16::from_be_bytes([self.b[self.p], self.b[self.p + 1]]);
        self.p += 2;
        Ok(v)
    }
    fn u4(&mut self) -> Result<u32, String> {
        if self.p + 4 > self.b.len() {
            return Err("module-info 属性被截断（读 u4 越界）".into());
        }
        let v = u32::from_be_bytes([
            self.b[self.p],
            self.b[self.p + 1],
            self.b[self.p + 2],
            self.b[self.p + 3],
        ]);
        self.p += 4;
        Ok(v)
    }
    fn skip(&mut self, n: usize) -> Result<(), String> {
        if self.p + n > self.b.len() {
            return Err("module-info 属性被截断".into());
        }
        self.p += n;
        Ok(())
    }
}

fn utf8(cf: &ClassFile, idx: u16) -> Option<String> {
    match cf.constant_pool.get(idx as usize) {
        Some(Some(CpEntry::Utf8(b))) => Some(String::from_utf8_lossy(b).into_owned()),
        _ => None,
    }
}

fn class_const(cf: &ClassFile, idx: u16) -> Option<String> {
    match cf.constant_pool.get(idx as usize) {
        Some(Some(CpEntry::Class(n))) => utf8(cf, *n),
        _ => None,
    }
}

/// `CONSTANT_Module_info`（模块名/`requires` 用的是它，不是 Class！）
fn module_const(cf: &ClassFile, idx: u16) -> Option<String> {
    match cf.constant_pool.get(idx as usize) {
        Some(Some(CpEntry::Module(n))) => utf8(cf, *n),
        _ => None,
    }
}

/// `CONSTANT_Package_info`（`exports` / `opens` 用的）
fn package_const(cf: &ClassFile, idx: u16) -> Option<String> {
    match cf.constant_pool.get(idx as usize) {
        Some(Some(CpEntry::Package(n))) => utf8(cf, *n),
        _ => None,
    }
}

/// 读 `module-info.class` 的字节，返回解析后的 `ModuleInfo`
pub fn parse_module_info(bytes: &[u8]) -> Result<ModuleInfo, String> {
    let cf = classfile::parse(bytes).map_err(|e| format!("解析 module-info.class 失败: {e}"))?;
    if cf.access_flags & ACC_MODULE == 0 {
        return Err("这不是 module-info.class（没有 ACC_MODULE 标志）".into());
    }
    let attr = cf
        .attributes
        .iter()
        .find(|a| cf.utf8(a.name_index).map(|b| b == b"Module").unwrap_or(false))
        .ok_or_else(|| "module-info.class 里没有 Module 属性".to_string())?;
    let mut r = Rd { b: &attr.info, p: 0 };
    let name_idx = r.u2()?;
    let flags = r.u2()?;
    let ver_idx = r.u2()?;
    let name = module_const(&cf, name_idx)
        .map(|n| ModuleInfo::internal_to_module(&n))
        .ok_or_else(|| "Module 属性的模块名索引无效".to_string())?;
    let mut mi = ModuleInfo {
        name,
        flags,
        version: if ver_idx == 0 { None } else { utf8(&cf, ver_idx) },
        ..Default::default()
    };
    let nreq = r.u2()?;
    for _ in 0..nreq {
        let idx = r.u2()?;
        let f = r.u2()?;
        let v = r.u2()?;
        mi.requires.push(Requires {
            name: module_const(&cf, idx)
                .map(|n| ModuleInfo::internal_to_module(&n))
                .unwrap_or_else(|| format!("<#{idx}>")),
            flags: f,
            version: if v == 0 { None } else { utf8(&cf, v) },
        });
    }
    let nexp = r.u2()?;
    for _ in 0..nexp {
        let idx = r.u2()?;
        let f = r.u2()?;
        let n = r.u2()?;
        let mut to = Vec::new();
        for _ in 0..n {
            let t = r.u2()?;
            if let Some(c) = class_const(&cf, t) {
                to.push(ModuleInfo::internal_to_module(&c));
            }
        }
        mi.exports.push(Export {
            package: package_const(&cf, idx).unwrap_or_else(|| format!("<#{idx}>")),
            flags: f,
            to,
        });
    }
    let nopen = r.u2()?;
    for _ in 0..nopen {
        let idx = r.u2()?;
        let f = r.u2()?;
        let n = r.u2()?;
        let mut to = Vec::new();
        for _ in 0..n {
            let t = r.u2()?;
            if let Some(c) = class_const(&cf, t) {
                to.push(ModuleInfo::internal_to_module(&c));
            }
        }
        mi.opens.push(Export {
            package: package_const(&cf, idx).unwrap_or_else(|| format!("<#{idx}>")),
            flags: f,
            to,
        });
    }
    let nuses = r.u2()?;
    for _ in 0..nuses {
        let idx = r.u2()?;
        if let Some(c) = class_const(&cf, idx) {
            mi.uses.push(ModuleInfo::internal_to_module(&c));
        }
    }
    let nprov = r.u2()?;
    for _ in 0..nprov {
        let idx = r.u2()?;
        let n = r.u2()?;
        let mut with = Vec::new();
        for _ in 0..n {
            let w = r.u2()?;
            if let Some(c) = class_const(&cf, w) {
                with.push(ModuleInfo::internal_to_module(&c));
            }
        }
        mi.provides.push(Provides {
            service: class_const(&cf, idx)
                .map(|n| ModuleInfo::internal_to_module(&n))
                .unwrap_or_default(),
            with,
        });
    }
    // 可选属性：ModuleMainClass / ModulePackages
    for a in &cf.attributes {
        let n = cf.utf8(a.name_index).map(|b| String::from_utf8_lossy(b).into_owned());
        match n.as_deref() {
            Some("ModuleMainClass") => {
                let mut rr = Rd { b: &a.info, p: 0 };
                if let Ok(i) = rr.u2() {
                    mi.main_class = class_const(&cf, i).map(|c| c.replace('/', "."));
                }
            }
            Some("ModulePackages") => {
                let mut rr = Rd { b: &a.info, p: 0 };
                if let Ok(cnt) = rr.u2() {
                    for _ in 0..cnt {
                        if let Ok(i) = rr.u2() {
                            if let Some(p) = utf8(&cf, i) {
                                mi.packages.push(p);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(mi)
}

/// 只要模块名（`jlink` 建索引时用，省一次完整解析）
pub fn module_name_of(bytes: &[u8]) -> Result<String, String> {
    Ok(parse_module_info(bytes)?.name)
}

/// `jmod describe` 的输出格式（贴近真 JDK）
pub fn describe_text(mi: &ModuleInfo) -> String {
    let mut s = String::new();
    s.push_str(&mi.name);
    if mi.is_open() {
        s.push_str(" open");
    }
    if let Some(v) = &mi.version {
        s.push_str(&format!("@{v}"));
    }
    s.push('\n');
    for r in &mi.requires {
        let m = mi_require_mods(r.flags);
        let mut line = format!("requires {}", r.name);
        if !m.is_empty() {
            line.push_str(&format!(" {m}"));
        }
        s.push_str(line.trim_end());
        s.push('\n');
    }
    for e in &mi.exports {
        let mut line = format!("exports {}", e.package.replace('/', "."));
        if !e.to.is_empty() {
            line.push_str(&format!(" to {}", e.to.join(", ")));
        }
        s.push_str(line.trim_end());
        s.push('\n');
    }
    for o in &mi.opens {
        let mut line = format!("opens {}", o.package.replace('/', "."));
        if !o.to.is_empty() {
            line.push_str(&format!(" to {}", o.to.join(", ")));
        }
        s.push_str(line.trim_end());
        s.push('\n');
    }
    for u in &mi.uses {
        s.push_str(&format!("uses {}\n", u.replace('/', ".")));
    }
    for p in &mi.provides {
        s.push_str(&format!(
            "provides {} with {}\n",
            p.service.replace('/', "."),
            p.with.join(", ")
        ));
    }
    s
}

fn mi_require_mods(flags: u16) -> String {
    let mut v = Vec::new();
    if flags & ACC_TRANSITIVE != 0 {
        v.push("transitive");
    }
    if flags & ACC_STATIC_PHASE != 0 {
        v.push("static");
    }
    v.join(" ")
}

#[cfg(test)]
mod tests {
    #[test]
    fn empty_input_errors() {
        assert!(super::parse_module_info(&[]).is_err());
    }
}
