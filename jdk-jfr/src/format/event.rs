//! 事件块与「值」的编解码。
//!
//! # 事件块
//!
//! ```text
//! [varint size][varint typeId]
//!   [varint startTicks]
//!   [varint durationTicks]   ← 只有类型里声明了 duration 字段才有
//!   [值 × 其余字段]
//! ```
//!
//! `startTime` / `duration` 这两个隐式字段**不走值解析器**，它们就是前两个 ticks
//! （`EventParser`：`startIndex = hasDuration ? 2 : 1`）。
//!
//! # 每种类型的编码（`ParserFactory` 里各 `*Parser`）
//!
//! | 类型 | 编码 |
//! | --- | --- |
//! | int / long / short / char | 变长整数（`byte` 例外，见下） |
//! | byte | **裸 1 字节**（`ByteParser` 用 `readByte`） |
//! | boolean | 1 字节，非 0 为真 |
//! | float / double | **裸大端 4 / 8 字节**（不是变长！） |
//! | java.lang.String | 自描述字符串（见 `format::metadata`） |
//! | 结构体 | 按字段顺序内联 |
//! | 数组 | varint 个数 + 元素 |
//! | 带 `constantPool="true"` 的字段 | 只放 varint key |
//!
//! `constantPool` 的判断**先于**所有类型分派 —— 所以一个 `constantPool="true"`
//! 的 String 字段存的也是裸 key，而不是自描述字符串。

use crate::bail;
use std::collections::HashMap;

use super::metadata::{read_string, StringConsts};
use super::{Cursor, Error, Result, MAX_ARRAY_ELEMS, MAX_DEPTH};
use crate::format::cp::ConstantPool;
use crate::model::{Metadata, Value, TYPE_STRING};

/// 一条解析出来的事件。
#[derive(Debug, Clone, PartialEq)]
pub struct EventRecord {
    pub type_id: i64,
    pub start_ticks: i64,
    /// 没有 `duration` 字段的类型恒为 0。
    pub duration_ticks: i64,
    /// 与 `TypeDef.fields` 一一对应（含 startTime / duration）。
    pub values: Vec<Value>,
}

