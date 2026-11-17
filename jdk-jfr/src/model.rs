//! 事件模型：类型定义（`TypeDef`）、注解、字段，以及事件里的**值**（[`Value`]）。
//!
//! 这一层是从「元数据元素树」翻译过来的**类型化视图**：读一次、校验一次，
//! 之后解析事件只查表，不再碰字符串。
//!
//! # 常量池里被特殊对待的四个名字
//!
//! `jdk.jfr.consumer` 的 `ObjectFactory` 按**类型名**决定把常量包成哪个类：
//!
//! | 类型名 | 包装成 |
//! | --- | --- |
//! | `java.lang.Thread` | `RecordedThread` |
//! | `java.lang.Class` | `RecordedClass` |
//! | `jdk.types.ClassLoader` | `RecordedClassLoader` |
//! | `jdk.types.Method` | `RecordedMethod` |
//! | `jdk.types.StackFrame` | `RecordedFrame` |
//! | `jdk.types.StackTrace` | `RecordedStackTrace` |
//!
//! 所以自己写元数据时这些名字**一个字都不能改** —— 改成 `jdk.types.Thread`
//! 之后真 `jfr print` 不会报错，只是把线程打印成一坨 `{...}` 结构体。

use std::collections::HashMap;

use crate::format::metadata::Element;
use crate::format::Result;

/// 事件类型（`superType` 正好是它的才是事件）。
pub const SUPER_EVENT: &str = "jdk.jfr.Event";
/// 注解类型。
pub const SUPER_ANNOTATION: &str = "java.lang.annotation.Annotation";
/// 设置类型。
pub const SUPER_SETTING: &str = "jdk.jfr.SettingControl";

/// 字符串常量的类型名。
pub const TYPE_STRING: &str = "java.lang.String";
/// 线程类型名（**必须是这个**，不能是 `jdk.types.Thread`）。
pub const TYPE_THREAD: &str = "java.lang.Thread";
/// 类类型名。
pub const TYPE_CLASS: &str = "java.lang.Class";
/// 类加载器。
pub const TYPE_CLASS_LOADER: &str = "jdk.types.ClassLoader";
/// 方法。
pub const TYPE_METHOD: &str = "jdk.types.Method";
/// 栈帧。
pub const TYPE_STACK_FRAME: &str = "jdk.types.StackFrame";
/// 栈。
pub const TYPE_STACK_TRACE: &str = "jdk.types.StackTrace";

/// 一个注解（`@Label("Start Time")` 这种）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AnnotationDef {
    pub type_id: i64,
    /// 解析出来的注解类型全名；解析不到时为空串。
    pub type_name: String,
    /// 注解的属性（名 → 值），**顺序就是写进文件里的顺序**，打印时要照抄。
    pub attrs: Vec<(String, String)>,
}

impl AnnotationDef {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// `@Label("x")` 里的 `value`。
    pub fn value(&self) -> Option<&str> {
        self.get("value")
    }

    pub fn simple_name(&self) -> &str {
        match self.type_name.rsplit_once('.') {
            Some((_, s)) => s,
            None => &self.type_name,
        }
    }
}

/// 一个字段（或一个 setting）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FieldDef {
    pub name: String,
    pub type_id: i64,
    /// 1 = 数组（格式里只有 0/1 两态，`dimension` 属性存在即为 1）。
    pub dimension: u8,
    /// 该字段是不是常量池引用（事件里只存一个 key）。
    pub constant_pool: bool,
    pub annotations: Vec<AnnotationDef>,
    /// 元数据里 `<setting>` 而不是 `<field>`。
    pub is_setting: bool,
}

impl FieldDef {
    pub fn is_array(&self) -> bool {
        self.dimension > 0
    }

    /// 按**简单名**找注解（`Label` / `Timestamp` / `Timespan` …）。
    pub fn annotation(&self, simple: &str) -> Option<&AnnotationDef> {
        self.annotations
            .iter()
            .find(|a| a.simple_name() == simple || a.type_name == simple)
    }

