//! 写：把 `ClassFile` 编回字节，保持逐字节一致。

use super::attr::write_attributes;
use super::pool::write_cp;
use super::{ClassFile, Member};

pub fn write(cf: &ClassFile) -> Vec<u8> {
    let mut out = Vec::with_capacity(1024);
    out.extend_from_slice(&0xCAFE_BABEu32.to_be_bytes());
    out.extend_from_slice(&cf.minor_version.to_be_bytes());
    out.extend_from_slice(&cf.major_version.to_be_bytes());
    out.extend_from_slice(&(cf.constant_pool.len() as u16).to_be_bytes());
    for entry in cf.constant_pool.iter().skip(1) {
        if let Some(e) = entry {
            write_cp(&mut out, e, &cf.utf8);
        }
    }
    out.extend_from_slice(&cf.access_flags.to_be_bytes());
    out.extend_from_slice(&cf.this_class.to_be_bytes());
    out.extend_from_slice(&cf.super_class.to_be_bytes());
    out.extend_from_slice(&(cf.interfaces.len() as u16).to_be_bytes());
    for i in &cf.interfaces {
        out.extend_from_slice(&i.to_be_bytes());
    }
    write_members(&mut out, &cf.fields);
    write_members(&mut out, &cf.methods);
    write_attributes(&mut out, &cf.attributes);
    out
}

fn write_members(out: &mut Vec<u8>, members: &[Member]) {
    out.extend_from_slice(&(members.len() as u16).to_be_bytes());
    for m in members {
        out.extend_from_slice(&m.access_flags.to_be_bytes());
        out.extend_from_slice(&m.name_index.to_be_bytes());
        out.extend_from_slice(&m.descriptor_index.to_be_bytes());
        write_attributes(out, &m.attributes);
    }
}
