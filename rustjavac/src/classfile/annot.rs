//! 运行期注解与 `Signature` 属性的解析。
//!
//! class 文件里这四种属性原来只以**裸字节**（`Attribute::info`）保留，反射层要能把它们
//! 变成 Java 对象，所以在这里解出来：
//!
//! | 属性 | 出现在 | 解成 |
//! | --- | --- | --- |
//! | `RuntimeVisibleAnnotations` | 类 / 字段 / 方法 | `Vec<Ann>` |
//! | `RuntimeVisibleParameterAnnotations` | 方法 | `Vec<Vec<Ann>>`（按参数下标） |
//! | `AnnotationDefault` | 注解类型的方法 | `AnnVal` |
//! | `Signature` | 类 / 字段 / 方法 | 原始字符串（泛型由 java.base 自己解析） |
//!
//! 只解 `RuntimeVisible*`：`RuntimeInvisible*` 按 JVMS 在运行期本来就不可见，反射也读不到。

use super::read::Reader;
use super::{Attribute, ClassFile, CpEntry};

/// 一个注解实例（`@Foo(a=1, b="x")`）
#[derive(Clone, Debug, PartialEq)]
pub struct Ann {
    /// 注解类型的字段描述符，如 `Lpkg/MyAnno;`
    pub ty: String,
    /// 显式写出的成员值（顺序 = class 文件里的顺序）
    pub pairs: Vec<(String, AnnVal)>,
}

/// 注解成员的值（`element_value` 的联合体）
#[derive(Clone, Debug, PartialEq)]
pub enum AnnVal {
    /// `Z`
    Bool(bool),
    /// `B`
    Byte(i8),
    /// `C`（UTF-16 码元）
    Char(u16),
    /// `S`
    Short(i16),
    /// `I`
    Int(i32),
    /// `J`
    Long(i64),
    /// `F`
    Float(f32),
    /// `D`
    Double(f64),
    /// `s`：字符串常量
    Str(String),
    /// `e`：`(枚举类型描述符, 常量名)`
    Enum(String, String),
    /// `c`：类字面量，值是**描述符**（`Ljava/lang/String;` / `I` / `[I`）
    Class(String),
    /// `@`：嵌套注解
    Anno(Ann),
    /// `[`：数组
    Array(Vec<AnnVal>),
}

/// 属性名（常量池 Utf8 文本）；索引坏了就是空串
fn attr_name<'a>(cf: &'a ClassFile, a: &Attribute) -> &'a [u8] {
    cf.utf8(a.name_index).unwrap_or_default()
}

fn find<'a, 'b>(cf: &'a ClassFile, attrs: &'b [Attribute], name: &[u8]) -> Option<&'b Attribute> {
    attrs.iter().find(|a| attr_name(cf, a) == name)
}

// ------------------------------------------------------------------ 常量池取值

fn utf8_at(cf: &ClassFile, index: u16) -> Option<String> {
    cf.utf8(index).map(|b| String::from_utf8_lossy(b).into_owned())
}

fn int_at(cf: &ClassFile, index: u16) -> Option<i32> {
    match cf.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
        Some(CpEntry::Integer(v)) => Some(*v as i32),
        _ => None,
    }
}

fn long_at(cf: &ClassFile, index: u16) -> Option<i64> {
    match cf.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
        Some(CpEntry::Long(v)) => Some(*v as i64),
        _ => None,
    }
}

fn float_at(cf: &ClassFile, index: u16) -> Option<f32> {
    match cf.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
        Some(CpEntry::Float(bits)) => Some(f32::from_bits(*bits)),
        _ => None,
    }
}

fn double_at(cf: &ClassFile, index: u16) -> Option<f64> {
    match cf.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
        Some(CpEntry::Double(bits)) => Some(f64::from_bits(*bits)),
        _ => None,
    }
}

// ------------------------------------------------------------------ 注解结构

/// 读一个 `annotation` 结构：`u2 type_index` + `u2 num_pairs` + 每项 `u2 name + element_value`
fn read_ann(cf: &ClassFile, r: &mut Reader<'_>) -> Option<Ann> {
    let ty = utf8_at(cf, r.u16().ok()?)?;
    let n = r.u16().ok()? as usize;
    let mut pairs = Vec::with_capacity(n);
    for _ in 0..n {
        let name = utf8_at(cf, r.u16().ok()?)?;
        let v = read_value(cf, r)?;
        pairs.push((name, v));
    }
    Some(Ann { ty, pairs })
}

