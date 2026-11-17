//! HPROF 结构校验器。
//!
//! # 为什么需要一个自己写的解析器
//!
//! `jhat` 在 JDK 9 就被删掉了，`jhsdb` 只能看**活进程**不能读文件。所以"这个 hprof 能不能
//! 被真工具打开"没法用真工具直接证明。能做到的最强证据是：**按规范写一个严格解析器**，
//! 让它既能解析我们自己的 dump，也能解析真 `jmap -dump` 产出的 dump ——
//! 后者证明解析器本身没写错（见 `docs/hprof.md` 的实测记录）。
//!
//! 校验的严格程度：**任何一处对不上就报错**，不猜、不跳过。因为宽容的解析器会把真正的
//! 结构错误隐藏起来，也就失去了校验的意义。

/// 一份 hprof 的结构摘要。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HprofSummary {
    pub id_size: u32,
    pub timestamp_ms: u64,
    pub strings: usize,
    pub load_classes: usize,
    pub class_dumps: usize,
    pub instance_dumps: usize,
    pub sticky_roots: usize,
    pub segments: usize,
    pub total_bytes: usize,
    pub ended: bool,
    /// STRING 记录里出现的前若干个名字（看一下是不是合理的类名）
    pub first_strings: Vec<String>,
}

const HEADER: &[u8] = b"JAVA PROFILE 1.0.2";
const TAG_STRING: u8 = 0x01;
const TAG_LOAD_CLASS: u8 = 0x02;
const TAG_HEAP_DUMP_SEGMENT: u8 = 0x1c;
const TAG_HEAP_DUMP_END: u8 = 0x2c;

// 子记录编号（OpenJDK 的 heapHprofBinWriter 用的就是这套）
const SUB_ROOT_UNKNOWN: u8 = 0xff;
const SUB_ROOT_JNI_GLOBAL: u8 = 0x01;
const SUB_ROOT_JNI_LOCAL: u8 = 0x02;
const SUB_ROOT_JAVA_FRAME: u8 = 0x03;
const SUB_ROOT_NATIVE_STACK: u8 = 0x04;
const SUB_ROOT_STICKY_CLASS: u8 = 0x05;
const SUB_ROOT_THREAD_BLOCK: u8 = 0x06;
const SUB_ROOT_MONITOR_USED: u8 = 0x07;
const SUB_ROOT_THREAD_OBJECT: u8 = 0x08;
const SUB_ROOT_INTERNED_STRING: u8 = 0x89;
const SUB_HEAP_DUMP_INFO: u8 = 0xfe;
const SUB_CLASS_DUMP: u8 = 0x20;
const SUB_INSTANCE_DUMP: u8 = 0x21;
const SUB_OBJ_ARRAY_DUMP: u8 = 0x22;
const SUB_PRIM_ARRAY_DUMP: u8 = 0x23;

/// 严格解析。**任何结构不合法都返回 Err**（不 panic）。
pub fn inspect(d: &[u8]) -> Result<HprofSummary, String> {
    let mut p = Parser::new(d)?;
    p.run()?;
    Ok(HprofSummary {
        id_size: p.id_size as u32,
        timestamp_ms: p.timestamp_ms,
        strings: p.strings,
        load_classes: p.load_classes,
        class_dumps: p.class_dumps,
        instance_dumps: p.instance_dumps,
        sticky_roots: p.sticky_roots,
        segments: p.segments,
        total_bytes: d.len(),
        ended: p.ended,
        first_strings: p.first_strings,
    })
}

struct Parser<'a> {
    d: &'a [u8],
    id_size: u8,
    timestamp_ms: u64,
    strings: usize,
    load_classes: usize,
    class_dumps: usize,
    instance_dumps: usize,
    sticky_roots: usize,
    segments: usize,
    ended: bool,
    first_strings: Vec<String>,
}

