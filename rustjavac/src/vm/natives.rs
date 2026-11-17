//! 内建 JDK 类的解释器实现：`String` / 五个包装类 / `Object` 协议 /
//! `System`、`Arrays` 的静态方法。
//!
//! 这些类没有 `.class` 在 classpath 上（`store.resolve_method` 找不到），
//! 所以调用点在 `exec` 里先问这张表；表本身在 `crate::jdk`（与 JIT 共用）。

use crate::jdk;

use super::heap::{RefKind, STR_BASE};
use super::slot::Slot;
use super::Vm;
use super::VmError;

/// 小整数缓存范围（与 Java 的 `Integer.valueOf` 一致：-128..=127）
const INT_CACHE: i32 = 128;

impl Vm {
    /// `System.out.println(...)` 的文本：按重载（参数类型）决定怎么格式化
    pub(super) fn println_text(&self, desc: &str, v: Slot) -> String {
        match desc {
            "(J)V" => v.as_i64().to_string(),
            "(D)V" => crate::java_double_to_string(v.as_f64()),
            // float 的十进制形态与 double 不同（`1.0f/3.0f` → 0.33333334），必须用 f32 规则
            "(F)V" => crate::java_float_to_string(v.as_f32()),
            "(Z)V" => (if v.as_i32() != 0 { "true" } else { "false" }).to_string(),
            "(C)V" => char::from_u32(v.as_i32() as u32).unwrap_or('\0').to_string(),
            "(Ljava/lang/String;)V" | "(Ljava/lang/Object;)V" => self.obj_to_string(v.as_i32()),
            _ => v.as_i32().to_string(),
        }
    }
    /// 包装类对象 → 它的值；不是包装类就给 None
    pub(super) fn wrapper_value(&self, h: i32) -> Option<Slot> {
        let RefKind::Obj(cls) = self.ref_kind(h) else { return None };
        crate::jdk::wrapper_prim(&cls)?;
        self.objects.get(h as usize - 1).and_then(|f| f.first().copied())
    }

    fn wrapper_class(&self, h: i32) -> Option<String> {
        match self.ref_kind(h) {
            RefKind::Obj(cls) if crate::jdk::wrapper_prim(&cls).is_some() => Some(cls),
            _ => None,
        }
    }

    /// 引用 → 文本（`String.valueOf` / `println(Object)` / `append(Object)` 共用）。
    ///
    /// Java 的规则：null → "null"，String → 内容，包装类 → 值的文本，
    /// 其余 → `Klass@hash`。
    pub(super) fn obj_to_string(&self, h: i32) -> String {
        match self.ref_kind(h) {
            RefKind::Null => "null".to_string(),
            RefKind::Str => self.string_text(h),
            RefKind::Obj(cls) => {
                // 真字节码 `new` 出来的 String 对象：文本在它的 value/coder 字段里
                if cls == "java/lang/String" {
                    if let Some(text) = self.real_string_text(h) {
                        return text;
                    }
                }
                if let Some(v) = self.wrapper_value(h) {
                    return match v {
                        Slot::F(d) => crate::java_double_to_string(d),
                        Slot::Fl(f) => crate::java_float_to_string(f),
                        Slot::L(n) => n.to_string(),
                        Slot::I(n) => {
                            if cls == "java/lang/Boolean" {
                                (if n != 0 { "true" } else { "false" }).to_string()
                            } else if cls == "java/lang/Character" {
                                char::from_u32(n as u32).unwrap_or('?').to_string()
                            } else {
                                n.to_string()
                            }
                        }
                    };
                }
                format!("{}@{}", cls.replace('/', "."), self.identity_hash(h))
            }
            RefKind::Arr(d) => format!("{}@{}", d.replace('/', "."), self.identity_hash(h)),
        }
    }