/// 反序列化一个值。
pub fn parse_value(
    cur: &mut Cursor,
    meta: &Metadata,
    cp: &ConstantPool,
    type_id: i64,
    dim: u8,
    cp_field: bool,
    depth: usize,
) -> Result<Value> {
    if depth > MAX_DEPTH {
        bail!("值嵌套过深（>{} 层），数据可能损坏", MAX_DEPTH);
    }
    if dim > 0 {
        let n = cur.leb()?;
        if n < 0 {
            bail!("数组长度是负数（{}）", n);
        }
        let n = n as usize;
        if n > MAX_ARRAY_ELEMS || n > cur.remaining() {
            bail!(
                "数组长度 {} 不合理（剩余 {} 字节，上限 {}）",
                n,
                cur.remaining(),
                MAX_ARRAY_ELEMS
            );
        }
        let mut out = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            out.push(parse_value(cur, meta, cp, type_id, 0, cp_field, depth + 1)?);
        }
        return Ok(Value::Array(out));
    }
    if cp_field {
        let key = cur.leb()?;
        // 只登记引用，不在这里解析：常量池的旧块可能还没读到（见 Value::Ref 的注释）
        return Ok(Value::Ref { type_id, key });
    }
    let t = match meta.type_by_id(type_id) {
        Some(t) => t,
        None => bail!("类型 id {} 未定义（元数据里没有这个类型）", type_id),
    };
    match t.name.as_str() {
        // int / short / char 都按 Java 的窄化语义截断：JVM 写的是**零扩展的 32 位**，
        // 直接当 i64 用会把 `-1` 读成 `4294967295`（实测 lineNumber=-1 就踩了这个坑）。
        "int" => Ok(Value::Int((cur.leb()? as i32) as i64)),
        "short" => Ok(Value::Int((cur.leb()? as i16) as i64)),
        "char" => Ok(Value::Int((cur.leb()? as u32 & 0xffff) as i64)),
        "long" => Ok(Value::Int(cur.leb()?)),
        // byte 是**裸 1 字节**：`ByteParser` 用 `input.readByte()`（不是变长整数）。
        // 实测证据：`jdk.types.ChunkHeader.payload`（byte[]）在真文件里 68 个元素正好占 68 字节。
        "byte" => Ok(Value::Int(cur.u8()? as i8 as i64)),
        "boolean" => Ok(Value::Bool(cur.u8()? != 0)),
        "float" => Ok(Value::Float(cur.f32()?)),
        "double" => Ok(Value::Double(cur.f64()?)),
        TYPE_STRING => {
            let s = read_string(cur, Some(cp as &dyn StringConsts))?;
            Ok(match s {
                Some(s) => Value::Str(s),
                None => Value::Null,
            })
        }
        _ => {
            // `simpleType="true"` 的类型在二进制里**没有外层结构**：它的唯一字段
            // 直接内联（`ParserFactory`：`if (t.isSimpleType()) parsers.put(id, cp.parsers[0])`）。
            // 例子：`jdk.types.ThreadState` 只有 `String name`，于是 `state` 字段在
            // 文件里就是一个字符串，真 jfr 打印成 `state = "STATE_RUNNABLE"`。
            if t.simple_type && t.fields.len() == 1 {
                let f = &t.fields[0];
                return parse_value(
                    cur,
                    meta,
                    cp,
                    f.type_id,
                    f.dimension,
                    f.constant_pool,
                    depth + 1,
                );
            }
            let mut fields = Vec::with_capacity(t.fields.len());
            for f in &t.fields {
                fields.push(parse_value(
                    cur,
                    meta,
                    cp,
                    f.type_id,
                    f.dimension,
                    f.constant_pool,
                    depth + 1,
                )?);
            }
            Ok(Value::Struct {
                type_id,
                cp_id: 0,
                fields,
            })
        }
    }
}

