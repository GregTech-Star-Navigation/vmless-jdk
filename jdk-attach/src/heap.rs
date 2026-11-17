//! 堆相关的产物：类直方图、类加载器统计、HPROF 导出。
//!
//! # HPROF 格式（`format=b`，`JAVA PROFILE 1.0.2`）
//!
//! ```text
//! header : "JAVA PROFILE 1.0.2\0"  u1 id_size(=8)  u8 timestamp_ms
//! record : u1 tag  u4 usec_delta  u4 length  body[length]
//!
//! 0x01 STRING              u8 id + utf8
//! 0x02 LOAD_CLASS          u4 serial, u8 class_id, u4 stack_serial, u8 name_string_id
//! 0x1C HEAP_DUMP_SEGMENT   sub-records（见下）
//! 0x2C HEAP_DUMP_END       空
//!
//! 子记录
//!   0x05 ROOT_STICKY_CLASS  u8 class_id
//!   0x20 CLASS_DUMP         u8 class_id, u4 stack, u8 super, u8 loader, u8 signers,
//!                           u8 prot_domain, u8 reserved1, u8 reserved2, u4 instance_size,
//!                           u2 cp_count [cp], u2 static_count [static],
//!                           u2 inst_field_count [u8 name_id, u1 type]
//!   0x21 INSTANCE_DUMP      u8 id, u4 stack, u8 class_id, u4 num_bytes, bytes
//! ```
//!
//! 顺序上的硬约束：`LOAD_CLASS` 必须在 `CLASS_DUMP` 之前（读端靠它把 class_id 映到名字），
//! `HEAP_DUMP_END` 必须在最后（否则读端认为文件被截断）。子记录**不能跨段**。
//!
//! 校验见 [`crate::hprof`]（按规范写的严格解析器）。

use std::collections::HashMap;
use std::io::{self, Write};

use crate::protocol::{Buf, Cur};

// ---------------------------------------------------------------------------
// 类直方图
// ---------------------------------------------------------------------------

/// `jmap -histo` / `jcmd GC.class_histogram` 的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassHisto {
    pub class_name: String,
    pub module: Option<String>,
    pub instances: u64,
    pub bytes: u64,
}

impl ClassHisto {
    pub fn to_wire(v: &[ClassHisto]) -> Vec<u8> {
        let mut b = Buf::new();
        b.u32(v.len() as u32);
        for h in v {
            b.u64(h.instances).u64(h.bytes).str(&h.class_name);
            match &h.module {
                Some(m) => {
                    b.u8(1).str(m);
                }
                None => {
                    b.u8(0);
                }
            }
        }
        b.into_vec()
    }

    pub fn from_wire(d: &[u8]) -> Option<Vec<ClassHisto>> {
        let mut c = Cur::new(d);
        let n = c.u32()? as usize;
        if n > 4_000_000 {
            return None;
        }
        let mut out = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            let instances = c.u64()?;
            let bytes = c.u64()?;
            let class_name = c.str()?;
            let has = c.u8()?;
            let module = if has == 1 { Some(c.str()?) } else { None };
            out.push(ClassHisto { class_name, module, instances, bytes });
        }
        Some(out)
    }

    /// 按 #bytes 降序（与 `jmap -histo` 一致）。
    pub fn sort_by_bytes(v: &mut [ClassHisto]) {
        v.sort_by(|a, b| {
            b.bytes
                .cmp(&a.bytes)
                .then(b.instances.cmp(&a.instances))
                .then(a.class_name.cmp(&b.class_name))
        });
    }
}

// ---------------------------------------------------------------------------
// 类加载器统计（jmap -clstats）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoaderStat {
    pub class_loader: u64,
    pub parent: u64,
    pub cld: u64,
    pub classes: u64,
    pub chunk_size: u64,
    pub block_size: u64,
    pub loader_type: String,
    pub hidden: bool,
}