impl<'a> Parser<'a> {
    fn new(d: &'a [u8]) -> Result<Parser<'a>, String> {
        // header 后面还有一个结尾 NUL，然后是 id_size、timestamp
        // header：19 字节字符串（含 NUL）+ u4 id_size + u8 timestamp，全部大端
        if d.len() < HEADER.len() + 1 + 4 + 8 {
            return Err("文件太短，连 header 都不够".into());
        }
        if &d[..HEADER.len()] != HEADER {
            return Err(format!(
                "魔数不对: 期望 {:?}，实际 {:?}",
                String::from_utf8_lossy(HEADER),
                String::from_utf8_lossy(&d[..HEADER.len().min(d.len())])
            ));
        }
        if d[HEADER.len()] != 0 {
            return Err("header 后面缺少结尾 NUL".into());
        }
        let idoff = HEADER.len() + 1;
        let id_size = u32::from_be_bytes([d[idoff], d[idoff + 1], d[idoff + 2], d[idoff + 3]]);
        if id_size != 8 && id_size != 4 {
            return Err(format!("id_size 只能是 4 或 8，实际 {}", id_size));
        }
        let mut ts = [0u8; 8];
        ts.copy_from_slice(&d[idoff + 4..idoff + 12]);
        Ok(Parser {
            d,
            id_size: id_size as u8,
            timestamp_ms: u64::from_be_bytes(ts),
            strings: 0,
            load_classes: 0,
            class_dumps: 0,
            instance_dumps: 0,
            sticky_roots: 0,
            segments: 0,
            ended: false,
            first_strings: Vec::new(),
        })
    }

    fn id(&self) -> usize {
        self.id_size as usize
    }

    fn run(&mut self) -> Result<(), String> {
        let mut p = HEADER.len() + 1 + 4 + 8;
        while p < self.d.len() {
            if p + 9 > self.d.len() {
                return Err(format!("偏移 {} 处的记录头被截断", p));
            }
            let tag = self.d[p];
            let len = u32::from_be_bytes([self.d[p + 5], self.d[p + 6], self.d[p + 7], self.d[p + 8]]) as usize;
            let body = self
                .d
                .get(p + 9..p + 9 + len)
                .ok_or_else(|| format!("偏移 {} 的记录声明长度 {} 越界", p, len))?;
            match tag {
                TAG_STRING => {
                    if len < self.id() {
                        return Err(format!("偏移 {} 的 STRING 记录太短", p));
                    }
                    self.strings += 1;
                    if self.first_strings.len() < 8 {
                        let raw = &body[self.id()..];
                        let s = String::from_utf8_lossy(raw).to_string();
                        if !s.is_empty() {
                            self.first_strings.push(s);
                        }
                    }
                }
                TAG_LOAD_CLASS => {
                    if len < 8 {
                        return Err(format!("偏移 {} 的 LOAD_CLASS 记录太短", p));
                    }
                    self.load_classes += 1;
                }
                TAG_HEAP_DUMP_SEGMENT => {
                    self.segments += 1;
                    self.walk_segment(body)?;
                }
                TAG_HEAP_DUMP_END => self.ended = true,
                _ => {}
            }
            p += 9 + len;
        }
        if !self.ended {
            return Err("没有 HEAP_DUMP_END 记录：读端会认为文件被截断".into());
        }
        Ok(())
    }

    fn walk_segment(&mut self, body: &[u8]) -> Result<(), String> {
        let mut b = body;
        let id = self.id();
        while !b.is_empty() {
            let sub = b[0];
            let need = |n: usize| -> Result<(), String> {
                if b.len() < n {
                    Err(format!("堆段子记录 0x{:02x} 需要 {} 字节，只剩 {}", sub, n, b.len()))
                } else {
                    Ok(())
                }
            };
            match sub {
                SUB_ROOT_STICKY_CLASS => {
                    need(1 + id)?;
                    self.sticky_roots += 1;
                    b = &b[1 + id..];
                }
                SUB_ROOT_UNKNOWN | SUB_ROOT_MONITOR_USED | SUB_ROOT_INTERNED_STRING => {
                    need(1 + id)?;
                    b = &b[1 + id..];
                }
                SUB_ROOT_JNI_GLOBAL => {
                    need(1 + id + id)?;
                    b = &b[1 + id + id..];
                }
                SUB_ROOT_JNI_LOCAL | SUB_ROOT_JAVA_FRAME => {
                    need(1 + id + 4 + 4)?;
                    b = &b[1 + id + 8..];
                }
                SUB_ROOT_NATIVE_STACK | SUB_ROOT_THREAD_BLOCK => {
                    need(1 + id + 4)?;
                    b = &b[1 + id + 4..];
                }
                SUB_ROOT_THREAD_OBJECT => {
                    need(1 + id + 4 + 4)?;
                    b = &b[1 + id + 8..];
                }
                SUB_HEAP_DUMP_INFO => {
                    // u4 heap id + id 名字字符串 id
                    need(1 + 4 + id)?;
                    b = &b[1 + 4 + id..];
                }
                SUB_CLASS_DUMP => {
                    need(1 + id + 4 + id * 6 + 4 + 2 + 2 + 2)?;
                    // sub(1) + cid + stack(4) + 6 个 id + instance_size(4) 之后才是 cp 计数
                    let mut q = 1 + id + 4 + id * 6 + 4;
                    let cp = u16::from_be_bytes([b[q], b[q + 1]]) as usize;
                    q += 2;
                    // 常量池条目长度可变，按类型跳过
                    for _ in 0..cp {
                        if q + 2 + 1 > b.len() {
                            return Err("CLASS_DUMP 常量池越界".into());
                        }
                        q += 2; // 名字 id（u2）+ 类型（u1）
                        q += 1;
                        q += self.value_size(b[q - 1])?;
                    }
                    if q + 2 > b.len() {
                        return Err("CLASS_DUMP 静态字段计数越界".into());
                    }
                    let sc = u16::from_be_bytes([b[q], b[q + 1]]) as usize;
                    q += 2;
                    for _ in 0..sc {
                        if q + 2 + 1 > b.len() {
                            return Err("CLASS_DUMP 静态字段越界".into());
                        }
                        q += 2;
                        q += 1;
                        q += self.value_size(b[q - 1])?;
                    }
                    if q + 2 > b.len() {
                        return Err("CLASS_DUMP 实例字段计数越界".into());
                    }
                    let ic = u16::from_be_bytes([b[q], b[q + 1]]) as usize;
                    q += 2 + ic * (2 + 1);
                    if q > b.len() {
                        return Err("CLASS_DUMP 实例字段越界".into());
                    }
                    self.class_dumps += 1;
                    b = &b[q..];
                }
                SUB_INSTANCE_DUMP => {
                    need(1 + id + 4 + id + 4)?;
                    let nb_off = 1 + id + 4 + id;
                    let nb = u32::from_be_bytes([b[nb_off], b[nb_off + 1], b[nb_off + 2], b[nb_off + 3]]) as usize;
                    let total = nb_off + 4 + nb;
                    if b.len() < total {
                        return Err(format!("INSTANCE_DUMP 声明 {} 字节，实际不够", nb));
                    }
                    self.instance_dumps += 1;
                    b = &b[total..];
                }
                SUB_OBJ_ARRAY_DUMP => {
                    need(1 + id + 4 + 4 + id + 4)?;
                    let n_off = 1 + id + 4 + 4 + id;
                    let n = u32::from_be_bytes([b[n_off], b[n_off + 1], b[n_off + 2], b[n_off + 3]]) as usize;
                    let total = n_off + 4 + n * id;
                    if b.len() < total {
                        return Err("OBJ_ARRAY_DUMP 越界".into());
                    }
                    b = &b[total..];
                }
                SUB_PRIM_ARRAY_DUMP => {
                    need(1 + id + 4 + 4 + 1 + 4)?;
                    let ty = b[1 + id + 4 + 4];
                    let n_off = 1 + id + 4 + 4 + 1;
                    let n = u32::from_be_bytes([b[n_off], b[n_off + 1], b[n_off + 2], b[n_off + 3]]) as usize;
                    let total = n_off + 4 + n * self.value_size(ty)?;
                    if b.len() < total {
                        return Err("PRIM_ARRAY_DUMP 越界".into());
                    }
                    b = &b[total..];
                }
                other => {
                    return Err(format!("堆段里有未知子记录 0x{:02x}", other));
                }
            }
        }
        Ok(())
    }

    /// 基本类型的宽度。HPROF 的类型码是 **JVM 类型编号**（2=object 4=boolean … 11=long），
    /// 不是 ASCII。
    fn value_size(&self, code: u8) -> Result<usize, String> {
        Ok(match code {
            2 => self.id(),  // object
            4 | 8 => 1,      // boolean / byte
            5 | 9 => 2,      // char / short
            6 | 10 => 4,     // float / int
            7 | 11 => 8,     // double / long
            other => return Err(format!("未知的类型码 {}（HPROF 用 2,4..11，不是 ASCII）", other)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_garbage() {
        assert!(inspect(b"not a heapprofile").is_err());
        assert!(inspect(b"").is_err());
        let mut d = HEADER.to_vec();
        d.push(8);
        d.extend_from_slice(&0u64.to_le_bytes());
        // 没有 HEAP_DUMP_END
        assert!(inspect(&d).is_err());
    }

    #[test]
    fn accepts_our_writer_output() {
        use crate::heap::{write_hprof, ClassDef, FieldDef, HeapModel, HeapObject};
        let model = HeapModel {
            classes: vec![
                ClassDef { name: "A".into(), super_name: None, instance_size: 4, fields: vec![] },
                ClassDef {
                    name: "B".into(),
                    super_name: Some("A".into()),
                    instance_size: 8,
                    fields: vec![
                        FieldDef { name: "x".into(), ty: 'I', size: 4 },
                        FieldDef { name: "y".into(), ty: 'I', size: 4 },
                    ],
                },
            ],
            objects: vec![
                HeapObject { class_name: "B".into(), data: vec![0u8; 8] },
                HeapObject { class_name: "B".into(), data: vec![1u8; 8] },
            ],
            jvm_version: "0.1.0".into(),
        };
        let mut out = Vec::new();
        write_hprof(&model, &mut out, 12345).unwrap();
        let s = inspect(&out).unwrap_or_else(|e| panic!("自己写的 hprof 必须能过严格校验: {}", e));
        assert_eq!(s.class_dumps, 2);
        assert_eq!(s.instance_dumps, 2);
        assert_eq!(s.load_classes, 2);
        assert!(s.segments >= 1);
        assert!(s.ended);
        assert_eq!(s.timestamp_ms, 12345);
        assert!(s.first_strings.iter().any(|n| n == "B"), "{:?}", s.first_strings);
    }
}