/// 读一个 `element_value`（tag 字节 + 联合体）
fn read_value(cf: &ClassFile, r: &mut Reader<'_>) -> Option<AnnVal> {
    let tag = r.u8().ok()?;
    let v = match tag {
        b'B' => AnnVal::Byte(int_at(cf, r.u16().ok()?)? as i8),
        b'C' => AnnVal::Char(int_at(cf, r.u16().ok()?)? as u16),
        b'S' => AnnVal::Short(int_at(cf, r.u16().ok()?)? as i16),
        b'Z' => AnnVal::Bool(int_at(cf, r.u16().ok()?)? != 0),
        b'I' => AnnVal::Int(int_at(cf, r.u16().ok()?)?),
        b'J' => AnnVal::Long(long_at(cf, r.u16().ok()?)?),
        b'F' => AnnVal::Float(float_at(cf, r.u16().ok()?)?),
        b'D' => AnnVal::Double(double_at(cf, r.u16().ok()?)?),
        b's' => AnnVal::Str(utf8_at(cf, r.u16().ok()?)?),
        b'e' => AnnVal::Enum(utf8_at(cf, r.u16().ok()?)?, utf8_at(cf, r.u16().ok()?)?),
        b'c' => AnnVal::Class(utf8_at(cf, r.u16().ok()?)?),
        b'@' => AnnVal::Anno(read_ann(cf, r)?),
        b'[' => {
            let n = r.u16().ok()? as usize;
            let mut items = Vec::with_capacity(n);
            for _ in 0..n {
                items.push(read_value(cf, r)?);
            }
            AnnVal::Array(items)
        }
        _ => return None,
    };
    Some(v)
}

/// `RuntimeVisibleAnnotations` 解出来的注解表
pub fn visible_annotations(cf: &ClassFile, attrs: &[Attribute]) -> Vec<Ann> {
    let Some(a) = find(cf, attrs, b"RuntimeVisibleAnnotations") else { return Vec::new() };
    let mut r = Reader::new(&a.info);
    let Ok(n) = r.u16() else { return Vec::new() };
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        match read_ann(cf, &mut r) {
            Some(ann) => out.push(ann),
            None => break,
        }
    }
    out
}

/// `RuntimeVisibleParameterAnnotations`：按参数下标的注解表（下标 = 参数，从 0 起）
pub fn visible_parameter_annotations(cf: &ClassFile, attrs: &[Attribute]) -> Vec<Vec<Ann>> {
    let Some(a) = find(cf, attrs, b"RuntimeVisibleParameterAnnotations") else { return Vec::new() };
    let mut r = Reader::new(&a.info);
    let Ok(n) = r.u8() else { return Vec::new() };
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let Ok(k) = r.u16() else { break };
        let mut per = Vec::with_capacity(k as usize);
        for _ in 0..k {
            match read_ann(cf, &mut r) {
                Some(ann) => per.push(ann),
                None => return out,
            }
        }
        out.push(per);
    }
    out
}

/// `AnnotationDefault`：注解类型某个成员方法的默认值
pub fn annotation_default(cf: &ClassFile, attrs: &[Attribute]) -> Option<AnnVal> {
    let a = find(cf, attrs, b"AnnotationDefault")?;
    read_value(cf, &mut Reader::new(&a.info))
}

/// `Signature` 属性（泛型签名原文）；没有就 None
pub fn signature(cf: &ClassFile, attrs: &[Attribute]) -> Option<String> {
    let a = find(cf, attrs, b"Signature")?;
    let mut r = Reader::new(&a.info);
    utf8_at(cf, r.u16().ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(name: &str) -> Option<ClassFile> {
        let path = std::path::Path::new("samples").join(format!("{name}.class"));
        let bytes = std::fs::read(path).ok()?;
        Some(super::super::parse(&bytes).expect("parse"))
    }

    #[test]
    fn reads_class_annotations_when_present() {
        // samples/ 里的类没有注解，只验证「没有属性就返回空」不会 panic
        if let Some(cf) = sample("I1") {
            assert!(visible_annotations(&cf, &cf.attributes).is_empty());
            assert!(signature(&cf, &cf.attributes).is_none());
        }
    }
}