impl LoaderStat {
    pub fn to_wire(v: &[LoaderStat]) -> Vec<u8> {
        let mut b = Buf::new();
        b.u32(v.len() as u32);
        for l in v {
            b.u64(l.class_loader)
                .u64(l.parent)
                .u64(l.cld)
                .u64(l.classes)
                .u64(l.chunk_size)
                .u64(l.block_size)
                .str(&l.loader_type)
                .u8(if l.hidden { 1 } else { 0 });
        }
        b.into_vec()
    }
    pub fn from_wire(d: &[u8]) -> Option<Vec<LoaderStat>> {
        let mut c = Cur::new(d);
        let n = c.u32()? as usize;
        if n > 1_000_000 {
            return None;
        }
        let mut out = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            out.push(LoaderStat {
                class_loader: c.u64()?,
                parent: c.u64()?,
                cld: c.u64()?,
                classes: c.u64()?,
                chunk_size: c.u64()?,
                block_size: c.u64()?,
                loader_type: c.str()?,
                hidden: c.u8()? == 1,
            });
        }
        Some(out)
    }
}

// ---------------------------------------------------------------------------
// 堆模型（VM → hprof）
// ---------------------------------------------------------------------------

/// HPROF 的字段类型码。
///
/// **不是 ASCII**：HPROF 用 JVM 的类型编号（`hprof_type`）——
/// `2=object 4=boolean 5=char 6=float 7=double 8=byte 9=short 10=int 11=long`。
/// 写成 `'I'` 这种 ASCII 会让读端报 `未知的类型码 0x02/0x49`。
pub fn hprof_type_code(ty: char) -> u8 {
    match ty {
        'L' | '[' => 2,
        'Z' => 4,
        'C' => 5,
        'F' => 6,
        'D' => 7,
        'B' => 8,
        'S' => 9,
        'I' => 10,
        'J' => 11,
        // 没写类型就当成 byte（不会崩，读端也认）
        _ => 8,
    }
}

/// 该类型码在文件里占几个字节（object 占 id 大小）。
pub fn hprof_type_size(code: u8) -> Option<usize> {
    Some(match code {
        2 => 8,          // object（我们固定 id_size=8）
        4 | 8 => 1,      // boolean / byte
        5 | 9 => 2,      // char / short
        6 | 10 => 4,     // float / int
        7 | 11 => 8,     // double / long
        _ => return None,
    })
}

/// 一个实例字段。
#[derive(Debug, Clone)]
pub struct FieldDef {
    pub name: String,
    /// Java 类型字符：`B C D F I J S Z L [`（写文件时转成 [`hprof_type_code`]）
    pub ty: char,
    pub size: u32,
}

/// 一个类。
#[derive(Debug, Clone)]
pub struct ClassDef {
    pub name: String,
    pub super_name: Option<String>,
    pub instance_size: u32,
    pub fields: Vec<FieldDef>,
}

/// 一个对象。
#[derive(Debug, Clone)]
pub struct HeapObject {
    pub class_name: String,
    /// 字段数据，长度应等于类的 `instance_size`
    pub data: Vec<u8>,
}

/// 一次堆快照的**结构**（值语义由 `data` 承载）。
#[derive(Debug, Clone, Default)]
pub struct HeapModel {
    pub classes: Vec<ClassDef>,
    pub objects: Vec<HeapObject>,
    pub jvm_version: String,
}

impl HeapModel {
    /// 从对象列表推导类定义（VM 只给名字/尺寸时的兜底路径）。
    pub fn from_objects(objects: Vec<HeapObject>) -> HeapModel {
        let mut seen: HashMap<String, usize> = HashMap::new();
        let mut classes: Vec<ClassDef> = Vec::new();
        for o in &objects {
            if seen.contains_key(&o.class_name) {
                continue;
            }
            seen.insert(o.class_name.clone(), classes.len());
            classes.push(ClassDef {
                name: o.class_name.clone(),
                super_name: None,
                instance_size: o.data.len() as u32,
                fields: Vec::new(),
            });
        }
        HeapModel { classes, objects, jvm_version: String::new() }
    }
}

const TAG_STRING: u8 = 0x01;
const TAG_LOAD_CLASS: u8 = 0x02;
const TAG_HEAP_DUMP_SEGMENT: u8 = 0x1c;
const TAG_HEAP_DUMP_END: u8 = 0x2c;

