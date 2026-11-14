//! JVM class 文件读写（Rust 版 javac 的地基）。
//!
//! 目标：读入任意 `.class`，原样写回，**逐字节一致**。
//! 有了它，符号表、代码生成、Rust 运行时（librustjrt）才有共同的数据结构。

pub mod ast;
pub mod sema;
pub mod lower;
pub mod lexer;
pub mod parser;

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Truncated(&'static str),
    BadMagic(u32),
    BadConstantPool(u8),
    Utf8(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated(w) => write!(f, "unexpected end of class file while reading {w}"),
            Error::BadMagic(m) => write!(f, "bad magic 0x{m:08x}"),
            Error::BadConstantPool(t) => write!(f, "bad constant pool tag {t}"),
            Error::Utf8(s) => write!(f, "utf8 error: {s}"),
        }
    }
}

impl std::error::Error for Error {}

// ---------------------------------------------------------------- reader

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn u8(&mut self) -> Result<u8> {
        if self.pos >= self.buf.len() {
            return Err(Error::Truncated("u8"));
        }
        let v = self.buf[self.pos];
        self.pos += 1;
        Ok(v)
    }

    fn u16(&mut self) -> Result<u16> {
        if self.pos + 2 > self.buf.len() {
            return Err(Error::Truncated("u16"));
        }
        let v = u16::from_be_bytes([self.buf[self.pos], self.buf[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    fn u32(&mut self) -> Result<u32> {
        if self.pos + 4 > self.buf.len() {
            return Err(Error::Truncated("u32"));
        }
        let v = u32::from_be_bytes([
            self.buf[self.pos],
            self.buf[self.pos + 1],
            self.buf[self.pos + 2],
            self.buf[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(v)
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            return Err(Error::Truncated("bytes"));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
}

// ---------------------------------------------------------------- model

#[derive(Clone, Debug, PartialEq)]
pub enum CpEntry {
    /// 原始 Modified UTF-8 字节，保持逐字节一致
    Utf8(Vec<u8>),
    Integer(u32),
    Float(u32),
    Long(u64),
    Double(u64),
    Class(u16),
    String(u16),
    FieldRef(u16, u16),
    MethodRef(u16, u16),
    InterfaceMethodRef(u16, u16),
    NameAndType(u16, u16),
    MethodHandle(u8, u16),
    MethodType(u16),
    Dynamic(u16, u16),
    InvokeDynamic(u16, u16),
    Module(u16),
    Package(u16),
}

impl CpEntry {
    /// long/double 在常量池里占两个槽位
    pub fn slots(&self) -> usize {
        match self {
            CpEntry::Long(_) | CpEntry::Double(_) => 2,
            _ => 1,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Attribute {
    pub name_index: u16,
    pub info: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Member {
    pub access_flags: u16,
    pub name_index: u16,
    pub descriptor_index: u16,
    pub attributes: Vec<Attribute>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClassFile {
    pub minor_version: u16,
    pub major_version: u16,
    /// 索引 0 未使用；long/double 后的槽位为 None
    pub constant_pool: Vec<Option<CpEntry>>,
    pub access_flags: u16,
    pub this_class: u16,
    pub super_class: u16,
    pub interfaces: Vec<u16>,
    pub fields: Vec<Member>,
    pub methods: Vec<Member>,
    pub attributes: Vec<Attribute>,
}

impl ClassFile {
    pub fn major(&self) -> u16 {
        self.major_version
    }

    /// 常量池里的 Utf8 文本（用于调试/后续符号表）
    pub fn utf8(&self, index: u16) -> Option<&[u8]> {
        match self.constant_pool.get(index as usize) {
            Some(Some(CpEntry::Utf8(b))) => Some(b),
            _ => None,
        }
    }

    pub fn this_class_name(&self) -> Option<String> {
        let idx = match self.constant_pool.get(self.this_class as usize) {
            Some(Some(CpEntry::Class(n))) => *n,
            _ => return None,
        };
        self.utf8(idx).map(|b| String::from_utf8_lossy(b).into_owned())
    }
}

// ---------------------------------------------------------------- parse

pub fn parse(buf: &[u8]) -> Result<ClassFile> {
    let mut r = Reader::new(buf);
    let magic = r.u32()?;
    if magic != 0xCAFE_BABE {
        return Err(Error::BadMagic(magic));
    }
    let minor_version = r.u16()?;
    let major_version = r.u16()?;

    let cp_count = r.u16()? as usize;
    let mut constant_pool: Vec<Option<CpEntry>> = vec![None; cp_count];
    let mut i = 1;
    while i < cp_count {
        let tag = r.u8()?;
        let entry = match tag {
            1 => {
                let len = r.u16()? as usize;
                CpEntry::Utf8(r.bytes(len)?.to_vec())
            }
            3 => CpEntry::Integer(r.u32()?),
            4 => CpEntry::Float(r.u32()?),
            5 => CpEntry::Long(((r.u32()? as u64) << 32) | r.u32()? as u64),
            6 => CpEntry::Double(((r.u32()? as u64) << 32) | r.u32()? as u64),
            7 => CpEntry::Class(r.u16()?),
            8 => CpEntry::String(r.u16()?),
            9 => CpEntry::FieldRef(r.u16()?, r.u16()?),
            10 => CpEntry::MethodRef(r.u16()?, r.u16()?),
            11 => CpEntry::InterfaceMethodRef(r.u16()?, r.u16()?),
            12 => CpEntry::NameAndType(r.u16()?, r.u16()?),
            15 => CpEntry::MethodHandle(r.u8()?, r.u16()?),
            16 => CpEntry::MethodType(r.u16()?),
            17 => CpEntry::Dynamic(r.u16()?, r.u16()?),
            18 => CpEntry::InvokeDynamic(r.u16()?, r.u16()?),
            19 => CpEntry::Module(r.u16()?),
            20 => CpEntry::Package(r.u16()?),
            t => return Err(Error::BadConstantPool(t)),
        };
        let slots = entry.slots();
        constant_pool[i] = Some(entry);
        i += slots;
    }

    let access_flags = r.u16()?;
    let this_class = r.u16()?;
    let super_class = r.u16()?;

    let iface_count = r.u16()? as usize;
    let mut interfaces = Vec::with_capacity(iface_count);
    for _ in 0..iface_count {
        interfaces.push(r.u16()?);
    }

    let fields = read_members(&mut r)?;
    let methods = read_members(&mut r)?;
    let attributes = read_attributes(&mut r)?;

    Ok(ClassFile {
        minor_version,
        major_version,
        constant_pool,
        access_flags,
        this_class,
        super_class,
        interfaces,
        fields,
        methods,
        attributes,
    })
}

fn read_members(r: &mut Reader<'_>) -> Result<Vec<Member>> {
    let count = r.u16()? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(Member {
            access_flags: r.u16()?,
            name_index: r.u16()?,
            descriptor_index: r.u16()?,
            attributes: read_attributes(r)?,
        });
    }
    Ok(out)
}

fn read_attributes(r: &mut Reader<'_>) -> Result<Vec<Attribute>> {
    let count = r.u16()? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let name_index = r.u16()?;
        let len = r.u32()? as usize;
        out.push(Attribute { name_index, info: r.bytes(len)?.to_vec() });
    }
    Ok(out)
}

// ---------------------------------------------------------------- write

pub fn write(cf: &ClassFile) -> Vec<u8> {
    let mut out = Vec::with_capacity(1024);
    out.extend_from_slice(&0xCAFE_BABEu32.to_be_bytes());
    out.extend_from_slice(&cf.minor_version.to_be_bytes());
    out.extend_from_slice(&cf.major_version.to_be_bytes());
    out.extend_from_slice(&(cf.constant_pool.len() as u16).to_be_bytes());
    for entry in cf.constant_pool.iter().skip(1) {
        if let Some(e) = entry {
            write_cp(&mut out, e);
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

fn write_cp(out: &mut Vec<u8>, e: &CpEntry) {
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

fn write_members(out: &mut Vec<u8>, members: &[Member]) {
    out.extend_from_slice(&(members.len() as u16).to_be_bytes());
    for m in members {
        out.extend_from_slice(&m.access_flags.to_be_bytes());
        out.extend_from_slice(&m.name_index.to_be_bytes());
        out.extend_from_slice(&m.descriptor_index.to_be_bytes());
        write_attributes(out, &m.attributes);
    }
}

fn write_attributes(out: &mut Vec<u8>, attrs: &[Attribute]) {
    out.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
    for a in attrs {
        out.extend_from_slice(&a.name_index.to_be_bytes());
        out.extend_from_slice(&(a.info.len() as u32).to_be_bytes());
        out.extend_from_slice(&a.info);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(bytes: &[u8]) -> Vec<u8> {
        let cf = parse(bytes).expect("parse");
        write(&cf)
    }

    #[test]
    fn roundtrip_is_byte_identical() {
        // 用 javac 编译出来的 class 文件（仓库内 samples/）
        let dir = std::path::Path::new("samples");
        if !dir.exists() {
            return;
        }
        let mut checked = 0;
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("class") {
                continue;
            }
            let bytes = std::fs::read(&path).expect("read");
            let out = roundtrip(&bytes);
            assert_eq!(bytes, out, "往返不一致: {}", path.display());
            checked += 1;
        }
        assert!(checked > 0, "samples/ 里没有 class 文件");
    }

    #[test]
    fn reads_this_class_name() {
        let dir = std::path::Path::new("samples");
        if !dir.exists() {
            return;
        }
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("class") {
                continue;
            }
            let bytes = std::fs::read(&path).expect("read");
            let cf = parse(&bytes).expect("parse");
            assert!(cf.this_class_name().is_some());
            assert_eq!(cf.major(), 69);
        }
    }
}
