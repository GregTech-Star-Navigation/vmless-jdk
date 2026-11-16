//! 常量池条目的编解码。
//!
//! 写回的一半（`write_cp`）在这里；读的一半仍是 `read::parse` 里的 tag 分派 ——
//! 拆出函数体属于逻辑改动，本轮不动。

use super::CpEntry;

pub(crate) fn write_cp(out: &mut Vec<u8>, e: &CpEntry) {
    match e {
        CpEntry::Utf8(b) => {
            out.push(1);
            out.extend_from_slice(&(b.len() as u16).to_be_bytes());
            out.extend_from_slice(b);
        }
        CpEntry::Integer(v) => {
            out.push(3);
            out.extend_from_slice(&v.to_be_bytes());
        }
        CpEntry::Float(v) => {
            out.push(4);
            out.extend_from_slice(&v.to_be_bytes());
        }
        CpEntry::Long(v) => {
            out.push(5);
            out.extend_from_slice(&v.to_be_bytes());
        }
        CpEntry::Double(v) => {
            out.push(6);
            out.extend_from_slice(&v.to_be_bytes());
        }
        CpEntry::Class(i) => {
            out.push(7);
            out.extend_from_slice(&i.to_be_bytes());
        }
        CpEntry::String(i) => {
            out.push(8);
            out.extend_from_slice(&i.to_be_bytes());
        }
        CpEntry::FieldRef(a, b) => {
            out.push(9);
            out.extend_from_slice(&a.to_be_bytes());
            out.extend_from_slice(&b.to_be_bytes());
        }
        CpEntry::MethodRef(a, b) => {
            out.push(10);
            out.extend_from_slice(&a.to_be_bytes());
            out.extend_from_slice(&b.to_be_bytes());
        }
        CpEntry::InterfaceMethodRef(a, b) => {
            out.push(11);
            out.extend_from_slice(&a.to_be_bytes());
            out.extend_from_slice(&b.to_be_bytes());
        }
        CpEntry::NameAndType(a, b) => {
            out.push(12);
            out.extend_from_slice(&a.to_be_bytes());
            out.extend_from_slice(&b.to_be_bytes());
        }
        CpEntry::MethodHandle(k, i) => {
            out.push(15);
            out.push(*k);
            out.extend_from_slice(&i.to_be_bytes());
        }
        CpEntry::MethodType(i) => {
            out.push(16);
            out.extend_from_slice(&i.to_be_bytes());
        }
        CpEntry::Dynamic(a, b) => {
            out.push(17);
            out.extend_from_slice(&a.to_be_bytes());
            out.extend_from_slice(&b.to_be_bytes());
        }
        CpEntry::InvokeDynamic(a, b) => {
            out.push(18);
            out.extend_from_slice(&a.to_be_bytes());
            out.extend_from_slice(&b.to_be_bytes());
        }
        CpEntry::Module(i) => {
            out.push(19);
            out.extend_from_slice(&i.to_be_bytes());
        }
        CpEntry::Package(i) => {
            out.push(20);
            out.extend_from_slice(&i.to_be_bytes());
        }
    }
}