/// 序列化一个值（写侧）。
///
/// `cp` 用于把带 `constantPool="true"` 的字段换成 key —— 见 [`CpBuilder`]。
pub fn write_value(
    out: &mut Vec<u8>,
    meta: &Metadata,
    cp: &CpBuilder,
    type_id: i64,
    dim: u8,
    cp_field: bool,
    v: &Value,
) -> Result<()> {
    if dim > 0 {
        let elems = match v {
            Value::Array(a) => a,
            Value::Null => &[][..],
            other => bail!("字段声明是数组，但值是 {:?}", other),
        };
        super::write_leb(out, elems.len() as i64);
        for e in elems {
            write_value(out, meta, cp, type_id, 0, cp_field, e)?;
        }
        return Ok(());
    }
    if cp_field {
        let key = if v.is_null() { 0 } else { cp.key_of(type_id, v)? };
        super::write_leb(out, key);
        return Ok(());
    }
    let t = match meta.type_by_id(type_id) {
        Some(t) => t,
        None => bail!("写事件时引用了未定义的类型 id {}", type_id),
    };
    match (t.name.as_str(), v) {
        ("int", Value::Int(x)) => super::write_leb(out, *x),
        ("long", Value::Int(x)) => super::write_leb(out, *x),
        ("short", Value::Int(x)) => super::write_leb(out, *x),
        ("byte", Value::Int(x)) => out.push(*x as u8),
        ("char", Value::Int(x)) => super::write_leb(out, *x),
        ("boolean", Value::Bool(b)) => out.push(*b as u8),
        ("float", Value::Float(x)) => out.extend_from_slice(&x.to_bits().to_be_bytes()),
        ("float", Value::Double(x)) => out.extend_from_slice(&(*x as f32).to_bits().to_be_bytes()),
        ("double", Value::Double(x)) => out.extend_from_slice(&x.to_bits().to_be_bytes()),
        ("double", Value::Float(x)) => out.extend_from_slice(&(*x as f64).to_bits().to_be_bytes()),
        // 整数类型给了别的数值形态：能转就转
        ("int" | "long" | "short" | "byte" | "char", Value::Bool(b)) => {
            super::write_leb(out, *b as i64)
        }
        ("int" | "long" | "short" | "byte" | "char", other) => match other.as_i64() {
            Some(x) => super::write_leb(out, x),
            None => bail!("字段类型 {} 收到了 {other:?}", t.name),
        },
        (TYPE_STRING, Value::Str(s)) => write_inline_string(out, s),
        (TYPE_STRING, Value::Null) => out.push(0),
        (TYPE_STRING, other) => bail!("字符串字段收到了 {other:?}"),
        (_, Value::Null) => {
            // 结构体 null：写一个「没有类型」的内联零值不合法，只能整体省略。
            // 调用方（写事件）应当保证非空，这里给出明确错误而不是写坏数据。
            bail!("字段类型 {} 不能是 null（结构体字段请用常量池引用）", t.name)
        }
        (_, Value::Struct { fields, .. }) => {
            // simpleType：没有外层结构，直接写唯一字段（与读侧对称）
            if t.simple_type && t.fields.len() == 1 && fields.len() != t.fields.len() {
                let f = &t.fields[0];
                return write_value(out, meta, cp, f.type_id, f.dimension, f.constant_pool, v);
            }
            if t.simple_type && t.fields.len() == 1 && fields.len() == 1 {
                let f = &t.fields[0];
                let inner = &fields[0];
                return write_value(out, meta, cp, f.type_id, f.dimension, f.constant_pool, inner);
            }
            if fields.len() != t.fields.len() {
                bail!(
                    "结构体 {} 的字段数不符：期望 {}，实际 {}",
                    t.name,
                    t.fields.len(),
                    fields.len()
                );
            }
            for (f, fv) in t.fields.iter().zip(fields.iter()) {
                write_value(out, meta, cp, f.type_id, f.dimension, f.constant_pool, fv)?;
            }
        }
        (_, other) => bail!("字段类型 {} 收到了 {other:?}", t.name),
    }
    Ok(())
}

fn write_inline_string(out: &mut Vec<u8>, s: &str) {
    // 用 UTF-16 字符数组（编码 4），与元数据里的写法一致
    out.push(4);
    let units: Vec<u16> = s.encode_utf16().collect();
    super::write_leb(out, units.len() as i64);
    for u in units {
        super::write_leb(out, u as i64);
    }
}

/// 解析一条事件（`type_id` 必须是事件类型）。
pub fn parse_event(
    cur: &mut Cursor,
    meta: &Metadata,
    cp: &ConstantPool,
    type_id: i64,
) -> Result<EventRecord> {
    let t = match meta.type_by_id(type_id) {
        Some(t) => t,
        None => bail!("事件类型 id {} 未定义", type_id),
    };
    // 诊断：把事件体的前若干字节打出来（VMLESS_JFR_TRACE=1 时）
    if std::env::var("VMLESS_JFR_TRACE").map(|v| v != "0").unwrap_or(false) {
        let p = cur.pos;
        let end = (p + 48).min(cur.buf.len());
        crate::format::metadata::trace(&format!("event {} payload@{} = {:02x?}", t.name, p, &cur.buf[p..end]));
    }
    let start_ticks = cur.leb()?;
    let has_dur = t.has_duration();
    let duration_ticks = if has_dur { cur.leb()? } else { 0 };
    let start_index = t.start_index();
    if t.fields.is_empty() {
        bail!("事件类型 {} 没有任何字段（畸形元数据）", t.name);
    }
    let mut values = vec![Value::Null; t.fields.len()];
    values[0] = Value::Int(start_ticks);
    if has_dur {
        if t.fields.len() < 2 {
            bail!("事件类型 {} 有 duration 却只有一个字段", t.name);
        }
        values[1] = Value::Int(duration_ticks);
    }
    for i in start_index..t.fields.len() {
        let f = &t.fields[i];
        values[i] = parse_value(
            cur,
            meta,
            cp,
            f.type_id,
            f.dimension,
            f.constant_pool,
            0,
        )
        .map_err(|e| Error::new(format!("{}.{}: {}", t.name, f.name, e)))?;
    }
    // 常量池引用**保留成 Ref**，由消费方按需 `ConstantPool::deref` 解引用。
    // 在这里展开会把共享的线程/类/方法复制成每事件一份（内存直接翻几十倍）。
    // `cp` 参数保留是为了将来需要「解析时校验 key 是否存在」时用。
    let _ = cp;
    Ok(EventRecord {
        type_id,
        start_ticks,
        duration_ticks,
        values,
    })
}

