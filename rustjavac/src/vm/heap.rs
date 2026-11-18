//! 对象 / 数组 / 字符串 / 类对象 四套堆与引用句柄编码。

use super::shared::lk;
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
pub(super) fn java_class_name(desc: &str) -> String {
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
        self.sh.heap.string_handle(class, cp_index, || self.string_at(class, cp_index))
    }

    pub(super) fn string_text(&self, handle: i32) -> String {
        self.sh.heap.string_text(handle)
    }

    pub(super) fn ref_kind(&self, handle: i32) -> RefKind {
        if handle == 0 {
            RefKind::Null
        } else if handle >= STR_BASE {
            RefKind::Str
        } else {
            match self.sh.heap.class_of(handle) {
                Some(c) if handle > 0 => RefKind::Obj(c.to_string()),
                Some(d) => RefKind::Arr(d.to_string()),
                // 句柄指向一个已经不存在的对象：报 null 而不是瞎猜一个类名
                None => RefKind::Null,
            }
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
        // 两种写法都收：内部名（`java/lang/String`）与字段描述符（`Ljava/lang/String;`）。
        // 不归一化的话 `[Ljava/lang/String;` 的元素会被当成一个叫 `Ljava/lang/String;`
        // 的类 —— `getComponentType().getName()` 就变成 `Ljava.lang.String;`（踩过）
        let desc = if desc.len() >= 2 && desc.starts_with('L') && desc.ends_with(';') {
            &desc[1..desc.len() - 1]
        } else {
            desc
        };
        if let Some(h) = lk(&self.sh.class_obj_cache).get(desc) {
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
        // 反射要看的那三个字段也一起填好 —— 填了 `modifiers`/`primitive`/`componentType`
        // 之后，`isInterface()` / `isArray()` / `isPrimitive()` / `isAnnotation()` /
        // `getModifiers()` / `getComponentType()` 这些**纯字节码**方法就都对了，
        // 一行 Rust 都不用写（HotSpot 也是 VM 在 `Class` 对象上填这几个字段）。
        let (flags, primitive) = self.class_flags_and_primitive(desc);
        if let Some(i) = self.store.field_index("java/lang/Class", "modifiers") {
            if i < slots.len() {
                slots[i] = Slot::I(i32::from(flags));
            }
        }
        if let Some(i) = self.store.field_index("java/lang/Class", "primitive") {
            if i < slots.len() {
                slots[i] = Slot::I(i32::from(primitive));
            }
        }
        if let Some(comp) = desc.strip_prefix('[') {
            let ch = self.class_obj(comp);
            if let Some(i) = self.store.field_index("java/lang/Class", "componentType") {
                if i < slots.len() {
                    slots[i] = Slot::I(ch);
                }
            }
        }
        let h = self.sh.heap.new_object(slots, "java/lang/Class").unwrap_or(0);
        // **收口必须原子**：上面那次 `class_obj_cache` 查询和这里相隔一整段构造，
        // 两个线程同时 miss 就会各造一个 Class 对象、各自 return 自己那个
        // （实测 4 线程并发首次 `ldc X.class`：15 次里 7 次拿到 2~3 个不同对象）。
        // 后果是 `X.class == X.class` 为假，而 `synchronized (X.class)` 于是锁在
        // **不同的锁**上 —— 临界区不互斥、`n++` 丢更新，且全程没有异常可看。
        //
        // 所以：同名的第一个赢，后来者**用自己刚造的那个换赢家的句柄**（输家那个
        // 变成无主垃圾，下一轮 GC 收掉）。注意**不能**把 `new_object` 搬进锁里 ——
        // 数组会递归调 `class_obj(comp)`，`sh.class_obj_cache` 是不可重入的
        // `std::sync::Mutex`，搬进去就是永久挂死。
        let h = {
            let mut g = lk(&self.sh.class_obj_cache);
            match g.get(desc) {
                Some(&exist) => exist,
                None => {
                    g.insert(desc.to_string(), h);
                    h
                }
            }
        };
        lk(&self.sh.class_objs).insert(h, desc.to_string());
        h
    }

    /// 类对象的 `modifiers` 字段（`char`）与 `primitive` 字段。
    ///
    /// 口径按 `Class.getModifiers` 的 javadoc：
    /// - 数组：元素的 PUBLIC/PRIVATE/PROTECTED + ABSTRACT + FINAL（INTERFACE 位一律不带）；
    /// - 基本类型与 void：PUBLIC + ABSTRACT + FINAL；
    /// - 其余：class 文件的 `access_flags`。
    fn class_flags_and_primitive(&self, desc: &str) -> (u16, bool) {
        const PUBLIC: u16 = 0x0001;
        const FINAL: u16 = 0x0010;
        const ABSTRACT: u16 = 0x0400;
        if let Some(comp) = desc.strip_prefix('[') {
            let (cf_flags, _) = self.class_flags_and_primitive(comp);
            return (cf_flags & 0x0007 | FINAL | ABSTRACT, false);
        }
        if desc.len() == 1 {
            return (PUBLIC | FINAL | ABSTRACT, true);
        }
        // `ACC_SUPER`(0x0020) 是 class 文件里的历史标志，**不是** Java 修饰符 ——
        // 不摘掉它 `Modifier.toString` 会多出一个 `synchronized`（踩过）
        const ACC_SUPER: u16 = 0x0020;
        let mut flags = self.store.class_file(desc).map(|cf| cf.access_flags).unwrap_or(PUBLIC);
        // 成员类（`Outer$Inner`）的 `static` / `private` 等标志**不在**自己 class 文件的
        // access_flags 里，而在 `InnerClasses` 属性那一项上 —— 不并进来 `Modifier.toString`
        // 会把 `static class A` 显示成空字符串（踩过）
        if let Some(inner) = self.refl_inner_flags(desc) {
            flags |= inner;
        }
        (flags & !ACC_SUPER, false)
    }

    /// 类对象句柄 → 它代表的类描述符（数组是 `[I` / `[Ljava/lang/String;` 这种）
    pub(crate) fn class_obj_name(&self, h: i32) -> Option<String> {
        lk(&self.sh.class_objs).get(&h).cloned()
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
    pub(super) fn str_field_slots(&mut self, h: i32) -> Vec<Slot> {
        if self.sh.heap.str_field_snapshot(h).is_none() {
            let text = self.string_text(h);
            let latin1 = text.chars().all(|c| (c as u32) <= 0xFF);
            let mut bytes: Vec<Slot> = Vec::with_capacity(text.len());
            if latin1 {
                for c in text.chars() {
                    bytes.push(Slot::I(c as i32));
                }
            } else {
                // **JDK 25 的 `StringUTF16` 用本机字节序（小端）存 UTF-16** —— 不是早期版本的
                // 大端。实测：真 JDK 里 `"中"` 的 `value` 是 `2d 4e`（U+4E2D 低字节在前）。
                // 写错字节序的症状很隐蔽：ASCII 完全正常（高低字节都是 0x00），
                // 只有非 ASCII 会「字符值被字节对调」（U+4E2D 读成 U+2D4E），
                // 一路表现为 `getBytes("UTF-8")` 出来是乱码、CRC 对不上。
                for u in text.encode_utf16() {
                    bytes.push(Slot::I((u & 0xFF) as i32));
                    bytes.push(Slot::I((u >> 8) as i32));
                }
            }
            let arr = self.sh.heap.new_array(bytes, "[B").unwrap_or(0);
            let coder = if latin1 { 0 } else { 1 };
            let mut slots = vec![Slot::I(0); self.str_field_count()];
            for (name, v) in [("value", Slot::I(arr)), ("coder", Slot::I(coder))] {
                if let Some(i) = self.str_field_index(name) {
                    slots[i] = v;
                }
            }
            // **必须**把算好的 value/coder 装进去；只 ensure 一个全零向量会让
            // 字符串常量的 value=null、coder=0，"hello".hashCode() 变成 0、
            // String.equals 与真 String 对象永远不等（踩过）
            self.sh.heap.str_fields_install(h, slots);
        }
        self.sh.heap.str_field_snapshot(h).unwrap_or_default()
    }

    /// 字符串句柄的字段写（`putfield` 落在字符串上时用）
    pub(super) fn str_field_write(&mut self, h: i32, i: usize, v: Slot) {
        self.sh.heap.str_field_set(h, i, v);
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
        let f = self.sh.heap.snapshot(h)?;
        let arr = f.get(self.str_field_index("value")?)?.as_i32();
        let coder = f.get(self.str_field_index("coder")?)?.as_i32();
        if arr >= 0 {
            return None; // value 还没被 <init> 赋上
        }
        let bytes = self.sh.heap.snapshot(arr)?;
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
