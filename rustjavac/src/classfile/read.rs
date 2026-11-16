//! 读：magic、版本、常量池、字段/方法、属性。

use super::attr::read_attributes;
use super::{ClassFile, CpEntry, Error, Member, Result};

pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        if self.pos >= self.buf.len() {
            return Err(Error::Truncated("u8"));
        }
        let v = self.buf[self.pos];
        self.pos += 1;
        Ok(v)
    }

    pub(crate) fn u16(&mut self) -> Result<u16> {
        if self.pos + 2 > self.buf.len() {
            return Err(Error::Truncated("u16"));
        }
        let v = u16::from_be_bytes([self.buf[self.pos], self.buf[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    pub(crate) fn u32(&mut self) -> Result<u32> {
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

    pub(crate) fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            return Err(Error::Truncated("bytes"));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
}

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