const SUB_ROOT_STICKY_CLASS: u8 = 0x05;
const SUB_CLASS_DUMP: u8 = 0x20;
const SUB_INSTANCE_DUMP: u8 = 0x21;

const ID_SIZE: u32 = 8;
/// 单个 HEAP_DUMP_SEGMENT 的目标上限（子记录不跨段）。
const SEGMENT_TARGET: usize = 512 * 1024;

/// HPROF 写端。所有 id 都是 8 字节，且在文件内唯一。
pub struct HprofWriter<W: Write> {
    w: W,
    next_id: u64,
    string_ids: HashMap<String, u64>,
    class_serials: HashMap<String, u32>,
    used_ids: std::collections::HashSet<u64>,
}

impl<W: Write> HprofWriter<W> {
    pub fn new(w: W, ts_ms: u64) -> io::Result<HprofWriter<W>> {
        let mut w = w;
        w.write_all(b"JAVA PROFILE 1.0.2")?;
        w.write_all(&[0u8])?;
        w.write_all(&ID_SIZE.to_be_bytes())?; // u4，不是 u1
        w.write_all(&ts_ms.to_be_bytes())?;
        Ok(HprofWriter {
            w,
            next_id: 0x1000,
            string_ids: HashMap::new(),
            class_serials: HashMap::new(),
            used_ids: std::collections::HashSet::new(),
        })
    }

    fn alloc_id(&mut self) -> u64 {
        loop {
            let id = self.next_id;
            self.next_id += 8;
            if self.used_ids.insert(id) {
                return id;
            }
        }
    }

    fn record(&mut self, tag: u8, body: &[u8]) -> io::Result<()> {
        // 记录头：tag(1) + usec_delta(4，恒 0) + length(4)
        self.w.write_all(&[tag])?;
        self.w.write_all(&0u32.to_be_bytes())?;
        self.w.write_all(&(body.len() as u32).to_be_bytes())?;
        self.w.write_all(body)
    }

    fn string_id(&mut self, s: &str) -> io::Result<u64> {
        if let Some(id) = self.string_ids.get(s) {
            return Ok(*id);
        }
        let id = self.alloc_id();
        let mut body = Vec::with_capacity(8 + s.len());
        body.extend_from_slice(&id.to_be_bytes());
        body.extend_from_slice(s.as_bytes());
        self.record(TAG_STRING, &body)?;
        self.string_ids.insert(s.to_string(), id);
        Ok(id)
    }