    /// 对象的标识哈希：同一对象多次调用结果相同、不同对象不同（Java 的 `Object.hashCode`）
    pub(super) fn identity_hash(&self, h: i32) -> i32 {
        // 字符串按内容算（Java 的 `String.hashCode` 就是这么算的）：
        // `"abc".hashCode()` 与 HotSpot 的数值也一致
        let text = match self.ref_kind(h) {
            RefKind::Str => Some(self.string_text(h)),
            RefKind::Obj(ref c) if c == "java/lang/String" => self.real_string_text(h),
            _ => None,
        };
        if let Some(text) = text {
            let mut hash = 0i32;
            for c in text.encode_utf16() {
                hash = hash.wrapping_mul(31).wrapping_add(c as i32);
            }
            return hash;
        }
        // 其余按句柄（不搬动的下标）算：稳定、且不同对象不同
        let mut x = h as u32;
        x ^= x >> 16;
        x = x.wrapping_mul(0x7feb_352d);
        x ^= x >> 15;
        (x & 0x7fff_ffff) as i32
    }

    // ---------- 堆上新值 ----------

    pub(super) fn str_new(&mut self, s: String) -> i32 {
        self.strings.push(s);
        STR_BASE + self.strings.len() as i32 - 1
    }

    /// 字符串句柄的文本；不是字符串就当空串
    pub(super) fn str_of(&self, h: i32) -> String {
        match self.ref_kind(h) {
            RefKind::Str => self.string_text(h),
            _ => String::new(),
        }
    }

