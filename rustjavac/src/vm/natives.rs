//! 内建 JDK 类的解释器实现：`String` / 五个包装类 / `Object` 协议 /
//! `System`、`Arrays` 的静态方法。
//!
//! 这些类没有 `.class` 在 classpath 上（`store.resolve_method` 找不到），
//! 所以调用点在 `exec` 里先问这张表；表本身在 `crate::jdk`（与 JIT 共用）。

use crate::jdk;

use super::heap::{java_class_name, RefKind, STR_BASE};
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

    /// 把已缓冲的输出刷到 stdout（`System.exit` 前必须调：缓冲在 Vm 里，进程退出会丢）
    pub(super) fn flush_output(&mut self) {
        for line in self.output.drain(..) {
            println!("{line}");
        }
    }

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
                // 集合迭代器：`java/util/Iterator` **不能**进 VIRTUALS 表 ——
                // 表一登，JIT 的 `builtin_vop` 会把用户自己 implements Iterable 的
                // for-each 一起截胡。所以在表外单独认。
                if class == "java/util/Iterator" {
                    return self.iter_op(name, recv);
                }
                // 异常类：沿内置继承链往上找（`IllegalStateException.getMessage`
                // 要能落到 `Throwable` 的那一份；JIT 的 builtin_vop 不爬链，
                // 所以它在表里按类逐个登记，解释器这边爬链就能覆盖任意子类）
                let mut cur = class.to_string();
                for _ in 0..12 {
                    if let Some(op) = jdk::vop(&cur, name, desc) {
                        return Some(self.builtin_virtual_op(op, desc, recv, args));
                    }
                    match super::class::builtin_super(&cur) {
                        Some(s) => cur = s.to_string(),
                        None => break,
                    }
                }
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
        // `getClass()` 对所有接收者都一样（字符串句柄 / 包装类 / 数组 / lambda / 普通对象），
        // 所以放在 String / 包装类分支之前
        if name == "getClass" {
            let desc = self.class_of(recv).unwrap_or_else(|| "java/lang/Object".to_string());
            return Ok(Slot::I(self.class_obj(&desc)));
        }
        // ---- String ----
        if matches!(self.ref_kind(recv), RefKind::Str) {
            return self.string_op(name, desc, recv, a, b);
        }
        // ---- 包装类 ----
        if let Some(cls) = self.wrapper_class(recv) {
            let v = self.wrapper_value(recv).unwrap_or(Slot::I(0));
            return self.wrapper_op(&cls, name, v, a);
        }
        // ---- 追加的 API：集合 / Throwable ----
        if let RefKind::Obj(cls) = self.ref_kind(recv) {
            let cls_owned = cls.clone();
            if let Some(r) = self.api_virtual_op(&cls_owned, name, recv, a, b) {
                return r;
            }
        }
        // ---- Object 协议（toString / hashCode / equals 的默认实现）----
        match name {
            "getName" => {
                // `Class.getName()`：类对象里的名字是**二进制名**（`int` / `java.lang.String` /
                // `[Ljava.lang.String;`）—— 用描述符映射一次，与 `class_obj` 填的字段一致
                match self.class_obj_name(recv) {
                    Some(d) => Ok(Slot::I(self.str_new(java_class_name(&d)))),
                    None => Err(VmError {
                        msg: format!("getName() 的接收者不是类对象（{}）", self.type_name(recv)),
                        exc: None,
                    }),
                }
            }
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
        let (class, name, desc) = (e.class, e.name, e.desc);
        if let Some(r) = self.static_api_op(class, name, desc, args) {
            return r;
        }
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
        // `Integer.TYPE` 这一族：值是**基本类型的类对象**（要真的建出来，不是常量）
        if name == "TYPE" {
            let prim = match class {
                "java/lang/Integer" => Some("I"),
                "java/lang/Long" => Some("J"),
                "java/lang/Float" => Some("F"),
                "java/lang/Double" => Some("D"),
                "java/lang/Boolean" => Some("Z"),
                "java/lang/Character" => Some("C"),
                "java/lang/Byte" => Some("B"),
                "java/lang/Short" => Some("S"),
                "java/lang/Void" => Some("V"),
                _ => None,
            };
            if let Some(p) = prim {
                return Some(Slot::I(self.class_obj(p)));
            }
        }
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

// ============================================================================
// 集合框架 / Throwable / 格式化 / Math —— 追加的内建实现
// ============================================================================

/// 一个集合的运行时状态。
///
/// **解释器没有 GC**：对象句柄是 `objects` 的下标、数组句柄是 `arrays` 的下标，
/// 两者都永不搬动，所以这里直接存句柄是安全的（JIT 那边是搬动式 GC，
/// 那边改用真正的 Java 数组做后端，见 `jit/natives.rs`）。
#[derive(Clone)]
enum Coll {
    /// 动态数组（也是 keySet/values/entrySet 视图的存储）
    List(Vec<i32>),
    /// 键值对，顺序 = 插入顺序（线性扫描；样例规模足够）
    /// 每项：(规范化键, 原键句柄, 值句柄)
    Map(Vec<(Key, i32, i32)>),
    /// 迭代器：源集合编号 + 游标
    Iter(usize, usize),
}

thread_local! {
    static COLLS: std::cell::RefCell<Vec<Coll>> = std::cell::RefCell::new(Vec::new());
}

fn colls<R>(f: impl FnOnce(&mut Vec<Coll>) -> R) -> R {
    COLLS.with(|c| f(&mut c.borrow_mut()))
}

/// 集合相关类的判定：这些类的实例用 `COLLS` 里的槽存状态
fn is_coll_class(c: &str) -> bool {
    matches!(
        c,
        "java/util/ArrayList"
            | "java/util/List"
            | "java/util/Set"
            | "java/util/Collection"
            | "java/util/HashMap"
            | "java/util/Map"
            | "java/util/Map$Entry"
    )
}

/// 异常类：`getMessage` / `toString` / `printStackTrace` 走内建
fn is_throwable(c: &str) -> bool {
    matches!(
        c,
        "java/lang/Throwable" | "java/lang/Exception" | "java/lang/RuntimeException" | "java/lang/Error"
    )
}

/// 沿内置继承链判断（`IllegalStateException` → RuntimeException → … → Throwable）
fn is_throwable_chain(c: &str) -> bool {
    let mut cur = c.to_string();
    for _ in 0..12 {
        if is_throwable(&cur) {
            return true;
        }
        match super::class::builtin_super(&cur) {
            Some(s) => cur = s.to_string(),
            None => return false,
        }
    }
    false
}

impl Vm {
    /// 集合对象的槽号（`field0` 存「下标 + 1」，0 表示还没分配）
    fn coll_slot(&self, recv: i32) -> Option<usize> {
        if recv <= 0 {
            return None;
        }
        let v = self.objects.get(recv as usize - 1)?.first()?.as_i32();
        if v <= 0 {
            None
        } else {
            Some((v - 1) as usize)
        }
    }

    /// 给对象挂一个新状态槽（`recv > 0` 时同时把槽号写进 `field0`）
    fn coll_attach(&mut self, recv: i32, st: Coll) -> usize {
        let id = colls(|v| {
            v.push(st);
            v.len() - 1
        });
        if recv > 0 {
            if let Some(f) = self.objects.get_mut(recv as usize - 1) {
                if f.is_empty() {
                    f.push(Slot::I(0));
                }
                f[0] = Slot::I(id as i32 + 1);
            }
        }
        id
    }

    /// 取已有的状态槽；没有就按 `fresh()` 建一个
    fn coll_ensure(&mut self, recv: i32, fresh: impl FnOnce() -> Coll) -> usize {
        match self.coll_slot(recv) {
            Some(id) if id < colls(|v| v.len()) => id,
            _ => {
                let st = fresh();
                self.coll_attach(recv, st)
            }
        }
    }

    /// 分配一个「只有状态槽」的内建对象（视图 / 迭代器 / Entry）
    fn new_plain(&mut self, cls: &str, n: usize) -> i32 {
        self.objects.push(vec![Slot::I(0); n.max(1)]);
        self.object_class.push(cls.to_string());
        self.objects.len() as i32
    }

    /// 两个引用是不是「Java equals」：String 比内容、包装类比数值、其余比句柄
    fn refs_equal(&self, a: i32, b: i32) -> bool {
        if a == b {
            return true;
        }
        match (self.ref_kind(a), self.ref_kind(b)) {
            (RefKind::Str, RefKind::Str) => self.string_text(a) == self.string_text(b),
            (RefKind::Obj(ca), RefKind::Obj(cb)) if ca == cb => {
                match (self.wrapper_value(a), self.wrapper_value(b)) {
                    (Some(x), Some(y)) => x == y,
                    (None, None) => false,
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// 元素的自然序比较（`Arrays.sort(Object[])` 用）
    fn refs_cmp(&self, a: i32, b: i32) -> std::cmp::Ordering {
        match (self.ref_kind(a), self.ref_kind(b)) {
            (RefKind::Str, RefKind::Str) => self.string_text(a).cmp(&self.string_text(b)),
            _ => match (self.wrapper_value(a), self.wrapper_value(b)) {
                (Some(x), Some(y)) => x.as_f64().partial_cmp(&y.as_f64()).unwrap_or(std::cmp::Ordering::Equal),
                _ => std::cmp::Ordering::Equal,
            },
        }
    }

    /// 集合类的方法（`ArrayList`/`List`/`Set`/`Collection`/`HashMap`/`Map`/`Map$Entry`）
    fn coll_op(&mut self, cls: &str, name: &str, recv: i32, a: Slot, b: Slot) -> Result<Slot, VmError> {
        // ---- Map.Entry ----
        if cls == "java/util/Map$Entry" {
            let f = self.objects.get(recv as usize - 1).cloned().unwrap_or_default();
            let k = f.first().copied().unwrap_or(Slot::I(0)).as_i32();
            let v = f.get(1).copied().unwrap_or(Slot::I(0)).as_i32();
            return Ok(match name {
                "getKey" => Slot::I(k),
                "getValue" => Slot::I(v),
                "toString" => {
                    let t = format!("{}={}", self.obj_to_string(k), self.obj_to_string(v));
                    Slot::I(self.str_new(t))
                }
                other => return Err(VmError { msg: format!("Map.Entry 没有方法 {other}"), exc: None }),
            });
        }
        let is_map = cls.ends_with("HashMap") || cls == "java/util/Map";
        if is_map {
            return self.map_op(name, recv, a, b);
        }
        // 其余都按「动态数组」实现（Set/Collection 是 keySet()/values() 的视图）
        let id = self.coll_ensure(recv, || Coll::List(Vec::new()));
        enum Act {
            Get(usize),
            Remove(usize),
            Contains(i32),
            IndexOf(i32),
            Add(i32),
            Clear,
            Size,
            Empty,
            Snapshot,
        }
        let len = || colls(|v| match &v[id] {
            Coll::List(l) => l.len(),
            _ => 0,
        });
        let act = match name {
            "add" => Act::Add(a.as_i32()),
            "get" => {
                let i = a.as_i32();
                if i < 0 || i as usize >= len() {
                    return Err(self.throw("java/lang/IndexOutOfBoundsException"));
                }
                Act::Get(i as usize)
            }
            "size" => Act::Size,
            "isEmpty" => Act::Empty,
            "contains" => Act::Contains(a.as_i32()),
            "indexOf" => Act::IndexOf(a.as_i32()),
            "remove" => {
                let i = a.as_i32();
                if i < 0 || i as usize >= len() {
                    return Err(self.throw("java/lang/IndexOutOfBoundsException"));
                }
                Act::Remove(i as usize)
            }
            "clear" => Act::Clear,
            "toArray" | "iterator" | "toString" => Act::Snapshot,
            other => return Err(VmError { msg: format!("{cls} 没有方法 {other}"), exc: None }),
        };
        match act {
            Act::Add(v) => {
                colls(|c| {
                    if let Coll::List(l) = &mut c[id] {
                        l.push(v);
                    }
                });
                Ok(Slot::I(1))
            }
            Act::Get(i) => Ok(colls(|c| match &c[id] {
                Coll::List(l) => Slot::I(l[i]),
                _ => Slot::I(0),
            })),
            Act::Size => Ok(Slot::I(len() as i32)),
            Act::Empty => Ok(Slot::I(if len() == 0 { 1 } else { 0 })),
            Act::Contains(v) | Act::IndexOf(v) => {
                let list = colls(|c| match &c[id] {
                    Coll::List(l) => l.clone(),
                    _ => Vec::new(),
                });
                let pos = list.iter().position(|&x| self.refs_equal(x, v));
                Ok(if name == "contains" {
                    Slot::I(if pos.is_some() { 1 } else { 0 })
                } else {
                    Slot::I(pos.map(|p| p as i32).unwrap_or(-1))
                })
            }
            Act::Remove(i) => {
                let old = colls(|c| match &mut c[id] {
                    Coll::List(l) => Some(l.remove(i)),
                    _ => None,
                });
                Ok(Slot::I(old.unwrap_or(0)))
            }
            Act::Clear => {
                colls(|c| {
                    if let Coll::List(l) = &mut c[id] {
                        l.clear();
                    }
                });
                Ok(Slot::I(0))
            }
            Act::Snapshot => {
                let list = colls(|c| match &c[id] {
                    Coll::List(l) => l.clone(),
                    _ => Vec::new(),
                });
                match name {
                    "toString" => {
                        let parts: Vec<String> = list.iter().map(|&h| self.obj_to_string(h)).collect();
                        let t = format!("[{}]", parts.join(", "));
                        Ok(Slot::I(self.str_new(t)))
                    }
                    "toArray" => {
                        let elems: Vec<Slot> = list.iter().map(|&h| Slot::I(h)).collect();
                        self.arrays.push(elems);
                        self.array_class.push("[Ljava/lang/Object;".to_string());
                        Ok(Slot::I(-(self.arrays.len() as i32)))
                    }
                    _ => {
                        // iterator：新对象 + 独立游标
                        let it = self.new_plain("java/util/ArrayList$Itr", 1);
                        self.coll_attach(it, Coll::Iter(id, 0));
                        Ok(Slot::I(it))
                    }
                }
            }
        }
    }

    fn map_op(&mut self, name: &str, recv: i32, a: Slot, b: Slot) -> Result<Slot, VmError> {
        let id = self.coll_ensure(recv, || Coll::Map(Vec::new()));
        // 键的规范化在**进闭包之前**算好（闭包里拿不到 &self）
        let kraw = a.as_i32();
        let key = self.coll_key_of(kraw);
        match name {
            "put" => {
                let v = b.as_i32();
                let old = colls(|c| {
                    let Coll::Map(m) = &mut c[id] else { return None };
                    match m.iter_mut().find(|e| e.0 == key) {
                        Some(slot) => Some(std::mem::replace(&mut slot.2, v)),
                        None => {
                            m.push((key, kraw, v));
                            None
                        }
                    }
                });
                Ok(Slot::I(old.unwrap_or(0)))
            }
            "get" => {
                let found = colls(|c| match &c[id] {
                    Coll::Map(m) => m.iter().find(|e| e.0 == key).map(|e| e.2),
                    _ => None,
                });
                Ok(Slot::I(found.unwrap_or(0)))
            }
            "containsKey" => {
                let has = colls(|c| match &c[id] {
                    Coll::Map(m) => m.iter().any(|e| e.0 == key),
                    _ => false,
                });
                Ok(Slot::I(if has { 1 } else { 0 }))
            }
            "remove" => {
                let old = colls(|c| match &mut c[id] {
                    Coll::Map(m) => m.iter().position(|e| e.0 == key).map(|p| m.remove(p).2),
                    _ => None,
                });
                Ok(Slot::I(old.unwrap_or(0)))
            }
            "size" => Ok(Slot::I(colls(|c| match &c[id] {
                Coll::Map(m) => m.len() as i32,
                _ => 0,
            }))),
            "isEmpty" => Ok(Slot::I(
                if colls(|c| match &c[id] {
                    Coll::Map(m) => m.len(),
                    _ => 0,
                }) == 0
                {
                    1
                } else {
                    0
                },
            )),
            "clear" => {
                colls(|c| {
                    if let Coll::Map(m) = &mut c[id] {
                        m.clear();
                    }
                });
                Ok(Slot::I(0))
            }
            "keySet" | "values" | "entrySet" | "toString" => {
                let (keys, vals): (Vec<i32>, Vec<i32>) = colls(|c| match &c[id] {
                    Coll::Map(m) => (m.iter().map(|e| e.1).collect(), m.iter().map(|e| e.2).collect()),
                    _ => (Vec::new(), Vec::new()),
                });
                match name {
                    "toString" => {
                        let mut parts = Vec::with_capacity(keys.len());
                        for i in 0..keys.len() {
                            parts.push(format!(
                                "{}={}",
                                self.obj_to_string(keys[i]),
                                self.obj_to_string(vals[i])
                            ));
                        }
                        Ok(Slot::I(self.str_new(format!("{{{}}}", parts.join(", ")))))
                    }
                    "entrySet" => {
                        let mut entries = Vec::with_capacity(keys.len());
                        for i in 0..keys.len() {
                            let e = self.new_plain("java/util/Map$Entry", 2);
                            if let Some(f) = self.objects.get_mut(e as usize - 1) {
                                f[0] = Slot::I(keys[i]);
                                f[1] = Slot::I(vals[i]);
                            }
                            entries.push(e);
                        }
                        let view = self.new_plain("java/util/Set", 1);
                        self.coll_attach(view, Coll::List(entries));
                        Ok(Slot::I(view))
                    }
                    _ => {
                        let items = if name == "keySet" { keys } else { vals };
                        let cls = if name == "keySet" { "java/util/Set" } else { "java/util/Collection" };
                        let view = self.new_plain(cls, 1);
                        self.coll_attach(view, Coll::List(items));
                        Ok(Slot::I(view))
                    }
                }
            }
            other => Err(VmError { msg: format!("Map 没有方法 {other}"), exc: None }),
        }
    }

    /// 键的规范化形式：String 用内容、包装类用值、其余用句柄 —— 让线性扫描的
    /// 「键相等」与 Java 的 `equals` 一致
    fn coll_key_of(&self, h: i32) -> Key {
        match self.ref_kind(h) {
            RefKind::Null => (0, String::new()),
            RefKind::Str => (1, self.string_text(h)),
            RefKind::Obj(_) => match self.wrapper_value(h) {
                Some(v) => (2, format!("{:?}", v)),
                None => (3, h.to_string()),
            },
            RefKind::Arr(_) => (3, h.to_string()),
        }
    }

    /// `java.util.Iterator` 的 `hasNext` / `next`。
    ///
    /// **这两个方法不能进 `jdk::VIRTUALS` 表**：表一登，JIT 的 `builtin_vop` 就会把
    /// 用户自己 `implements Iterable` 的 for-each 一起截胡。所以只在接收者确实
    /// 是我们的迭代器对象时才认。
    fn iter_op(&mut self, name: &str, recv: i32) -> Option<Result<Slot, VmError>> {
        let id = self.coll_slot(recv)?;
        let src = colls(|c| match c.get(id) {
            Some(Coll::Iter(src, _)) => *src,
            _ => usize::MAX,
        });
        if src == usize::MAX {
            return None;
        }
        Some(match name {
            "hasNext" => {
                let (pos, len) = colls(|c| match (&c[src], &c[id]) {
                    (Coll::List(l), Coll::Iter(_, p)) => (*p, l.len()),
                    (Coll::Map(m), Coll::Iter(_, p)) => (*p, m.len()),
                    _ => (0, 0),
                });
                Ok(Slot::I(if pos < len { 1 } else { 0 }))
            }
            "next" => {
                let out = colls(|c| {
                    let Coll::Iter(_, p) = c[id] else { return None };
                    let item = match &c[src] {
                        Coll::List(l) => l.get(p).copied(),
                        Coll::Map(m) => m.get(p).map(|e| e.1),
                        _ => None,
                    };
                    if let Coll::Iter(_, ref mut pos) = c[id] {
                        *pos = p + 1;
                    }
                    item
                });
                match out {
                    Some(h) => Ok(Slot::I(h)),
                    None => Err(self.throw("java/util/NoSuchElementException")),
                }
            }
            other => return Some(Err(VmError { msg: format!("Iterator 没有方法 {other}"), exc: None })),
        })
    }

    /// `Throwable` 的 `getMessage` / `toString` / `printStackTrace`
    fn throwable_op(&mut self, name: &str, recv: i32) -> Result<Slot, VmError> {
        let msg = self.exc_msgs.get(&recv).copied().unwrap_or(0);
        let text = if msg != 0 { self.obj_to_string(msg) } else { String::new() };
        match name {
            "getMessage" => Ok(Slot::I(msg)),
            "toString" => {
                let cls = match self.ref_kind(recv) {
                    RefKind::Obj(c) => c.replace('/', "."),
                    _ => "java.lang.Throwable".to_string(),
                };
                // HotSpot 的规则：detailMessage == null 才只打类名（空串仍要打 ": "）
                let t = if msg == 0 {
                    cls
                } else {
                    format!("{cls}: {text}")
                };
                Ok(Slot::I(self.str_new(t)))
            }
            "printStackTrace" => {
                // 栈帧拿不到行号表，按「类.方法」打一层；输出走 stderr，不参与
                // stdout 的三方比对（HotSpot 的行号本来也无法逐字复现）
                let cls = match self.ref_kind(recv) {
                    RefKind::Obj(c) => c.replace('/', "."),
                    _ => "java.lang.Throwable".into(),
                };
                if msg == 0 {
                    eprintln!("{cls}");
                } else {
                    eprintln!("{cls}: {text}");
                }
                Ok(Slot::I(0))
            }
            other => Err(VmError { msg: format!("Throwable 没有方法 {other}"), exc: None }),
        }
    }

    /// 虚方法里「新增 API」的分派；`None` = 不归我管
    fn api_virtual_op(&mut self, cls: &str, name: &str, recv: i32, a: Slot, b: Slot) -> Option<Result<Slot, VmError>> {
        if is_coll_class(cls) {
            return Some(self.coll_op(cls, name, recv, a, b));
        }
        if is_throwable_chain(cls) {
            return Some(self.throwable_op(name, recv));
        }
        None
    }
}

/// 规范化键：(种类, 文本)。种类 0=null 1=String 2=包装类 3=其它（按句柄）
type Key = (u8, String);

/// `Math.random()` 的固定种子 LCG 状态（序列与 HotSpot 不同，**不能**进三路一致的样例）
thread_local! {
    static RNG: std::cell::Cell<u64> = const { std::cell::Cell::new(0x2545_F491_4F6C_DD1D) };
}

fn lcg_next() -> f64 {
    RNG.with(|s| {
        // Numerical Recipes 的 LCG-64（周期够长、平台无关）
        let v = s.get().wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        s.set(v);
        ((v >> 11) as f64) / ((1u64 << 53) as f64)
    })
}

impl Vm {
    /// `String.valueOf` 的文本（`desc` 是**完整**描述符，按形参部分分流）
    fn value_of_text(&self, desc: &str, v: Slot) -> String {
        if desc.starts_with("(Ljava/lang/Object;)") {
            return self.obj_to_string(v.as_i32());
        }
        if desc.starts_with("(Z)") {
            return (if v.as_i32() != 0 { "true" } else { "false" }).to_string();
        }
        if desc.starts_with("(C)") {
            return char::from_u32(v.as_i32() as u32).unwrap_or('\0').to_string();
        }
        if desc.starts_with("(I)") {
            return v.as_i32().to_string();
        }
        if desc.starts_with("(J)") {
            return v.as_i64().to_string();
        }
        if desc.starts_with("(F)") {
            return crate::java_float_to_string(v.as_f32());
        }
        if desc.starts_with("(D)") {
            return crate::java_double_to_string(v.as_f64());
        }
        if desc.starts_with("([C)") {
            let h = v.as_i32();
            if h >= 0 {
                return String::new();
            }
            return self
                .arrays
                .get((-h - 1) as usize)
                .map(|a| a.iter().map(|x| char::from_u32(x.as_i32() as u32).unwrap_or('\0')).collect())
                .unwrap_or_default();
        }
        String::new()
    }

    /// 格式化实参 → 数值（`%d` 收到的是包装类句柄，要拆箱）
    fn fmt_i64(&self, h: i32) -> i64 {
        match self.ref_kind(h) {
            RefKind::Obj(_) => self.wrapper_value(h).map(|v| v.as_i64()).unwrap_or(0),
            RefKind::Str => 0,
            _ => h as i64,
        }
    }

    fn fmt_f64(&self, h: i32) -> f64 {
        match self.ref_kind(h) {
            RefKind::Obj(_) => self.wrapper_value(h).map(|v| v.as_f64()).unwrap_or(0.0),
            RefKind::Str => 0.0,
            _ => h as f64,
        }
    }

    fn fmt_char(&self, h: i32) -> char {
        let v = match self.ref_kind(h) {
            RefKind::Obj(_) => self.wrapper_value(h).map(|v| v.as_i32()).unwrap_or(0),
            _ => h,
        };
        char::from_u32(v as u32).unwrap_or('\u{fffd}')
    }

    /// `String.format` 的格式化引擎：`%d %s %f %c %x %o %b %% %n` + 宽度/精度/`-`/`0`/`+`
    fn java_format(&mut self, fmt: &str, argv: &[Slot]) -> String {
        let cs: Vec<char> = fmt.chars().collect();
        let mut out = String::new();
        let mut ai = 0usize;
        let mut i = 0usize;
        while i < cs.len() {
            let c = cs[i];
            i += 1;
            if c != '%' {
                out.push(c);
                continue;
            }
            if i >= cs.len() {
                out.push('%');
                break;
            }
            let (mut left, mut zero, mut plus) = (false, false, false);
            while i < cs.len() {
                match cs[i] {
                    '-' => left = true,
                    '0' => zero = true,
                    '+' => plus = true,
                    ' ' | ',' | '(' | '#' => {}
                    _ => break,
                }
                i += 1;
            }
            let mut width = 0usize;
            while i < cs.len() && cs[i].is_ascii_digit() {
                width = width * 10 + (cs[i] as u8 - b'0') as usize;
                i += 1;
            }
            let mut prec: Option<usize> = None;
            if i < cs.len() && cs[i] == '.' {
                i += 1;
                let mut p = 0usize;
                while i < cs.len() && cs[i].is_ascii_digit() {
                    p = p * 10 + (cs[i] as u8 - b'0') as usize;
                    i += 1;
                }
                prec = Some(p);
            }
            let conv = if i < cs.len() { cs[i] } else { ' ' };
            if i < cs.len() {
                i += 1;
            }
            if conv == '%' {
                out.push('%');
                continue;
            }
            if conv == 'n' {
                out.push('\n');
                continue;
            }
            let arg = argv.get(ai).copied().unwrap_or(Slot::I(0)).as_i32();
            ai += 1;
            let raw = match conv {
                'd' => {
                    let v = self.fmt_i64(arg);
                    if plus && v >= 0 {
                        format!("+{v}")
                    } else {
                        v.to_string()
                    }
                }
                'x' => format!("{:x}", self.fmt_i64(arg)),
                'X' => format!("{:X}", self.fmt_i64(arg)),
                'o' => format!("{:o}", self.fmt_i64(arg)),
                'f' => format!("{:.*}", prec.unwrap_or(6), self.fmt_f64(arg)),
                's' | 'S' => {
                    let t = self.obj_to_string(arg);
                    if prec.is_some() {
                        t.chars().take(prec.unwrap_or(0)).collect()
                    } else {
                        t
                    }
                }
                'c' => self.fmt_char(arg).to_string(),
                'b' => match self.ref_kind(arg) {
                    RefKind::Null => "false".to_string(),
                    RefKind::Obj(_) => match self.wrapper_value(arg) {
                        Some(v) => (if v.as_i32() != 0 { "true" } else { "false" }).to_string(),
                        None => "true".to_string(),
                    },
                    _ => "true".to_string(),
                },
                other => other.to_string(),
            };
            out.push_str(&format_pad(raw, width, left, zero));
        }
        out
    }

    /// 任意数组 → `Arrays.toString` 文本（按**运行时数组类型**分派，
    /// 这样 `[I`/`[J`/`[D`/`[Z`/`[C`/`[LL` 各重载共用一个实现）
    fn array_text_any(&self, h: i32) -> Result<String, VmError> {
        if h == 0 {
            return Ok("null".to_string());
        }
        let RefKind::Arr(d) = self.ref_kind(h) else {
            return Ok("null".to_string());
        };
        let arr = &self.arrays[(-h - 1) as usize];
        let parts: Vec<String> = match d.as_str() {
            "[J" => arr.iter().map(|v| v.as_i64().to_string()).collect(),
            "[D" => arr.iter().map(|v| crate::java_double_to_string(v.as_f64())).collect(),
            "[F" => arr.iter().map(|v| crate::java_float_to_string(v.as_f32())).collect(),
            "[Z" => arr.iter().map(|v| (if v.as_i32() != 0 { "true" } else { "false" }).to_string()).collect(),
            "[C" => arr
                .iter()
                .map(|v| char::from_u32(v.as_i32() as u32).unwrap_or('\0').to_string())
                .collect(),
            "[B" => arr.iter().map(|v| (v.as_i32() as i8).to_string()).collect(),
            "[S" => arr.iter().map(|v| v.as_i32().to_string()).collect(),
            _ if d.starts_with("[L") || d.starts_with("[[") => {
                arr.iter().map(|v| self.obj_to_string(v.as_i32())).collect()
            }
            _ => arr.iter().map(|v| v.as_i32().to_string()).collect(),
        };
        Ok(format!("[{}]", parts.join(", ")))
    }

    /// 追加的静态 API：`String.valueOf`/`format`、`Math`、`Integer`/`Long` 进制、
    /// `Character`、`Arrays`。`None` = 不归我管（落到原有分派）
    fn static_api_op(
        &mut self,
        class: &str,
        name: &str,
        desc: &str,
        args: &[Slot],
    ) -> Option<Result<Slot, VmError>> {
        let a = args.first().copied().unwrap_or(Slot::I(0));
        let b = args.get(1).copied().unwrap_or(Slot::I(0));
        let out = match (class, name) {
            ("java/lang/String", "valueOf") => Ok(Slot::I(self.str_new(self.value_of_text(desc, a)))),
            ("java/lang/String", "format") => {
                let fmt = self.str_of(a.as_i32());
                let hv = b.as_i32();
                let argv: Vec<Slot> = if hv < 0 {
                    self.arrays.get((-hv - 1) as usize).cloned().unwrap_or_default()
                } else {
                    Vec::new()
                };
                let t = self.java_format(&fmt, &argv);
                Ok(Slot::I(self.str_new(t)))
            }
            // ---- Math ----
            ("java/lang/Math", "abs") => Ok(match desc {
                "(I)I" => Slot::I(a.as_i32().wrapping_abs()),
                "(J)J" => Slot::L(a.as_i64().wrapping_abs()),
                "(F)F" => Slot::Fl(a.as_f32().abs()),
                _ => Slot::F(a.as_f64().abs()),
            }),
            ("java/lang/Math", "max") => Ok(match desc {
                "(II)I" => Slot::I(a.as_i32().max(b.as_i32())),
                "(JJ)J" => Slot::L(a.as_i64().max(b.as_i64())),
                _ => Slot::F(a.as_f64().max(b.as_f64())),
            }),
            ("java/lang/Math", "min") => Ok(match desc {
                "(II)I" => Slot::I(a.as_i32().min(b.as_i32())),
                "(JJ)J" => Slot::L(a.as_i64().min(b.as_i64())),
                _ => Slot::F(a.as_f64().min(b.as_f64())),
            }),
            ("java/lang/Math", "sqrt") => Ok(Slot::F(a.as_f64().sqrt())),
            ("java/lang/Math", "pow") => Ok(Slot::F(a.as_f64().powf(b.as_f64()))),
            ("java/lang/Math", "floor") => Ok(Slot::F(a.as_f64().floor())),
            ("java/lang/Math", "ceil") => Ok(Slot::F(a.as_f64().ceil())),
            ("java/lang/Math", "signum") => Ok(Slot::F(if a.as_f64() > 0.0 {
                1.0
            } else if a.as_f64() < 0.0 {
                -1.0
            } else {
                a.as_f64()
            })),
            // Java 的 round 是 floor(x + 0.5)，与 Rust 的 round（远离零）不同
            ("java/lang/Math", "round") => Ok(match desc {
                "(F)I" => Slot::I((a.as_f32() + 0.5).floor() as i32),
                _ => Slot::L((a.as_f64() + 0.5).floor() as i64),
            }),
            ("java/lang/Math", "random") => Ok(Slot::F(lcg_next())),
            // ---- Integer / Long ----
            ("java/lang/Integer", "toHexString") => Ok(Slot::I(self.str_new(format!("{:x}", a.as_i32() as u32)))),
            ("java/lang/Integer", "toBinaryString") => {
                Ok(Slot::I(self.str_new(format!("{:b}", a.as_i32() as u32))))
            }
            ("java/lang/Integer", "toOctalString") => Ok(Slot::I(self.str_new(format!("{:o}", a.as_i32() as u32)))),
            ("java/lang/Integer", "compare") => Ok(Slot::I(a.as_i32().cmp(&b.as_i32()) as i32)),
            ("java/lang/Long", "toHexString") => Ok(Slot::I(self.str_new(format!("{:x}", a.as_i64() as u64)))),
            ("java/lang/Long", "compare") => Ok(Slot::I(a.as_i64().cmp(&b.as_i64()) as i32)),
            // ---- Character（ASCII 范围与 HotSpot 一致）----
            ("java/lang/Character", "isDigit") => {
                Ok(Slot::I(if (a.as_i32() as u32).try_into().map(|c: char| c.is_ascii_digit()).unwrap_or(false) {
                    1
                } else {
                    0
                }))
            }
            ("java/lang/Character", "isLetter") => {
                Ok(Slot::I(if (a.as_i32() as u32).try_into().map(|c: char| c.is_alphabetic()).unwrap_or(false) {
                    1
                } else {
                    0
                }))
            }
            ("java/lang/Character", "isLetterOrDigit") => Ok(Slot::I(
                if (a.as_i32() as u32)
                    .try_into()
                    .map(|c: char| c.is_alphanumeric())
                    .unwrap_or(false)
                {
                    1
                } else {
                    0
                },
            )),
            ("java/lang/Character", "isUpperCase") => Ok(Slot::I(
                if (a.as_i32() as u32).try_into().map(|c: char| c.is_uppercase()).unwrap_or(false) {
                    1
                } else {
                    0
                },
            )),
            ("java/lang/Character", "isLowerCase") => Ok(Slot::I(
                if (a.as_i32() as u32).try_into().map(|c: char| c.is_lowercase()).unwrap_or(false) {
                    1
                } else {
                    0
                },
            )),
            ("java/lang/Character", "isWhitespace") => Ok(Slot::I(
                if char::from_u32(a.as_i32() as u32).map(|c| c.is_whitespace()).unwrap_or(false) {
                    1
                } else {
                    0
                },
            )),
            ("java/lang/Character", "toUpperCase") => Ok(Slot::I(
                char::from_u32(a.as_i32() as u32)
                    .and_then(|c| c.to_uppercase().next())
                    .map(|c| c as i32)
                    .unwrap_or(a.as_i32()),
            )),
            ("java/lang/Character", "toLowerCase") => Ok(Slot::I(
                char::from_u32(a.as_i32() as u32)
                    .and_then(|c| c.to_lowercase().next())
                    .map(|c| c as i32)
                    .unwrap_or(a.as_i32()),
            )),
            ("java/lang/Character", "getNumericValue") => Ok(Slot::I(match char::from_u32(a.as_i32() as u32) {
                Some(c) if c.is_ascii_digit() => c as i32 - '0' as i32,
                Some(c) if c.is_ascii_alphabetic() => c.to_ascii_lowercase() as i32 - 'a' as i32 + 10,
                _ => -1,
            })),
            // ---- Arrays ----
            ("java/util/Arrays", "toString") => match self.array_text_any(a.as_i32()) {
                Ok(t) => Ok(Slot::I(self.str_new(t))),
                Err(e) => Err(e),
            },
            ("java/util/Arrays", "equals") => {
                let (x, y) = (a.as_i32(), b.as_i32());
                let same = if x == y {
                    true
                } else if x == 0 || y == 0 {
                    false
                } else {
                    let ax = self.arrays.get((-x - 1) as usize).cloned().unwrap_or_default();
                    let ay = self.arrays.get((-y - 1) as usize).cloned().unwrap_or_default();
                    ax == ay
                };
                Ok(Slot::I(if same { 1 } else { 0 }))
            }
            ("java/util/Arrays", "fill") => {
                let h = a.as_i32();
                if h < 0 {
                    let v = b;
                    if let Some(arr) = self.arrays.get_mut((-h - 1) as usize) {
                        for e in arr.iter_mut() {
                            *e = v;
                        }
                    }
                }
                Ok(Slot::I(0))
            }
            ("java/util/Arrays", "sort") => {
                let h = a.as_i32();
                if h < 0 {
                    let d = self.array_class.get((-h - 1) as usize).cloned().unwrap_or_default();
                    // 引用元素要先按 `&self` 比完序（排序闭包拿不到 self），再写回
                    if matches!(d.as_str(), "[J" | "[D" | "[I") {
                        if let Some(arr) = self.arrays.get_mut((-h - 1) as usize) {
                            match d.as_str() {
                                "[J" => arr.sort_by_key(|v| v.as_i64()),
                                "[D" => arr.sort_by(|x, y| {
                                    x.as_f64().partial_cmp(&y.as_f64()).unwrap_or(std::cmp::Ordering::Equal)
                                }),
                                _ => arr.sort_by_key(|v| v.as_i32()),
                            }
                        }
                    } else {
                        let mut refs: Vec<i32> = self
                            .arrays
                            .get((-h - 1) as usize)
                            .map(|a| a.iter().map(|v| v.as_i32()).collect())
                            .unwrap_or_default();
                        refs.sort_by(|&x, &y| self.refs_cmp(x, y));
                        if let Some(arr) = self.arrays.get_mut((-h - 1) as usize) {
                            for (i, r) in refs.into_iter().enumerate() {
                                if i < arr.len() {
                                    arr[i] = Slot::I(r);
                                }
                            }
                        }
                    }
                }
                Ok(Slot::I(0))
            }
            _ => return None,
        };
        Some(out)
    }
}

/// 宽度 / 对齐 / 补零
fn format_pad(text: String, width: usize, left: bool, zero: bool) -> String {
    let n = text.chars().count();
    if n >= width {
        return text;
    }
    let fill = width - n;
    if left {
        let mut s = text;
        s.push_str(&" ".repeat(fill));
        s
    } else if zero {
        // 补零要留在符号之后（`%05d` 的 -42 → -0042）
        let (sign, body) = if let Some(rest) = text.strip_prefix('-') {
            ("-", rest.to_string())
        } else if let Some(rest) = text.strip_prefix('+') {
            ("+", rest.to_string())
        } else {
            ("", text)
        };
        format!("{sign}{}{body}", "0".repeat(fill))
    } else {
        format!("{}{text}", " ".repeat(fill))
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