    /// 写整个堆。
    pub fn write_heap(mut self, model: &HeapModel) -> io::Result<W> {
        // ① 所有字符串（类名 + 字段名 + 超类名）
        let mut class_ids: HashMap<String, u64> = HashMap::new();
        let mut class_name_ids: HashMap<String, u64> = HashMap::new();
        for (i, c) in model.classes.iter().enumerate() {
            let nid = self.string_id(&c.name)?;
            class_name_ids.insert(c.name.clone(), nid);
            let cid = self.alloc_id();
            class_ids.insert(c.name.clone(), cid);
            self.class_serials.insert(c.name.clone(), i as u32 + 1);
        }
        for c in &model.classes {
            for f in &c.fields {
                self.string_id(&f.name)?;
            }
            if let Some(s) = &c.super_name {
                self.string_id(s)?;
            }
        }

        // ② LOAD_CLASS（必须在 CLASS_DUMP 之前）
        for c in &model.classes {
            let cid = match class_ids.get(&c.name) {
                Some(v) => *v,
                None => continue,
            };
            let serial = *self.class_serials.get(&c.name).unwrap_or(&0);
            let nid = *class_name_ids.get(&c.name).unwrap_or(&cid);
            let mut body = Vec::with_capacity(24);
            body.extend_from_slice(&serial.to_be_bytes());
            body.extend_from_slice(&cid.to_be_bytes());
            body.extend_from_slice(&0u32.to_be_bytes()); // stack trace serial
            body.extend_from_slice(&nid.to_be_bytes());
            self.record(TAG_LOAD_CLASS, &body)?;
        }

        // ③ 堆段
        let mut seg: Vec<u8> = Vec::with_capacity(SEGMENT_TARGET);
        fn flush(seg: &mut Vec<u8>, w: &mut impl Write) -> io::Result<()> {
            if seg.is_empty() {
                return Ok(());
            }
            w.write_all(&[TAG_HEAP_DUMP_SEGMENT])?;
            w.write_all(&0u32.to_be_bytes())?;
            w.write_all(&(seg.len() as u32).to_be_bytes())?;
            w.write_all(seg)?;
            seg.clear();
            Ok(())
        }

        // 3a. ROOT_STICKY_CLASS：每个类一个根
        for c in &model.classes {
            if let Some(cid) = class_ids.get(&c.name) {
                let mut sub = Vec::with_capacity(9);
                sub.push(SUB_ROOT_STICKY_CLASS);
                sub.extend_from_slice(&cid.to_be_bytes());
                if seg.len() + sub.len() > SEGMENT_TARGET {
                    flush(&mut seg, &mut self.w)?;
                }
                seg.extend_from_slice(&sub);
            }
        }

        // 3b. CLASS_DUMP
        for c in &model.classes {
            let cid = match class_ids.get(&c.name) {
                Some(v) => *v,
                None => continue,
            };
            let super_id = c
                .super_name
                .as_ref()
                .and_then(|s| class_ids.get(s))
                .copied()
                .unwrap_or(0);
            let mut sub = Vec::with_capacity(64 + c.fields.len() * 9);
            sub.push(SUB_CLASS_DUMP);
            sub.extend_from_slice(&cid.to_be_bytes());
            sub.extend_from_slice(&0u32.to_be_bytes()); // stack trace serial
            sub.extend_from_slice(&super_id.to_be_bytes());
            sub.extend_from_slice(&0u64.to_be_bytes()); // class loader
            sub.extend_from_slice(&0u64.to_be_bytes()); // signers
            sub.extend_from_slice(&0u64.to_be_bytes()); // protection domain
            sub.extend_from_slice(&0u64.to_be_bytes()); // reserved1
            sub.extend_from_slice(&0u64.to_be_bytes()); // reserved2
            sub.extend_from_slice(&c.instance_size.to_be_bytes());
            // 常量池：HPROF 规范里实例字段的"名字"是该类自己的常量池**索引（u2）**，
            // 不是全局 8 字节 id。这里给每个字段建一条常量池记录，索引从 1 开始。
            sub.extend_from_slice(&(c.fields.len() as u16).to_be_bytes());
            for (i, f) in c.fields.iter().enumerate() {
                let _ = i;
                sub.extend_from_slice(&((i + 1) as u16).to_be_bytes());
                let code = hprof_type_code(f.ty);
                sub.push(code);
                // 常量池里要带一个该类型的值（我们不关心，填 0）
                let sz = hprof_type_size(code).unwrap_or(8);
                sub.extend(std::iter::repeat(0u8).take(sz));
            }
            sub.extend_from_slice(&0u16.to_be_bytes()); // static fields
            sub.extend_from_slice(&(c.fields.len() as u16).to_be_bytes());
            for (i, f) in c.fields.iter().enumerate() {
                sub.extend_from_slice(&((i + 1) as u16).to_be_bytes());
                sub.push(hprof_type_code(f.ty));
            }
            if seg.len() + sub.len() > SEGMENT_TARGET {
                flush(&mut seg, &mut self.w)?;
            }
            seg.extend_from_slice(&sub);
        }

        // 3c. INSTANCE_DUMP
        for o in &model.objects {
            let cid = match class_ids.get(&o.class_name) {
                Some(v) => *v,
                None => continue,
            };
            let oid = self.alloc_id();
            let mut sub = Vec::with_capacity(25 + o.data.len());
            sub.push(SUB_INSTANCE_DUMP);
            sub.extend_from_slice(&oid.to_be_bytes());
            sub.extend_from_slice(&0u32.to_be_bytes()); // stack trace serial
            sub.extend_from_slice(&cid.to_be_bytes());
            sub.extend_from_slice(&(o.data.len() as u32).to_be_bytes());
            sub.extend_from_slice(&o.data);
            if seg.len() + sub.len() > SEGMENT_TARGET {
                flush(&mut seg, &mut self.w)?;
            }
            seg.extend_from_slice(&sub);
        }
        flush(&mut seg, &mut self.w)?;

        // ④ 结束标记（少了它读端会认为文件被截断）
        self.record(TAG_HEAP_DUMP_END, &[])?;
        Ok(self.w)
    }
}

