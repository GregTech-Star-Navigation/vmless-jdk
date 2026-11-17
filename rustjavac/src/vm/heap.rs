//! 对象 / 数组 / 字符串 / 类对象 四套堆与引用句柄编码。

use crate::CpEntry;

use super::slot::Slot;
use super::Vm;

/// 字符串句柄的基址：`handle >= STR_BASE` 就是字符串堆里的下标。
/// 对象句柄是正数（1 起）、数组句柄是负数、0 是 null，四者不重叠。
pub(super) const STR_BASE: i32 = 1 << 24;

/// 一个引用句柄的运行时类型
pub(super) enum RefKind {
    Null,
    Str,
    Obj(String),
    Arr(String),
}

/// 类描述符 → `Class.getName()` 的返回值（二进制名）。
///
/// 基本类型用 Java 的写法（`I` → `int`）；数组是 `[` + 元素名
/// （`[I` → `[I`，`[Ljava/lang/String;` → `[Ljava.lang.String;`）。
fn java_class_name(desc: &str) -> String {
    match desc {
        "I" => "int".into(),
        "J" => "long".into(),
        "D" => "double".into(),
        "F" => "float".into(),
        "Z" => "boolean".into(),
        "C" => "char".into(),
        "B" => "byte".into(),
        "S" => "short".into(),
        "V" => "void".into(),
        _ => desc.replace('/', "."),
    }
}

impl Vm {
    /// 字符串常量 → 句柄（同一常量池项复用同一句柄；不同类要分开记，索引会撞）
    pub(super) fn string_handle(&mut self, class: usize, cp_index: u16) -> i32 {
        if let Some(h) = self.string_handles.get(&(class, cp_index)) {
            return *h;
        }
        let text = self.string_at(class, cp_index).unwrap_or_default();
        self.strings.push(text);
        let h = STR_BASE + self.strings.len() as i32 - 1;
        self.string_handles.insert((class, cp_index), h);
        h
    }

    pub(super) fn string_text(&self, handle: i32) -> String {
        self.strings.get((handle - STR_BASE) as usize).cloned().unwrap_or_default()
    }

    pub(super) fn ref_kind(&self, handle: i32) -> RefKind {
        if handle == 0 {
            RefKind::Null
        } else if handle >= STR_BASE {
            RefKind::Str
        } else if handle > 0 {
            RefKind::Obj(self.object_class.get(handle as usize - 1).cloned().unwrap_or_default())
        } else {
            RefKind::Arr(self.array_class.get((-handle - 1) as usize).cloned().unwrap_or_default())
        }
    }