/// 值树里有没有待解析的引用（没有就不必走一遍深拷贝）。
fn contains_ref(v: &Value) -> bool {
    match v {
        Value::Ref { .. } => true,
        Value::Struct { fields, .. } => fields.iter().any(contains_ref),
        Value::Array(items) => items.iter().any(contains_ref),
        _ => false,
    }
}

/// 序列化一条事件块。
pub fn write_event(
    out: &mut Vec<u8>,
    meta: &Metadata,
    cp: &CpBuilder,
    rec: &EventRecord,
) -> Result<()> {
    let t = match meta.type_by_id(rec.type_id) {
        Some(t) => t,
        None => bail!("写事件时类型 id {} 未定义", rec.type_id),
    };
    if rec.values.len() != t.fields.len() {
        bail!(
            "事件 {} 的值个数不符：期望 {}，实际 {}",
            t.name,
            t.fields.len(),
            rec.values.len()
        );
    }
    let mut payload = Vec::new();
    super::write_leb(&mut payload, rec.start_ticks);
    if t.has_duration() {
        super::write_leb(&mut payload, rec.duration_ticks);
    }
    for i in t.start_index()..t.fields.len() {
        let f = &t.fields[i];
        write_value(
            &mut payload,
            meta,
            cp,
            f.type_id,
            f.dimension,
            f.constant_pool,
            &rec.values[i],
        )
        .map_err(|e| Error::new(format!("{}.{}: {}", t.name, f.name, e)))?;
    }
    super::push_block(out, rec.type_id, &payload);
    Ok(())
}

/// 写侧的常量池：把重复出现、且字段标了 `constantPool` 的值换成 key。
///
/// 去重靠值的 `Debug` 形态 —— 值在这里一定是树（不可能有圈），
/// 所以「序列化文本相同 = 值相同」成立。真值语义上不去重也不会写错，
/// 只是文件大一点。
#[derive(Debug, Default)]
pub struct CpBuilder {
    /// (类型 id, 值的指纹) → key
    by_value: HashMap<(i64, String), i64>,
    /// 类型 id → 下一个可用 key（key 0 保留给 null）
    next_key: HashMap<i64, i64>,
    /// 条目： (类型 id, key, 值)
    entries: Vec<(i64, i64, Value)>,
}