    /// 任意引用 → 文本：字符串堆句柄**和**真字节码 `new` 出来的 String 对象都能取
    pub(super) fn text_of(&self, h: i32) -> String {
        match self.ref_kind(h) {
            RefKind::Str => self.string_text(h),
            RefKind::Obj(ref c) if c == "java/lang/String" => self.real_string_text(h).unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// `java.lang.reflect.Field` 对象 → (声明类名, 字段名)。
    /// 我们的 VM 里 Field 的唯一用途是 `Unsafe.objectFieldOffset`。
    pub(super) fn reflect_field_parts(&self, h: i32) -> (Option<String>, Option<String>) {
        if !matches!(self.ref_kind(h), RefKind::Obj(ref c) if c == "java/lang/reflect/Field") {
            return (None, None);
        }
        let Some(slots) = self.objects.get(h as usize - 1) else { return (None, None) };
        let get = |name: &str| -> Option<Slot> {
            let i = self.store.field_index("java/lang/reflect/Field", name)?;
            slots.get(i).copied()
        };
        let cls = get("clazz").and_then(|s| self.class_obj_name(s.as_i32()));
        let name = get("name").map(|s| self.text_of(s.as_i32()));
        (cls, name)
    }

    /// 新建一个包装类对象（字段 0 = 值），含小整数缓存与布尔单例
    pub(super) fn new_wrapper(&mut self, cls: &str, v: Slot) -> i32 {
        // Boolean 只有两个值：与 Java 一样返回单例（`Boolean.TRUE == Boolean.TRUE`）
        if cls == "java/lang/Boolean" {
            let key = format!("Boolean#{}", if v.as_i32() != 0 { 1 } else { 0 });
            if let Some(h) = self.wrapper_cache.get(&key) {
                return *h;
            }
            let h = self.alloc_wrapper(cls, Slot::I(if v.as_i32() != 0 { 1 } else { 0 }));
            self.wrapper_cache.insert(key, h);
            return h;
        }
        if cls == "java/lang/Integer" {
            let n = v.as_i32();
            if (-INT_CACHE..INT_CACHE).contains(&n) {
                let key = format!("Integer#{n}");
                if let Some(h) = self.wrapper_cache.get(&key) {
                    return *h;
                }
                let h = self.alloc_wrapper(cls, v);
                self.wrapper_cache.insert(key, h);
                return h;
            }
        }
        self.alloc_wrapper(cls, v)
    }

    fn alloc_wrapper(&mut self, cls: &str, v: Slot) -> i32 {
        self.objects.push(vec![v]);
        self.object_class.push(cls.to_string());
        self.objects.len() as i32
    }

    /// 抛一个 Java 异常（新建异常对象，交给异常表处理）
    pub(super) fn throw(&mut self, class: &str) -> VmError {
        self.objects.push(vec![Slot::I(0)]);
        self.object_class.push(class.to_string());
        let h = self.objects.len() as i32;
        VmError { msg: format!("抛出 {class}"), exc: Some(h) }
    }

    /// `new X(...)` 的 Rust 侧入口：分配对象并**真的执行**构造器字节码。
    /// native 实现里要造 Java 对象时用（`VM.initialize()` 要一个空 HashMap）。
    pub(super) fn new_instance(&mut self, class: &str, ctor_desc: &str) -> Result<i32, VmError> {
        self.ensure_init(class)?;
        let n = self.store.field_count(class);
        self.objects.push(vec![Slot::I(0); n.max(1)]);
        self.object_class.push(class.to_string());
        let h = self.objects.len() as i32;
        if let Some((ci, mi)) = self.store.resolve_method(class, "<init>", ctor_desc) {
            self.depth += 1;
            let r = self.execute(ci, mi, ctor_desc, &[Slot::I(h)], true);
            self.depth -= 1;
            r?;
        }
        Ok(h)
    }

    // ---------- 调用分派 ----------

    /// 虚方法内建分派：None 表示「不是内建，按正常方法解析走」
    pub(super) fn builtin_virtual(
        &mut self,
        class: &str,
        name: &str,
        desc: &str,
        recv: i32,
        args: &[Slot],
    ) -> Option<Result<Slot, VmError>> {
        let op = match jdk::vop(class, name, desc) {
            Some(op) => op,
            // 没覆写的 `toString`/`hashCode`/`equals`：继承自 Object（没有它的 .class）
            None => {
                let ptr = jdk::object_protocol(name, args.len())?;
                if ptr != desc {
                    return None;
                }
                jdk::vop("java/lang/Object", name, desc)?
            }
        };
        Some(self.builtin_virtual_op(op, desc, recv, args))
    }

    fn builtin_virtual_op(
        &mut self,
        op: u32,
        desc: &str,
        recv: i32,
        args: &[Slot],
    ) -> Result<Slot, VmError> {
        let name = jdk::VIRTUALS[op as usize].name;
        let a = args.first().copied().unwrap_or(Slot::I(0));
        let b = args.get(1).copied().unwrap_or(Slot::I(0));
        // ---- String ----
        if matches!(self.ref_kind(recv), RefKind::Str) {
            return self.string_op(name, desc, recv, a, b);
        }
        // ---- 包装类 ----
        if let Some(cls) = self.wrapper_class(recv) {
            let v = self.wrapper_value(recv).unwrap_or(Slot::I(0));
            return self.wrapper_op(&cls, name, v, a);
        }
        // ---- Object 协议（toString / hashCode / equals 的默认实现）----
        match name {
            "hashCode" => Ok(Slot::I(self.identity_hash(recv))),
            "toString" => {
                let text = self.obj_to_string(recv);
                let h = self.str_new(text);
                Ok(Slot::I(h))
            }
            "equals" => Ok(Slot::I(if recv == a.as_i32() { 1 } else { 0 })),
            other => Err(VmError { msg: format!("内建方法 {other} 未实现"), exc: None }),
        }
    }

    /// `java.lang.String` 的方法（ASCII 范围内与 HotSpot 一致）
    fn string_op(
        &mut self,
        name: &str,
        desc: &str,
        recv: i32,
        a: Slot,
        b: Slot,
    ) -> Result<Slot, VmError> {
        let s = self.string_text(recv);
        // 重载按描述符区分（不能按实参个数：`replace(CC)` 也是两个实参）
        let two = matches!(
            desc,
            "(II)Ljava/lang/String;" | "(Ljava/lang/String;I)I"
        ) || desc.starts_with("(Ljava/lang/CharSequence;L");
        let chars: Vec<char> = s.chars().collect();
        let chars_of = |h: i32| -> Vec<char> {
            match self.ref_kind(h) {
                RefKind::Str => self.string_text(h).chars().collect(),
                _ => Vec::new(),
            }
        };
        // 子串查找（按字符下标）：找不到返回 None
        let find = |hay: &[char], needle: &[char], from: usize| -> Option<usize> {
            if needle.is_empty() {
                return Some(from.min(hay.len()));
            }
            if needle.len() > hay.len() {
                return None;
            }
            (from..=hay.len().saturating_sub(needle.len())).find(|&i| hay[i..i + needle.len()] == *needle)
        };
        let out = match name {
            "length" => Slot::I(chars.len() as i32),
            "charAt" => {
                let i = a.as_i32();
                match usize::try_from(i).ok().and_then(|i| chars.get(i)) {
                    Some(c) => Slot::I(*c as i32),
                    None => return Err(self.throw("java/lang/StringIndexOutOfBoundsException")),
                }
            }
            "isEmpty" => Slot::I(if chars.is_empty() { 1 } else { 0 }),
            "substring" => {
                let start = a.as_i32().max(0) as usize;
                let end = if two { b.as_i32().max(0) as usize } else { chars.len() };
                if start > end || end > chars.len() {
                    return Err(self.throw("java/lang/StringIndexOutOfBoundsException"));
                }
                Slot::I(self.str_new(chars[start..end].iter().collect()))
            }
            "indexOf" => {
                // 三种重载：indexOf(String) / indexOf(int) / indexOf(String,int)
                let (needle, from) = if two {
                    (chars_of(a.as_i32()), b.as_i32().max(0) as usize)
                } else if let RefKind::Str = self.ref_kind(a.as_i32()) {
                    (chars_of(a.as_i32()), 0)
                } else {
                    (vec![a.as_i32() as u8 as char], 0)
                };
                Slot::I(find(&chars, &needle, from).map(|i| i as i32).unwrap_or(-1))
            }
            "contains" => {
                let needle = chars_of(a.as_i32());
                Slot::I(if find(&chars, &needle, 0).is_some() { 1 } else { 0 })
            }
            "startsWith" => {
                let p = chars_of(a.as_i32());
                Slot::I(if chars.len() >= p.len() && chars[..p.len()] == p[..] { 1 } else { 0 })
            }
            "endsWith" => {
                let p = chars_of(a.as_i32());
                Slot::I(if chars.len() >= p.len() && chars[chars.len() - p.len()..] == p[..] { 1 } else { 0 })
            }
            "toUpperCase" => {
                let t: String = s.chars().map(|c| c.to_ascii_uppercase()).collect();
                Slot::I(self.str_new(t))
            }
            "toLowerCase" => {
                let t: String = s.chars().map(|c| c.to_ascii_lowercase()).collect();
                Slot::I(self.str_new(t))
            }
            "trim" => {
                let t = s.trim_matches(|c: char| c <= ' ').to_string();
                Slot::I(self.str_new(t))
            }
            "replace" => {
                if two {
                    // replace(CharSequence, CharSequence)
                    let from = self.str_of(a.as_i32());
                    let to = self.str_of(b.as_i32());
                    let t = if from.is_empty() { s.clone() } else { s.replace(&from, &to) };
                    Slot::I(self.str_new(t))
                } else {
                    // replace(char, char)
                    let from = char::from_u32(a.as_i32() as u32).unwrap_or('\0');
                    let to = char::from_u32(b.as_i32() as u32).unwrap_or('\0');
                    let t: String = s.chars().map(|c| if c == from { to } else { c }).collect();
                    Slot::I(self.str_new(t))
                }
            }
            "compareTo" => {
                let other = self.string_text(a.as_i32());
                let mut r = 0i32;
                for (x, y) in s.chars().zip(other.chars()) {
                    if x != y {
                        r = x as i32 - y as i32;
                        break;
                    }
                }
                if r == 0 {
                    r = s.chars().count() as i32 - other.chars().count() as i32;
                }
                Slot::I(r)
            }
            "concat" => {
                let mut t = s.clone();
                t.push_str(&self.string_text(a.as_i32()));
                Slot::I(self.str_new(t))
            }
            "split" => {
                let sep = self.str_of(a.as_i32());
                let parts: Vec<String> = if sep.is_empty() {
                    s.chars().map(|c| c.to_string()).collect()
                } else {
                    s.split(sep.as_str()).map(|x| x.to_string()).collect()
                };
                // 建 String[]：数组堆里存字符串句柄
                let mut elems = Vec::with_capacity(parts.len());
                for p in parts {
                    elems.push(Slot::I(self.str_new(p)));
                }
                self.arrays.push(elems);
                self.array_class.push("[Ljava/lang/String;".to_string());
                Slot::I(-(self.arrays.len() as i32))
            }
            "equals" => {
                let same = match self.ref_kind(a.as_i32()) {
                    RefKind::Str => self.string_text(a.as_i32()) == s,
                    _ => false,
                };
                Slot::I(if same { 1 } else { 0 })
            }
            "hashCode" => Slot::I(self.identity_hash(recv)),
            "toString" => Slot::I(recv),
            other => return Err(VmError { msg: format!("String 没有方法 {other}"), exc: None }),
        };
        Ok(out)
    }

    fn wrapper_op(&mut self, cls: &str, name: &str, v: Slot, a: Slot) -> Result<Slot, VmError> {
        let text = || match v {
            Slot::F(d) => crate::java_double_to_string(d),
            Slot::Fl(f) => crate::java_float_to_string(f),
            Slot::L(n) => n.to_string(),
            Slot::I(n) => {
                if cls == "java/lang/Boolean" {
                    (if n != 0 { "true" } else { "false" }).to_string()
                } else if cls == "java/lang/Character" {
                    char::from_u32(n as u32).unwrap_or('?').to_string()
                } else {
                    n.to_string()
                }
            }
        };
        match name {
            "intValue" => Ok(Slot::I(match v {
                Slot::F(d) => d as i32,
                other => other.as_i64() as i32,
            })),
            "longValue" => Ok(Slot::L(match v {
                Slot::F(d) => d as i64,
                other => other.as_i64(),
            })),
            "doubleValue" => Ok(Slot::F(v.as_f64())),
            "booleanValue" => Ok(Slot::I(if v.as_i32() != 0 { 1 } else { 0 })),
            "charValue" => Ok(Slot::I(v.as_i32())),
            "toString" => {
                let h = self.str_new(text());
                Ok(Slot::I(h))
            }
            "hashCode" => Ok(Slot::I(match v {
                Slot::F(d) => {
                    let bits = d.to_bits();
                    (bits ^ (bits >> 32)) as i32
                }
                Slot::Fl(f) => f.to_bits() as i32,
                Slot::L(n) => (n ^ (n as u64 >> 32) as i64) as i32,
                Slot::I(n) => {
                    if cls == "java/lang/Boolean" {
                        if n != 0 { 1231 } else { 1237 }
                    } else {
                        n
                    }
                }
            })),
            "equals" => {
                let other = a.as_i32();
                let same = match self.wrapper_class(other) {
                    Some(ocls) if ocls == cls => self.wrapper_value(other) == Some(v),
                    _ => false,
                };
                Ok(Slot::I(if same { 1 } else { 0 }))
            }
            other => Err(VmError { msg: format!("{cls} 没有方法 {other}"), exc: None }),
        }
    }

    /// 静态方法内建分派（op 号与 JIT 共用 `crate::jdk::STATICS` 的下标）
    pub(super) fn builtin_static(
        &mut self,
        class: &str,
        name: &str,
        desc: &str,
        args: &[Slot],
    ) -> Option<Result<Slot, VmError>> {
        let op = jdk::sop(class, name, desc)?;
        Some(self.builtin_static_op(op, args))
    }

    fn builtin_static_op(&mut self, op: u32, args: &[Slot]) -> Result<Slot, VmError> {
        let e = &jdk::STATICS[op as usize];
        let (class, name) = (e.class, e.name);
        let a = args.first().copied().unwrap_or(Slot::I(0));
        match (class, name) {
            ("java/lang/Integer", "valueOf") => {
                Ok(Slot::I(self.new_wrapper("java/lang/Integer", Slot::I(a.as_i32()))))
            }
            ("java/lang/Long", "valueOf") => Ok(Slot::I(self.new_wrapper("java/lang/Long", Slot::L(a.as_i64())))),
            ("java/lang/Double", "valueOf") => {
                Ok(Slot::I(self.new_wrapper("java/lang/Double", Slot::F(a.as_f64()))))
            }
            ("java/lang/Boolean", "valueOf") => Ok(Slot::I(
                self.new_wrapper("java/lang/Boolean", Slot::I(if a.as_i32() != 0 { 1 } else { 0 })),
            )),
            ("java/lang/Character", "valueOf") => {
                Ok(Slot::I(self.new_wrapper("java/lang/Character", Slot::I(a.as_i32()))))
            }
            ("java/lang/Integer", "parseInt") => {
                let s = self.str_of(a.as_i32()).trim().to_string();
                match s.parse::<i32>() {
                    Ok(v) => Ok(Slot::I(v)),
                    Err(_) => Err(self.throw("java/lang/NumberFormatException")),
                }
            }
            ("java/lang/Long", "parseLong") => {
                let s = self.str_of(a.as_i32()).trim().to_string();
                match s.parse::<i64>() {
                    Ok(v) => Ok(Slot::L(v)),
                    Err(_) => Err(self.throw("java/lang/NumberFormatException")),
                }
            }
            ("java/lang/Double", "parseDouble") => {
                let s = self.str_of(a.as_i32()).trim().to_string();
                match s.parse::<f64>() {
                    Ok(v) => Ok(Slot::F(v)),
                    Err(_) => Err(self.throw("java/lang/NumberFormatException")),
                }
            }
            ("java/lang/Integer", "toString") => Ok(Slot::I(self.str_new(a.as_i32().to_string()))),
            ("java/lang/Long", "toString") => Ok(Slot::I(self.str_new(a.as_i64().to_string()))),
            ("java/lang/Double", "toString") => {
                Ok(Slot::I(self.str_new(crate::java_double_to_string(a.as_f64()))))
            }
            ("java/lang/System", "currentTimeMillis") => Ok(Slot::L(now_millis())),
            ("java/lang/System", "nanoTime") => Ok(Slot::L(now_nanos())),
            ("java/lang/System", "arraycopy") => {
                let src = a.as_i32();
                let sp = args.get(1).copied().unwrap_or(Slot::I(0)).as_i32();
                let dst = args.get(2).copied().unwrap_or(Slot::I(0)).as_i32();
                let dp = args.get(3).copied().unwrap_or(Slot::I(0)).as_i32();
                let n = args.get(4).copied().unwrap_or(Slot::I(0)).as_i32();
                self.array_copy(src, sp, dst, dp, n)?;
                Ok(Slot::I(0))
            }
            ("java/util/Arrays", "toString") => {
                let t = self.int_array_text(a.as_i32())?;
                Ok(Slot::I(self.str_new(t)))
            }
            ("java/util/Arrays", "sort") => {
                self.int_array_sort(a.as_i32())?;
                Ok(Slot::I(0))
            }
            _ => Err(VmError { msg: format!("内建静态方法 {class}.{name} 未实现"), exc: None }),
        }
    }

    /// 内建静态字段的值（`Integer.MAX_VALUE` 这类）
    pub(super) fn builtin_static_field(&mut self, class: &str, name: &str) -> Option<Slot> {
        Some(match (class, name) {
            ("java/lang/Integer", "MAX_VALUE") => Slot::I(i32::MAX),
            ("java/lang/Integer", "MIN_VALUE") => Slot::I(i32::MIN),
            ("java/lang/Long", "MAX_VALUE") => Slot::L(i64::MAX),
            ("java/lang/Long", "MIN_VALUE") => Slot::L(i64::MIN),
            ("java/lang/Double", "MAX_VALUE") => Slot::F(f64::MAX),
            ("java/lang/Double", "MIN_VALUE") => Slot::F(f64::from_bits(1)), // Java 的 MIN_VALUE 是最小 **subnormal**
            ("java/lang/Double", "NaN") => Slot::F(f64::NAN),
            ("java/lang/Float", "MAX_VALUE") => Slot::Fl(f32::MAX),
            ("java/lang/Float", "MIN_VALUE") => Slot::Fl(f32::from_bits(1)), // 同上：最小 subnormal（1.4E-45）
            ("java/lang/Float", "NaN") => Slot::Fl(f32::NAN),
            ("java/lang/Character", "MAX_VALUE") => Slot::I(u16::MAX as i32),
            ("java/lang/Character", "MIN_VALUE") => Slot::I(0),
            ("java/lang/Boolean", "TRUE") => {
                Slot::I(self.new_wrapper("java/lang/Boolean", Slot::I(1)))
            }
            ("java/lang/Boolean", "FALSE") => {
                Slot::I(self.new_wrapper("java/lang/Boolean", Slot::I(0)))
            }
            _ => return None,
        })
    }

    // ---------- 数组工具 ----------

    fn int_array_text(&self, h: i32) -> Result<String, VmError> {
        if h == 0 {
            return Ok("null".to_string());
        }
        match self.ref_kind(h) {
            RefKind::Arr(d) if d == "[I" => {
                let arr = &self.arrays[(-h - 1) as usize];
                let parts: Vec<String> = arr.iter().map(|v| v.as_i32().to_string()).collect();
                Ok(format!("[{}]", parts.join(", ")))
            }
            _ => Err(VmError { msg: "Arrays.toString 只支持 int[]".into(), exc: None }),
        }
    }

    fn int_array_sort(&mut self, h: i32) -> Result<(), VmError> {
        if h == 0 {
            return Ok(());
        }
        match self.ref_kind(h) {
            RefKind::Arr(d) if d == "[I" => {
                if let Some(arr) = self.arrays.get_mut((-h - 1) as usize) {
                    let mut vals: Vec<i32> = arr.iter().map(|v| v.as_i32()).collect();
                    vals.sort_unstable();
                    for (i, v) in vals.into_iter().enumerate() {
                        arr[i] = Slot::I(v);
                    }
                }
                Ok(())
            }
            _ => Err(VmError { msg: "Arrays.sort 只支持 int[]".into(), exc: None }),
        }
    }

    pub(super) fn array_copy(&mut self, src: i32, sp: i32, dst: i32, dp: i32, n: i32) -> Result<(), VmError> {
        let sd = self.array_desc(src);
        let dd = self.array_desc(dst);
        if sd.is_none() || sd != dd || n < 0 || sp < 0 || dp < 0 {
            return Err(VmError { msg: "System.arraycopy 参数非法".into(), exc: None });
        }
        match sd.as_deref() {
            // 引用元素的数组：搬句柄即可（句柄就是 GC 的引用本身）
            Some(d) if d.starts_with("[L") || d.starts_with("[[") => {
                let mut tmp = Vec::with_capacity(n as usize);
                {
                    let a = &self.arrays[(-src - 1) as usize];
                    for i in 0..n as usize {
                        tmp.push(*a.get(sp as usize + i).ok_or_else(|| VmError {
                            msg: "System.arraycopy 越界".into(),
                            exc: None,
                        })?);
                    }
                }
                let d = &mut self.arrays[(-dst - 1) as usize];
                for (i, v) in tmp.into_iter().enumerate() {
                    if dp as usize + i >= d.len() {
                        return Err(VmError { msg: "System.arraycopy 越界".into(), exc: None });
                    }
                    d[dp as usize + i] = v;
                }
            }
            _ => {
                let mut tmp = Vec::with_capacity(n as usize);
                {
                    let a = &self.arrays[(-src - 1) as usize];
                    for i in 0..n as usize {
                        tmp.push(*a.get(sp as usize + i).ok_or_else(|| VmError {
                            msg: "System.arraycopy 越界".into(),
                            exc: None,
                        })?);
                    }
                }
                let d = &mut self.arrays[(-dst - 1) as usize];
                for (i, v) in tmp.into_iter().enumerate() {
                    if dp as usize + i >= d.len() {
                        return Err(VmError { msg: "System.arraycopy 越界".into(), exc: None });
                    }
                    d[dp as usize + i] = v;
                }
            }
        }
        Ok(())
    }

    fn array_desc(&self, h: i32) -> Option<String> {
        if h >= 0 {
            return None;
        }
        self.array_class.get((-h - 1) as usize).cloned()
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn now_nanos() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}