/// 便捷入口：把模型写成 hprof。
pub fn write_hprof<W: Write>(model: &HeapModel, w: W, ts_ms: u64) -> io::Result<W> {
    HprofWriter::new(w, ts_ms)?.write_heap(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> HeapModel {
        HeapModel {
            classes: vec![
                ClassDef { name: "java.lang.Object".into(), super_name: None, instance_size: 0, fields: vec![] },
                ClassDef {
                    name: "com.example.Point".into(),
                    super_name: Some("java.lang.Object".into()),
                    instance_size: 8,
                    fields: vec![
                        FieldDef { name: "x".into(), ty: 'I', size: 4 },
                        FieldDef { name: "y".into(), ty: 'I', size: 4 },
                    ],
                },
            ],
            objects: vec![
                HeapObject { class_name: "com.example.Point".into(), data: vec![1, 0, 0, 0, 2, 0, 0, 0] },
                HeapObject { class_name: "com.example.Point".into(), data: vec![0u8; 8] },
            ],
            jvm_version: "0.1.0".into(),
        }
    }

    #[test]
    fn hprof_header_and_tags() {
        let mut out = Vec::new();
        write_hprof(&model(), &mut out, 1234).unwrap();
        assert!(out.starts_with(b"JAVA PROFILE 1.0.2"));
        assert_eq!(out[18], 0); // 结尾 NUL
        // id_size 是 u4 大端（19..23），时间戳是 u8 大端（23..31）
        assert_eq!(u32::from_be_bytes([out[19], out[20], out[21], out[22]]), 8);
        let mut ts = [0u8; 8];
        ts.copy_from_slice(&out[23..31]);
        assert_eq!(u64::from_be_bytes(ts), 1234);
        assert!(out.windows(17).any(|w| w == b"com.example.Point"));
        assert!(out.windows(16).any(|w| w == b"java.lang.Object"));
    }

    #[test]
    fn hprof_is_parseable_by_strict_parser() {
        let mut out = Vec::new();
        write_hprof(&model(), &mut out, 1).unwrap();
        let s = crate::hprof::inspect(&out).unwrap_or_else(|e| panic!("HPROF 结构解析失败: {}", e));
        assert_eq!(s.class_dumps, 2);
        assert_eq!(s.instance_dumps, 2);
        // 2 个类名 + Point 的 2 个字段名（超类名就是 Object，去重后共 4 条）
        assert_eq!(s.strings, 4);
        assert_eq!(s.load_classes, 2);
        assert!(s.sticky_roots >= 2);
        assert!(s.segments >= 1);
        assert!(s.ended);
    }

    #[test]
    fn histo_wire_roundtrip() {
        let v = vec![
            ClassHisto { class_name: "A".into(), module: None, instances: 3, bytes: 30 },
            ClassHisto { class_name: "[B".into(), module: Some("java.base".into()), instances: 2, bytes: 20 },
        ];
        let w = ClassHisto::to_wire(&v);
        assert_eq!(ClassHisto::from_wire(&w).unwrap(), v);
    }

    #[test]
    fn histo_wire_rejects_truncated() {
        let v = vec![ClassHisto { class_name: "A".into(), module: None, instances: 3, bytes: 30 }];
        let w = ClassHisto::to_wire(&v);
        assert!(ClassHisto::from_wire(&w[..w.len() - 2]).is_none());
    }

    #[test]
    fn loader_wire_roundtrip() {
        let v = vec![LoaderStat {
            class_loader: 1,
            parent: 0,
            cld: 2,
            classes: 7,
            chunk_size: 10,
            block_size: 9,
            loader_type: "app".into(),
            hidden: false,
        }];
        let w = LoaderStat::to_wire(&v);
        assert_eq!(LoaderStat::from_wire(&w).unwrap(), v);
    }
}