    pub fn has_annotation(&self, simple: &str) -> bool {
        self.annotation(simple).is_some()
    }
}

/// 一个类型定义。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TypeDef {
    pub id: i64,
    pub name: String,
    pub super_type: Option<String>,
    /// `simpleType="true"`：值直接内联，不再包一层结构体。
    pub simple_type: bool,
    pub fields: Vec<FieldDef>,
    pub settings: Vec<FieldDef>,
    pub annotations: Vec<AnnotationDef>,
}

impl TypeDef {
    pub fn simple_name(&self) -> &str {
        match self.name.rsplit_once('.') {
            Some((_, s)) => s,
            None => &self.name,
        }
    }

    pub fn is_event(&self) -> bool {
        self.super_type.as_deref() == Some(SUPER_EVENT)
    }

    pub fn is_annotation(&self) -> bool {
        self.super_type.as_deref() == Some(SUPER_ANNOTATION)
    }

    pub fn field(&self, name: &str) -> Option<&FieldDef> {
        self.fields.iter().find(|f| f.name == name)
    }

    pub fn field_index(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name == name)
    }

    pub fn annotation(&self, simple: &str) -> Option<&AnnotationDef> {
        self.annotations
            .iter()
            .find(|a| a.simple_name() == simple || a.type_name == simple)
    }

    /// 事件类型里有没有 `duration` 字段。
    ///
    /// 有的话事件体多一个 ticks（`EventParser.startIndex = 2`），没有就只有 startTime。
    pub fn has_duration(&self) -> bool {
        self.field_index("duration").is_some()
    }

    /// 事件体里「元数据自己声明」的第一个字段下标。
    pub fn start_index(&self) -> usize {
        if self.has_duration() {
            2
        } else {
            1
        }
    }
}

/// 一次录制里出现的全部类型。
#[derive(Debug, Clone, Default)]
pub struct Metadata {
    pub types: Vec<TypeDef>,
    by_id: HashMap<i64, usize>,
    by_name: HashMap<String, i64>,
    /// 事件类型 id，**保持元数据里的顺序**（打印/汇总要用）。
    pub event_type_ids: Vec<i64>,
    /// 记录时区偏移（毫秒）。
    pub gmt_offset_ms: i64,
    pub dst_ms: i64,
    pub locale: String,
}