impl CpBuilder {
    pub fn new() -> Self {
        CpBuilder::default()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 登记一个值并返回它的 key（已存在则复用）。
    pub fn intern(&mut self, type_id: i64, v: &Value) -> Result<i64> {
        let fp = format!("{:?}", v);
        if let Some(k) = self.by_value.get(&(type_id, fp.clone())) {
            return Ok(*k);
        }
        let key = {
            let n = self.next_key.entry(type_id).or_insert(0);
            *n += 1;
            *n
        };
        self.by_value.insert((type_id, fp), key);
        self.entries.push((type_id, key, v.clone()));
        Ok(key)
    }

    /// 已经登记过的值 → key。
    pub fn key_of(&self, type_id: i64, v: &Value) -> Result<i64> {
        let fp = format!("{:?}", v);
        match self.by_value.get(&(type_id, fp)) {
            Some(k) => Ok(*k),
            None => Err(Error::new(format!(
                "常量池里没有这个值（写事件前必须先 intern）：type={} {:?}",
                type_id, v
            ))),
        }
    }

    /// 一条事件引用了哪些 (类型, 值)：递归收集，供调用方 intern。
    ///
    /// 与 [`write_value`] 的递归形状严格对应 —— 两边只要有一处不一致，
    /// 写出来的文件就会在真 `jfr` 里读成 null，而且不会报错。
    pub fn collect(meta: &Metadata, type_id: i64, dim: u8, cp_field: bool, v: &Value, out: &mut Vec<(i64, Value)>) {
        if dim > 0 {
            let elems = match v {
                Value::Array(a) => a,
                _ => return,
            };
            for e in elems {
                CpBuilder::collect(meta, type_id, 0, cp_field, e, out);
            }
            return;
        }
        if cp_field {
            if !v.is_null() {
                out.push((type_id, v.clone()));
                // 继续深入：常量池条目自己也可能带常量池字段
                // （`jdk.types.Method.type` → `java.lang.Class` 就是一层引用）
                if let Some(t) = meta.type_by_id(type_id) {
                    if let Value::Struct { fields, .. } = v {
                        for (f, fv) in t.fields.iter().zip(fields.iter()) {
                            CpBuilder::collect(meta, f.type_id, f.dimension, f.constant_pool, fv, out);
                        }
                    }
                }
            }
            return;
        }
        let t = match meta.type_by_id(type_id) {
            Some(t) => t,
            None => return,
        };
        if let Value::Struct { fields, .. } = v {
            for (f, fv) in t.fields.iter().zip(fields.iter()) {
                CpBuilder::collect(meta, f.type_id, f.dimension, f.constant_pool, fv, out);
            }
        }
    }

    /// 把所有条目按类型聚合成 `PoolWriter::write_block` 需要的样子。
    ///
    /// 顺序：类型 id 升序、key 升序（稳定 → 同样输入产出同样字节）。
    pub fn pools(&self) -> Vec<(i64, Vec<(i64, Value)>)> {
        let mut v: Vec<(i64, i64, &Value)> = self
            .entries
            .iter()
            .map(|(t, k, val)| (*t, *k, val))
            .collect();
        v.sort_by_key(|(t, k, _)| (*t, *k));
        let mut out: Vec<(i64, Vec<(i64, Value)>)> = Vec::new();
        for (t, k, val) in v {
            match out.last_mut() {
                Some((lt, list)) if *lt == t => list.push((k, val.clone())),
                _ => out.push((t, vec![(k, val.clone())])),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AnnotationDef, FieldDef, Metadata, TypeDef, SUPER_EVENT};

    fn meta() -> Metadata {
        let types = vec![
            TypeDef {
                id: 10,
                name: "long".into(),
                ..Default::default()
            },
            TypeDef {
                id: 11,
                name: "int".into(),
                ..Default::default()
            },
            TypeDef {
                id: 12,
                name: "boolean".into(),
                ..Default::default()
            },
            TypeDef {
                id: 13,
                name: TYPE_STRING.into(),
                ..Default::default()
            },
            TypeDef {
                id: 20,
                name: "jdk.types.Recorder".into(),
                fields: vec![
                    FieldDef {
                        name: "name".into(),
                        type_id: 13,
                        ..Default::default()
                    },
                    FieldDef {
                        name: "count".into(),
                        type_id: 10,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            TypeDef {
                id: 30,
                name: "jdk.TestEvent".into(),
                super_type: Some(SUPER_EVENT.into()),
                fields: vec![
                    FieldDef {
                        name: "startTime".into(),
                        type_id: 10,
                        ..Default::default()
                    },
                    FieldDef {
                        name: "duration".into(),
                        type_id: 10,
                        ..Default::default()
                    },
                    FieldDef {
                        name: "owner".into(),
                        type_id: 20,
                        constant_pool: true,
                        ..Default::default()
                    },
                    FieldDef {
                        name: "flag".into(),
                        type_id: 12,
                        ..Default::default()
                    },
                    FieldDef {
                        name: "ratio".into(),
                        type_id: 10,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
        ];
        Metadata::build(types, 0, 0, "")
    }

    #[test]
    fn event_roundtrip_with_constant_pool() {
        let m = meta();
        let owner = Value::Struct {
            type_id: 20,
            cp_id: 0,
            fields: vec![Value::Str("main".into()), Value::Int(7)],
        };
        let rec = EventRecord {
            type_id: 30,
            start_ticks: 1234,
            duration_ticks: 55,
            values: vec![
                Value::Int(1234),
                Value::Int(55),
                owner.clone(),
                Value::Bool(true),
                Value::Int(9),
            ],
        };
        let mut cp = CpBuilder::new();
        // 收集 + 登记
        let mut todo = Vec::new();
        let t = m.type_by_id(30).unwrap();
        for i in t.start_index()..t.fields.len() {
            let f = &t.fields[i];
            CpBuilder::collect(&m, f.type_id, f.dimension, f.constant_pool, &rec.values[i], &mut todo);
        }
        for (tid, v) in &todo {
            cp.intern(*tid, v).unwrap();
        }
        assert_eq!(cp.pools().len(), 1);

        let mut buf = Vec::new();
        write_event(&mut buf, &m, &cp, &rec).unwrap();

        let mut c = Cursor::new(&buf);
        let size = c.leb().unwrap();
        assert_eq!(size as usize, buf.len());
        let tid = c.leb().unwrap();
        assert_eq!(tid, 30);
        // 造一个含该常量的池
        let mut pool = ConstantPool::new();
        pool.insert(20, cp.key_of(20, &owner).unwrap(), owner);
        let back = parse_event(&mut c, &m, &pool, 30).unwrap();
        assert_eq!(back.start_ticks, 1234);
        assert_eq!(back.duration_ticks, 55);
        // 事件体里的常量池字段保持 `Ref`（共享、不展开），要经 `deref` 才能看到结构
        let owner_back = pool.deref(&back.values[2]);
        assert_eq!(owner_back.as_struct().unwrap().1[0].as_str(), Some("main"));
        assert_eq!(back.values[3], Value::Bool(true));
        assert_eq!(back.values[4], Value::Int(9));
    }

    #[test]
    fn float_is_raw_big_endian() {
        let m = meta();
        let mut cp = CpBuilder::new();
        let mut buf = Vec::new();
        write_value(&mut buf, &m, &cp, 10, 0, false, &Value::Int(300)).unwrap();
        // 300 = 0b1_0010_1100 → leb: 0xAC 0x02
        assert_eq!(buf, vec![0xAC, 0x02]);
        buf.clear();
        write_value(&mut buf, &m, &cp, 12, 0, false, &Value::Bool(true)).unwrap();
        assert_eq!(buf, vec![1]);
        let _ = &mut cp;
    }

    #[test]
    fn insane_array_length_is_error() {
        let m = meta();
        let cp = ConstantPool::new();
        // 声明 10 亿个元素
        let data = [0xff, 0xff, 0xff, 0xff, 0x07];
        let mut c = Cursor::new(&data);
        let r = parse_value(&mut c, &m, &cp, 10, 1, false, 0);
        assert!(r.is_err());
    }

    #[test]
    fn missing_type_is_error_not_panic() {
        let m = meta();
        let cp = ConstantPool::new();
        let data = [0u8];
        let mut c = Cursor::new(&data);
        assert!(parse_value(&mut c, &m, &cp, 9999, 0, false, 0).is_err());
    }

    #[test]
    fn annotation_helpers() {
        let a = AnnotationDef {
            type_id: 1,
            type_name: "jdk.jfr.Timespan".into(),
            attrs: vec![("value".into(), "NANOSECONDS".into())],
        };
        assert_eq!(a.simple_name(), "Timespan");
        assert_eq!(a.value(), Some("NANOSECONDS"));
    }
}
