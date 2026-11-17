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
        Some(CpEntry::Utf8(b)) if b == b"BootstrapMethods" => Some(i as u16),
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