impl Metadata {
    /// 从元数据元素树（`root` 元素）构建类型视图。
    pub fn from_root(root: &Element) -> Result<Metadata> {
        let md = match root.child("metadata") {
            Some(m) => m,
            None => crate::bail!("元数据里没有 <metadata> 元素"),
        };
        let mut types = Vec::new();
        for ce in md.children_named("class") {
            let name = ce.attr("name").unwrap_or("").to_string();
            // 类型元素上的 id 属性名是 "id"；字段上的类型引用才是 "class"
            // （MetadataDescriptor: ATTRIBUTE_ID="id" / ATTRIBUTE_TYPE_ID="class"）
            let id = match ce.attr("id").and_then(|s| s.parse::<i64>().ok()) {
                Some(v) => v,
                None => crate::bail!("类型 {} 的 id 不是整数", name),
            };
            let mut t = TypeDef {
                id,
                name,
                super_type: ce.attr("superType").map(|s| s.to_string()),
                simple_type: ce.attr("simpleType").is_some(),
                fields: Vec::new(),
                settings: Vec::new(),
                annotations: Vec::new(),
            };
            for (child_name, is_setting) in [("setting", true), ("field", false)] {
                for fe in ce.children_named(child_name) {
                    let ty = match fe.attr("class").and_then(|s| s.parse::<i64>().ok()) {
                        Some(v) => v,
                        None => {
                            crate::bail!("{}.{} 的 class 属性不是整数", t.name, fe.attr("name").unwrap_or("?"))
                        }
                    };
                    let f = FieldDef {
                        name: fe.attr("name").unwrap_or("").to_string(),
                        type_id: ty,
                        dimension: if fe.attr("dimension").is_some() { 1 } else { 0 },
                        constant_pool: fe.attr("constantPool").is_some(),
                        annotations: read_annotations(fe),
                        is_setting,
                    };
                    if is_setting {
                        t.settings.push(f);
                    } else {
                        t.fields.push(f);
                    }
                }
            }
            t.annotations = read_annotations(ce);
            types.push(t);
        }

        let mut by_id = HashMap::with_capacity(types.len() * 2);
        let mut by_name = HashMap::with_capacity(types.len() * 2);
        let mut names: HashMap<i64, String> = HashMap::with_capacity(types.len() * 2);
        for (i, t) in types.iter().enumerate() {
            by_id.insert(t.id, i);
            by_name.insert(t.name.clone(), t.id);
            names.insert(t.id, t.name.clone());
        }
        // 注解里的 type_id → 类型名（打印要用）
        let name_of = |id: i64| -> String { names.get(&id).cloned().unwrap_or_default() };
        for t in types.iter_mut() {
            for a in t.annotations.iter_mut() {
                a.type_name = name_of(a.type_id);
            }
            for f in t.fields.iter_mut().chain(t.settings.iter_mut()) {
                for a in f.annotations.iter_mut() {
                    a.type_name = name_of(a.type_id);
                }
            }
        }
        let event_type_ids = types.iter().filter(|t| t.is_event()).map(|t| t.id).collect();

        let region = root.child("region");
        let gmt = region
            .and_then(|r| r.attr("gmtOffset"))
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        let dst = region
            .and_then(|r| r.attr("dst"))
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        let locale = region
            .and_then(|r| r.attr("locale"))
            .map(|s| s.to_string())
            .unwrap_or_default();

        Ok(Metadata {
            types,
            by_id,
            by_name,
            event_type_ids,
            gmt_offset_ms: gmt,
            dst_ms: dst,
            locale,
        })
    }

    /// 直接从类型表构建（写侧用），同时补齐下标。
    pub fn build(
        mut types: Vec<TypeDef>,
        gmt_offset_ms: i64,
        dst_ms: i64,
        locale: &str,
    ) -> Metadata {
        let mut by_id = HashMap::with_capacity(types.len() * 2);
        let mut by_name = HashMap::with_capacity(types.len() * 2);
        let mut names: HashMap<i64, String> = HashMap::with_capacity(types.len() * 2);
        for (i, t) in types.iter().enumerate() {
            by_id.insert(t.id, i);
            by_name.insert(t.name.clone(), t.id);
            names.insert(t.id, t.name.clone());
        }
        let name_of = |id: i64| -> String { names.get(&id).cloned().unwrap_or_default() };
        for t in types.iter_mut() {
            for a in t.annotations.iter_mut() {
                a.type_name = name_of(a.type_id);
            }
            for f in t.fields.iter_mut().chain(t.settings.iter_mut()) {
                for a in f.annotations.iter_mut() {
                    a.type_name = name_of(a.type_id);
                }
            }
        }
        let event_type_ids = types.iter().filter(|t| t.is_event()).map(|t| t.id).collect();
        Metadata {
            types,
            by_id,
            by_name,
            event_type_ids,
            gmt_offset_ms,
            dst_ms,
            locale: locale.to_string(),
        }
    }

    /// 反向：类型表 → 元数据元素树。
    ///
    /// 顺序 = `types` 的顺序；写出去的顺序不影响读，但固定下来才能做逐字节复现测试。
    pub fn to_root(&self) -> Element {
        let mut root = Element::new("root");
        let mut md = Element::new("metadata");
        for t in &self.types {
            let mut ce = Element::new("class");
            ce.add_attr("name", t.name.clone());
            if let Some(st) = &t.super_type {
                ce.add_attr("superType", st.clone());
            }
            if t.simple_type {
                ce.add_attr("simpleType", "true");
            }
            ce.add_attr("id", t.id.to_string());
            for s in &t.settings {
                write_field_element(&mut ce, s, "setting");
            }
            for f in &t.fields {
                write_field_element(&mut ce, f, "field");
            }
            write_annotations(&mut ce, &t.annotations);
            md.children.push(ce);
        }
        root.children.push(md);
        let mut region = Element::new("region");
        region.add_attr("locale", self.locale.clone());
        region.add_attr("gmtOffset", self.gmt_offset_ms.to_string());
        region.add_attr("dst", self.dst_ms.to_string());
        root.children.push(region);
        root
    }