    pub(super) fn string_at(&self, class: usize, index: u16) -> Option<String> {
        let cf = self.store.classes.get(class)?;
        match cf.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
            Some(CpEntry::String(u)) => cf.utf8(*u).map(|b| String::from_utf8_lossy(b).into_owned()),
            _ => None,
        }
    }

    // ---------- 类对象 ----------

    /// 类描述符 → 类对象句柄（同一个类永远是**同一个**句柄：`X.class == X.class` 必须为真）。
    ///
    /// 类对象是**真对象**（运行时类是 `java/lang/Class`），不是另一种句柄 —— 这样
    /// `java.lang.Class` 自己的字节码（`getName`/`getSuperclass`/`isInstance` …）里的
    /// `getfield`/`putfield` 才能照常工作。`name` 字段先填好（`getName` 是 Java 代码，
    /// 它自己也会在第一次访问时补上）。
    pub(crate) fn class_obj(&mut self, desc: &str) -> i32 {
        if let Some(h) = self.class_obj_cache.get(desc) {
            return *h;
        }
        let n = self.store.field_count("java/lang/Class").max(1);
        let mut slots = vec![Slot::I(0); n];
        // `Class.name` 存的是**二进制名**（点号）：`Class.getName()` 直接返回它，
        // 数组只换分隔符 —— `[Ljava/lang/String;` → `[Ljava.lang.String;`；
        // 基本类型是 Java 的写法（`I` 的 `getName()` 是 `int`，数组才是 `[I`）
        let name_h = self.str_new(java_class_name(desc));
        if let Some(i) = self.store.field_index("java/lang/Class", "name") {
            if i < slots.len() {
                slots[i] = Slot::I(name_h);
            }
        }
        self.objects.push(slots);
        self.object_class.push("java/lang/Class".to_string());
        let h = self.objects.len() as i32;
        self.class_obj_cache.insert(desc.to_string(), h);
        self.class_objs.insert(h, desc.to_string());
        h
    }

    /// 类对象句柄 → 它代表的类描述符（数组是 `[I` / `[Ljava/lang/String;` 这种）
    pub(crate) fn class_obj_name(&self, h: i32) -> Option<String> {
        self.class_objs.get(&h).cloned()
    }

    /// 任意引用 → 它的运行时类描述符（`getClass()` 的语义；null 没有类）
    pub(crate) fn class_of(&self, h: i32) -> Option<String> {
        match self.ref_kind(h) {
            RefKind::Null => None,
            RefKind::Str => Some("java/lang/String".to_string()),
            RefKind::Obj(c) => Some(c),
            RefKind::Arr(d) => Some(d),
        }
    }

    // ---------- 字符串句柄的字段视图 ----------

    /// 把字符串句柄的文本摊成真正的 `java.lang.String` 实例字段
    /// （`value:[B` / `coder:B` / `hash:I` / `hashIsZero:Z`）。
    ///
    /// 为什么要它：真 java.base 的 `String.length()` 是 `value.length >> coder`，
    /// 走的是 `getfield`。而 `ldc "abc"` 给的是字符串堆句柄（没有字段槽）。
    /// 两边必须能互相看见，否则「真字节码」一碰字符串常量就断。
    pub(super) fn str_field_slots(&mut self, h: i32) -> &mut Vec<Slot> {
        if !self.str_fields.contains_key(&h) {
            let text = self.string_text(h);
            let latin1 = text.chars().all(|c| (c as u32) <= 0xFF);
            let mut bytes: Vec<Slot> = Vec::with_capacity(text.len());
            if latin1 {
                for c in text.chars() {
                    bytes.push(Slot::I(c as i32));
                }
            } else {
                for u in text.encode_utf16() {
                    bytes.push(Slot::I((u >> 8) as i32)); // UTF16 是大端（StringUTF16.getChar）
                    bytes.push(Slot::I((u & 0xFF) as i32));
                }
            }
            self.arrays.push(bytes);
            self.array_class.push("[B".to_string());
            let arr = -(self.arrays.len() as i32);
            let coder = if latin1 { 0 } else { 1 };
            let mut slots = vec![Slot::I(0); self.str_field_count()];
            for (name, v) in [("value", Slot::I(arr)), ("coder", Slot::I(coder))] {
                if let Some(i) = self.str_field_index(name) {
                    slots[i] = v;
                }
            }
            self.str_fields.insert(h, slots);
        }
        self.str_fields.get_mut(&h).expect("刚插入")
    }

    /// `java.lang.String` 的实例字段数（没加载这个类时按 4：value/coder/hash/hashIsZero）
    pub(super) fn str_field_count(&self) -> usize {
        let n = self.store.field_count("java/lang/String");
        if n == 0 {
            4
        } else {
            n
        }
    }

    /// `java.lang.String` 的字段名 → 槽位下标（没加载这个类时按声明顺序兜底）
    pub(super) fn str_field_index(&self, name: &str) -> Option<usize> {
        if let Some(i) = self.store.field_index("java/lang/String", name) {
            return Some(i);
        }
        match name {
            "value" => Some(0),
            "coder" => Some(1),
            "hash" => Some(2),
            "hashIsZero" => Some(3),
            _ => None,
        }
    }

    /// `java.lang.String` 的字段名 → 槽位下标；不是它的字段就报错（**不许**悄悄读 value）
    pub(super) fn str_field_slot(&self, name: &str) -> Result<usize, super::VmError> {
        self.str_field_index(name)
            .ok_or_else(|| super::VmError { msg: format!("java.lang.String 没有实例字段 {name}"), exc: None })
    }

    /// `java.lang.String` 实例（真字节码 `new` 出来的那个）→ 文本；不是就 None
    pub(crate) fn real_string_text(&self, h: i32) -> Option<String> {
        if !matches!(self.ref_kind(h), RefKind::Obj(ref c) if c == "java/lang/String") {
            return None;
        }
        let f = self.objects.get(h as usize - 1)?;
        let arr = f.get(self.str_field_index("value")?)?.as_i32();
        let coder = f.get(self.str_field_index("coder")?)?.as_i32();
        if arr >= 0 {
            return None; // value 还没被 <init> 赋上
        }
        let bytes = self.arrays.get((-arr - 1) as usize)?;
        let mut out = String::new();
        if coder == 0 {
            for s in bytes {
                out.push(char::from_u32((s.as_i32() & 0xFF) as u32).unwrap_or('\0'));
            }
        } else {
            let mut i = 0;
            while i + 1 < bytes.len() {
                let u = ((bytes[i].as_i32() & 0xFF) << 8) | (bytes[i + 1].as_i32() & 0xFF);
                i += 2;
                match char::from_u32(u as u32) {
                    Some(c) => out.push(c),
                    None => out.push('\u{fffd}'),
                }
            }
        }
        Some(out)
    }
}
