//! 属性表的读写。

use super::read::Reader;
use super::{Attribute, Result};

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

pub(crate) fn write_attributes(out: &mut Vec<u8>, attrs: &[Attribute]) {
    out.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
    for a in attrs {
        out.extend_from_slice(&a.name_index.to_be_bytes());
        out.extend_from_slice(&(a.info.len() as u32).to_be_bytes());
        out.extend_from_slice(&a.info);
    }
}