    pub fn type_by_id(&self, id: i64) -> Option<&TypeDef> {
        self.by_id.get(&id).and_then(|i| self.types.get(*i))
    }

    pub fn type_id_by_name(&self, name: &str) -> Option<i64> {
        self.by_name.get(name).copied()
    }

    pub fn type_by_name(&self, name: &str) -> Option<&TypeDef> {
        self.type_id_by_name(name).and_then(|id| self.type_by_id(id))
    }

    /// 类型名；查不到时返回 `unknown(<id>)`（与真 jfr 的报错口径一致）。
    pub fn type_name(&self, id: i64) -> String {
        match self.type_by_id(id) {
            Some(t) => t.name.clone(),
            None => format!("unknown({})", id),
        }
    }

    /// 字段的类型名（打印 `metadata` 要用简单名）。
    pub fn field_type_name(&self, f: &FieldDef) -> String {
        self.type_name(f.type_id)
    }

    pub fn is_event(&self, id: i64) -> bool {
        self.type_by_id(id).map(|t| t.is_event()).unwrap_or(false)
    }

    /// 事件里 startTime 的下标（恒为 0；不存在的畸形数据返回 None）。
    pub fn start_field_index(&self, type_id: i64) -> Option<usize> {
        self.type_by_id(type_id)
            .and_then(|t| t.field_index("startTime"))
    }
}

fn read_annotations(e: &Element) -> Vec<AnnotationDef> {
    e.children_named("annotation")
        .filter_map(|a| {
            let id = a.attr("class").and_then(|s| s.parse::<i64>().ok())?;
            Some(AnnotationDef {
                type_id: id,
                type_name: String::new(),
                attrs: a
                    .attrs
                    .iter()
                    .filter(|(k, _)| k != "class")
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            })
        })
        .collect()
}

fn write_annotations(e: &mut Element, anns: &[AnnotationDef]) {
    for a in anns {
        let mut ae = Element::new("annotation");
        ae.add_attr("class", a.type_id.to_string());
        for (k, v) in &a.attrs {
            ae.add_attr(k, v.clone());
        }
        e.children.push(ae);
    }
}

fn write_field_element(parent: &mut Element, f: &FieldDef, kind: &str) {
    let mut fe = Element::new(kind);
    fe.add_attr("name", f.name.clone());
    fe.add_attr("class", f.type_id.to_string());
    if f.dimension > 0 {
        fe.add_attr("dimension", "1");
    }
    if f.constant_pool {
        fe.add_attr("constantPool", "true");
    }
    write_annotations(&mut fe, &f.annotations);
    parent.children.push(fe);
}

/// 事件里的一个值。
///
/// 与 Java 侧 `ParserFactory` 的解析产物一一对应：
/// `Long/Integer/Short/Byte/Char` → [`Value::Int`]（区别记在字段的声明类型里），
/// `Boolean` → [`Value::Bool`]，`String` → [`Value::Str`]，
/// `Float/Double` → [`Value::Float`]/[`Value::Double`]，结构体 → [`Value::Struct`]，
/// 数组 → [`Value::Array`]，`null` → [`Value::Null`]。
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Float(f32),
    Double(f64),
    Bool(bool),
    Str(String),
    Struct {
        type_id: i64,
        fields: Vec<Value>,
        /// 这个结构体在常量池里的 key（内联结构体为 0）。
        /// 为什么要留着：`RecordedClassLoader.getId()` 返回的就是它 ——
        /// 真文件里 `jdk.types.ClassLoader` 根本没有 `id` 字段，打印的是池键。
        cp_id: i64,
    },
    Array(Vec<Value>),
    /// 常量池引用（尚未解析）。
    ///
    /// 为什么要有这个中间态：常量池的块是**倒着**链的（新的在前），而块里的条目
    /// 会互相引用（`StackFrame.method` → `Method.type` → `Class`），被引用的条目
    /// 往往在**更旧**的块里 —— 那时还没读到。Java 侧同样先存 `Reference`，
    /// 全部读完再 `resolve()`。不做这一步的结果是「帧的方法名全是 null」，
    /// 而且不报错。
    Ref { type_id: i64, key: i64 },
}

impl Value {
    pub fn type_id(&self) -> Option<i64> {
        match self {
            Value::Struct { type_id, .. } => Some(*type_id),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(v) => Some(*v),
            Value::Float(v) => Some(*v as i64),
            Value::Double(v) => Some(*v as i64),
            Value::Bool(b) => Some(*b as i64),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(v) => Some(*v as f64),
            Value::Float(v) => Some(*v as f64),
            Value::Double(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_struct(&self) -> Option<(i64, &[Value])> {
        match self {
            Value::Struct { type_id, fields, .. } => Some((*type_id, fields)),
            _ => None,
        }
    }

    /// 常量池键（内联结构体为 0）。
    pub fn cp_id(&self) -> i64 {
        match self {
            Value::Struct { cp_id, .. } => *cp_id,
            _ => 0,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// 取结构体里名为 `name` 的字段值。
    pub fn field(&self, meta: &Metadata, name: &str) -> Option<&Value> {
        let (type_id, fields) = self.as_struct()?;
        let idx = meta.type_by_id(type_id)?.field_index(name)?;
        fields.get(idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_meta() -> Metadata {
        let types = vec![
            TypeDef {
                id: 10,
                name: "long".into(),
                ..Default::default()
            },
            TypeDef {
                id: 99,
                name: "jdk.jfr.Timestamp".into(),
                super_type: Some(SUPER_ANNOTATION.into()),
                ..Default::default()
            },
            TypeDef {
                id: 20,
                name: "jdk.TestEvent".into(),
                super_type: Some(SUPER_EVENT.into()),
                fields: vec![
                    FieldDef {
                        name: "startTime".into(),
                        type_id: 10,
                        annotations: vec![AnnotationDef {
                            type_id: 99,
                            type_name: "jdk.jfr.Timestamp".into(),
                            attrs: vec![("value".into(), "TICKS".into())],
                        }],
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
        ];
        Metadata::build(types, 28800000, 0, "zh_CN")
    }

    #[test]
    fn build_and_lookup() {
        let m = simple_meta();
        assert_eq!(m.type_id_by_name("jdk.TestEvent"), Some(20));
        assert!(m.is_event(20));
        assert!(!m.is_event(10));
        assert_eq!(m.event_type_ids, vec![20]);
        assert_eq!(m.type_name(12345), "unknown(12345)");
        let t = m.type_by_id(20).unwrap();
        assert!(!t.has_duration());
        assert_eq!(t.start_index(), 1);
        let f = t.field("startTime").unwrap();
        assert_eq!(
            f.annotation("Timestamp").and_then(|a| a.value()),
            Some("TICKS")
        );
    }

    #[test]
    fn event_annotation_roundtrip_through_element() {
        let m = simple_meta();
        let root = m.to_root();
        let g = Metadata::from_root(&root).unwrap();
        assert_eq!(g.types.len(), m.types.len());
        assert_eq!(g.gmt_offset_ms, 28800000);
        let t = g.type_by_id(20).unwrap();
        assert_eq!(t.name, "jdk.TestEvent");
        let f = t.field("startTime").unwrap();
        assert_eq!(f.annotation("Timestamp").unwrap().type_name, "jdk.jfr.Timestamp");
        assert_eq!(g.event_type_ids, vec![20]);
    }
}
