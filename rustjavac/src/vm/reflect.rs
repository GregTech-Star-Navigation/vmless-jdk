//! 反射层：`java.lang.Class` 的成员查询 + `Field` / `Method` / `Constructor` +
//! 运行期注解 + 泛型签名。
//!
//! # 为什么是「VM 接管」而不是跑 java.base 的反射字节码
//!
//! 真 HotSpot 的反射由 VM 提供：`Class.getDeclaredMethods0` 是 native，`Field.get`
//! 走的是 `jdk.internal.reflect.UnsafeFieldAccessorImpl` 那一整套，而 `Method.invoke`
//! 背后的 `MethodAccessorGenerator` **运行期生成字节码**。我们的堆是「句柄 → 槽数组」，
//! 没有内存偏移，也没有运行期字节码生成，所以这条路不可能照抄。
//!
//! 这里的做法与 HotSpot 的分工等价：**语义在 VM 里**（这个文件），java.lang.reflect 的
//! 类只当**载体** —— 它们的字段（`clazz` / `slot` / `name` / `type` / `modifiers` /
//! `signature` / `override`）照常填好，于是 `getName()` / `getModifiers()` / `getType()` /
//! `equals` / `toString` / `Modifier.toString` 这些纯字节码方法**一行都不用写**。
//!
//! 于是本文件只有两类入口：
//!
//! - **native**（OpenJDK 里本来就是 `native` 的）：`forName0`、`getDeclared*0`、
//!   `getDeclaringClass0`、`getSimpleBinaryName0`、`getInterfaces0`；
//! - **override**（OpenJDK 里是字节码、但语义必须由 VM 给的）：`getDeclaredFields/Methods/
//!   Constructors` 一族、`Field.get/set` 一族、`Method.invoke`、`Constructor.newInstance`、
//!   `getAnnotation` 一族（`Proxy` 需要运行期生成字节码，做不到）。
//!
//! 注解代理对象也不是 `java.lang.reflect.Proxy`：它是一批「运行时类 = 注解接口、
//! 成员值放在 `Vm::ann_objs` 里」的对象，成员方法的调用在 `ann_dispatch` 里按名字查表
//! 返回 —— 与 lambda 对象用的是同一类手法（见 `indy.rs::LambdaInfo`）。

use std::rc::Rc;

use crate::classfile::annot::{self, Ann, AnnVal};
use crate::{ClassFile, CpEntry, Member};

use super::err;
use super::frame::Frame;
use super::heap::RefKind;
use super::slot::Slot;
use super::Vm;
use super::VmError;

/// 访问标志（JVM 的那一套位）
pub(super) const ACC_PUBLIC: u16 = 0x0001;
pub(super) const ACC_PRIVATE: u16 = 0x0002;
pub(super) const ACC_PROTECTED: u16 = 0x0004;
pub(super) const ACC_STATIC: u16 = 0x0008;
pub(super) const ACC_FINAL: u16 = 0x0010;
pub(super) const ACC_INTERFACE: u16 = 0x0200;
pub(super) const ACC_ABSTRACT: u16 = 0x0400;
pub(super) const ACC_SYNTHETIC: u16 = 0x1000;
pub(super) const ACC_ANNOTATION: u16 = 0x2000;
pub(super) const ACC_ENUM: u16 = 0x4000;

/// 一个注解实例（`java.lang.annotation.Annotation` 的替身）
#[derive(Clone)]
pub(super) struct AnnInst {
    /// 注解类型的内部名，如 `P0$Anno`
    pub ty: String,
    /// 成员名 → 值（**默认值在前**，显式值覆盖或追加 —— 与 `AnnotationParser` 的
    /// `LinkedHashMap(memberDefaults)` 一致，`toString` 的顺序靠它）
    pub vals: Vec<(String, AnnVal)>,
}

// ------------------------------------------------------------------ 小工具

fn name_of(cf: &ClassFile, m: &Member) -> String {
    String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default()).into_owned()
}

fn desc_of(cf: &ClassFile, m: &Member) -> String {
    String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default()).into_owned()
}

/// 字段描述符 → 类对象的键：`Ljava/lang/String;` → `java/lang/String`，其余原样
pub(super) fn internal_of_desc(desc: &str) -> String {
    if desc.len() >= 2 && desc.starts_with('L') && desc.ends_with(';') {
        desc[1..desc.len() - 1].to_string()
    } else {
        desc.to_string()
    }
}

/// 方法描述符 → (参数描述符, 返回描述符)
pub(super) fn split_desc(desc: &str) -> (Vec<String>, String) {
    let b = desc.as_bytes();
    let mut i = 1usize;
    let mut params = Vec::new();
    while i < b.len() && b[i] != b')' {
        let start = i;
        while i < b.len() && b[i] == b'[' {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        match b[i] {
            b'L' => {
                while i < b.len() && b[i] != b';' {
                    i += 1;
                }
                i += 1;
            }
            _ => i += 1,
        }
        params.push(desc[start..i.min(desc.len())].to_string());
    }
    let ret = if i + 1 <= desc.len() { desc[i + 1..].to_string() } else { String::new() };
    (params, ret)
}

/// 基本类型描述符（`I`/`J`/…），引用类型返回 None
fn prim_of(desc: &str) -> Option<char> {
    match desc {
        "Z" => Some('Z'),
        "B" => Some('B'),
        "C" => Some('C'),
        "S" => Some('S'),
        "I" => Some('I'),
        "J" => Some('J'),
        "F" => Some('F'),
        "D" => Some('D'),
        "V" => Some('V'),
        _ => None,
    }
}

/// 基本类型描述符 → 包装类内部名
fn box_of(desc: char) -> Option<&'static str> {
    match desc {
        'Z' => Some("java/lang/Boolean"),
        'B' => Some("java/lang/Byte"),
        'C' => Some("java/lang/Character"),
        'S' => Some("java/lang/Short"),
        'I' => Some("java/lang/Integer"),
        'J' => Some("java/lang/Long"),
        'F' => Some("java/lang/Float"),
        'D' => Some("java/lang/Double"),
        _ => None,
    }
}

/// 顶层类名（内部类截到第一个 `$` 之前）—— javac 的**嵌套宿主**（nest host）规则
fn nest_prefix(name: &str) -> &str {
    match name.find('$') {
        Some(i) => &name[..i],
        None => name,
    }
}

/// 类文件里一个成员的定位（下标 + 元数据）
#[derive(Clone)]
pub(super) struct MemAt {
    pub name: String,
    pub desc: String,
    pub flags: u16,
    pub idx: usize,
}

fn declared_fields(cf: &ClassFile) -> Vec<MemAt> {
    cf.fields
        .iter()
        .enumerate()
        .map(|(i, m)| MemAt { name: name_of(cf, m), desc: desc_of(cf, m), flags: m.access_flags, idx: i })
        .collect()
}

/// 类声明的方法（**不含** `<clinit>`；`<init>` 走 `declared_ctors`）
fn declared_methods(cf: &ClassFile) -> Vec<MemAt> {
    cf.methods
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            let name = name_of(cf, m);
            if name == "<clinit>" || name == "<init>" {
                return None;
            }
            Some(MemAt { name, desc: desc_of(cf, m), flags: m.access_flags, idx: i })
        })
        .collect()
}

fn declared_ctors(cf: &ClassFile) -> Vec<MemAt> {
    cf.methods
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            let name = name_of(cf, m);
            if name != "<init>" {
                return None;
            }
            Some(MemAt { name, desc: desc_of(cf, m), flags: m.access_flags, idx: i })
        })
        .collect()
}

/// 实例字段总数（含继承）里，`class` 声明的第 `ordinal` 个非静态字段的槽下标
///
/// 必须与 `ClassStore::build_metas` 的布局一致：**父类字段在前、本类在后**。
fn field_slot_index(vm: &Vm, class: &str, ordinal: usize) -> Option<usize> {
    let mut n = 0usize;
    let chain = vm.store.chain(class);
    // `chain` 是 [本类, 父类, …, Object]：**父类在前**，所以从 skip(1) 开始累加
    for c in chain.iter().skip(1) {
        if let Some(cf) = vm.store.class_file(c) {
            n += cf.fields.iter().filter(|f| f.access_flags & ACC_STATIC == 0).count();
        }
    }
    // 本类里第 ordinal 个非静态字段：ordinal 是字段在 **cf.fields** 里的下标，要换算成
    // 「第几个非静态字段」
    let cf = vm.store.class_file(class)?;
    let mut seen = 0usize;
    for (i, f) in cf.fields.iter().enumerate() {
        if f.access_flags & ACC_STATIC != 0 {
            continue;
        }
        if i == ordinal {
            return Some(n + seen);
        }
        seen += 1;
    }
    None
}

// ------------------------------------------------------------------ 注解值 → Java 对象

impl Vm {
    /// 注解成员值 → 那个成员的**返回类型**对应的 Java 对象
    ///
    /// `want` 是该成员的返回类型描述符（数组要知道元素类型才建得对）。
    fn ann_val_to_obj(&mut self, v: &AnnVal, want: Option<&str>) -> Result<i32, VmError> {
        Ok(match v {
            AnnVal::Bool(b) => self.new_wrapper("java/lang/Boolean", Slot::I(i32::from(*b))),
            AnnVal::Byte(b) => self.new_wrapper("java/lang/Byte", Slot::I(*b as i32)),
            AnnVal::Char(c) => self.new_wrapper("java/lang/Character", Slot::I(*c as i32)),
            AnnVal::Short(s) => self.new_wrapper("java/lang/Short", Slot::I(*s as i32)),
            AnnVal::Int(i) => self.new_wrapper("java/lang/Integer", Slot::I(*i)),
            AnnVal::Long(l) => self.new_wrapper("java/lang/Long", Slot::L(*l)),
            AnnVal::Float(f) => self.new_wrapper("java/lang/Float", Slot::Fl(*f)),
            AnnVal::Double(d) => self.new_wrapper("java/lang/Double", Slot::F(*d)),
            AnnVal::Str(s) => self.str_new(s.clone()),
            AnnVal::Class(d) => {
                let key = internal_of_desc(d);
                self.class_obj(&key)
            }
            AnnVal::Enum(ty, name) => {
                let cls = internal_of_desc(ty);
                self.ensure_init(&cls)?;
                self.statics
                    .get(&(cls.clone(), name.clone()))
                    .copied()
                    .unwrap_or(Slot::I(0))
                    .as_i32()
            }
            AnnVal::Anno(a) => self.make_ann_obj(a)?,
            AnnVal::Array(items) => {
                // 元素类型从成员的返回类型剥一层 `[` 得到（`()[I` → `I`）
                let elem_desc = want
                    .and_then(|w| w.strip_prefix('['))
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "Ljava/lang/Object;".to_string());
                let mut slots = Vec::with_capacity(items.len());
                for it in items {
                    let h = self.ann_val_to_obj(it, Some(&elem_desc))?;
                    slots.push(Slot::I(h));
                }
                self.new_array(&format!("[{elem_desc}"), slots)
            }
        })
    }

    /// 新建一个数组（元素已经是槽）
    pub(super) fn new_array(&mut self, array_desc: &str, items: Vec<Slot>) -> i32 {
        self.arrays.push(items);
        self.array_class.push(array_desc.to_string());
        -(self.arrays.len() as i32)
    }

    /// 注解实例对象（分配 + 登记到 `ann_objs`，值表按「默认值在前」合并）
    fn make_ann_obj(&mut self, ann: &Ann) -> Result<i32, VmError> {
        let ty = internal_of_desc(&ann.ty);
        let mut vals: Vec<(String, AnnVal)> = Vec::new();
        if let Some(cf) = self.store.class_file(&ty) {
            for m in &cf.methods {
                if m.access_flags & ACC_STATIC != 0 {
                    continue;
                }
                if let Some(d) = annot::annotation_default(&cf, &m.attributes) {
                    vals.push((name_of(&cf, m), d));
                }
            }
        }
        // `AnnotationParser` 用 `new LinkedHashMap<>(type.memberDefaults())` 起手，而
        // `memberDefaults` 是 **HashMap** —— 所以默认值的初始顺序是 HashMap 的迭代顺序，
        // 不是方法声明顺序。`Annotation.toString` 直接按这个顺序打印，必须照着来。
        let order = hashmap_order(&vals.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>());
        let defaults: Vec<(String, AnnVal)> = order.into_iter().map(|i| vals[i].clone()).collect();
        let mut vals = defaults;
        for (k, v) in &ann.pairs {
            match vals.iter_mut().find(|(n, _)| n == k) {
                Some(e) => e.1 = v.clone(),
                None => vals.push((k.clone(), v.clone())),
            }
        }
        let n = self.store.field_count(&ty);
        self.objects.push(vec![Slot::I(0); n.max(1)]);
        self.object_class.push(ty.clone());
        let h = self.objects.len() as i32;
        self.ann_objs.insert(h, AnnInst { ty, vals });
        Ok(h)
    }

    /// 注解成员的取值（**没有**就 None，由调用点报 `IncompleteAnnotationException` 级别的问题）
    fn ann_member(&self, recv: i32, name: &str) -> Option<AnnVal> {
        self.ann_objs.get(&recv)?.vals.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone())
    }

    /// 注解对象的成员方法分派（`Annotation` 的语义在 VM 里，没有真正的 Proxy）
    pub(super) fn ann_dispatch(
        &mut self,
        recv: i32,
        name: &str,
        desc: &str,
    ) -> Option<Result<Slot, VmError>> {
        let inst = self.ann_objs.get(&recv)?.clone();
        let ty = inst.ty.clone();
        match (name, desc) {
            ("annotationType", "()Ljava/lang/Class;") => {
                Some(Ok(Slot::I(self.class_obj(&ty))))
            }
            ("toString", "()Ljava/lang/String;") => {
                let text = self.ann_to_string(&inst);
                Some(Ok(Slot::I(self.str_new(text))))
            }
            ("hashCode", "()I") => Some(Ok(Slot::I(self.ann_hash(&inst)))),
            ("equals", "(Ljava/lang/Object;)Z") => {
                // 与 JDK 一致：类型相同且每个成员值相等才算相等（本 VM 只比字面值）
                let other = self.ann_objs.get(&recv).map(|i| i.vals.clone());
                Some(Ok(Slot::I(i32::from(other == Some(inst.vals.clone())))))
            }
            _ => {
                let v = inst.vals.iter().find(|(n, _)| n == name)?.1.clone();
                let (_, ret) = split_desc(desc);
                match prim_of(&ret) {
                    // **基本类型的成员必须返回未装箱的值**：注解成员的返回类型可以是
                    // `int` / `boolean` / …，返回一个包装类句柄的话调用点会把它当
                    // 整数用（`@Q(n=5)` 打印成 50 那种垃圾值，踩过）
                    Some(p) => Some(Ok(ann_val_prim(&v, p))),
                    None => Some(self.ann_val_to_obj(&v, Some(&ret)).map(Slot::I)),
                }
            }
        }
    }

    /// `Annotation.toString()`：`@pkg.Anno(k=v, ...)`（单个 `value` 成员省略名字）
    fn ann_to_string(&self, inst: &AnnInst) -> String {
        let mut out = String::from("@");
        out.push_str(&self.ann_canonical(&inst.ty));
        out.push('(');
        let lone = inst.vals.len() == 1;
        for (i, (k, v)) in inst.vals.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            // `AnnotationInvocationHandler.toStringImpl`：只有一个 `value` 成员时省略名字
            if !(lone && k == "value") {
                out.push_str(k);
                out.push('=');
            }
            out.push_str(&ann_val_source(v));
        }
        out.push(')');
        out
    }

    fn ann_canonical(&self, ty: &str) -> String {
        ty.replace('/', ".").replace('$', ".")
    }
    /// `Annotation.hashCode()`：`sum((127 * 成员名.hashCode()) ^ 值.hashCode())`
    fn ann_hash(&self, inst: &AnnInst) -> i32 {
        let mut sum = 0i32;
        for (k, v) in &inst.vals {
            let nh = java_str_hash(k);
            sum = sum.wrapping_add(127i32.wrapping_mul(nh) ^ self.ann_val_hash(v));
        }
        sum
    }

    fn ann_val_hash(&self, v: &AnnVal) -> i32 {
        match v {
            AnnVal::Bool(b) => {
                if *b { 1231 } else { 1237 }
            }
            AnnVal::Byte(b) => i32::from(*b),
            AnnVal::Char(c) => *c as i32,
            AnnVal::Short(s) => i32::from(*s),
            AnnVal::Int(i) => *i,
            AnnVal::Long(l) => (*l ^ (*l >> 32)) as i32,
            AnnVal::Float(f) => {
                let bits = f.to_bits();
                if *f == 0.0 { 0 } else { bits as i32 }
            }
            AnnVal::Double(d) => {
                let bits = d.to_bits();
                let lo = bits as i32;
                let hi = (bits >> 32) as i32;
                (lo ^ hi) & 0xFFFF_FFFFu32 as i32
            }
            AnnVal::Str(s) => java_str_hash(s),
            AnnVal::Class(d) => java_str_hash(&internal_of_desc(d).replace('/', ".")),
            AnnVal::Enum(_, name) => java_str_hash(name),
            AnnVal::Anno(a) => self.ann_hash(&AnnInst { ty: internal_of_desc(&a.ty), vals: a.pairs.clone() }),
            AnnVal::Array(items) => {
                let mut h = 1i32;
                for x in items {
                    h = h.wrapping_mul(31).wrapping_add(self.ann_val_hash(x));
                }
                h
            }
        }
    }

    // -------------------------------------------------------------- 类元数据入口

    /// 类对象句柄 → 类描述符（数组是 `[I` 这种）
    pub(super) fn class_desc(&self, h: i32) -> Option<String> {
        self.class_obj_name(h)
    }

    /// 类描述符 → 已解析的 class 文件（数组与基本类型没有）
    fn cf_of(&self, desc: &str) -> Option<Rc<ClassFile>> {
        if desc.starts_with('[') || desc.len() == 1 {
            return None;
        }
        self.store.class_file(desc)
    }

    /// 描述符 → 类对象（`Ljava/lang/String;` / `I` / `[I` 都吃）
    pub(super) fn desc_class(&mut self, desc: &str) -> i32 {
        let key = internal_of_desc(desc);
        self.class_obj(&key)
    }

    /// 类对象 → 它的二进制名（`java.lang.String`），数组是 `[Ljava.lang.String;`
    fn class_binary_name(&self, desc: &str) -> String {
        super::heap::java_class_name(desc)
    }

    // -------------------------------------------------------------- Class 的成员查询

    /// `Class.getDeclaredFields0(publicOnly)`
    pub(super) fn refl_declared_fields(&mut self, class: &str, public_only: bool) -> Result<i32, VmError> {
        let mut items = Vec::new();
        if let Some(cf) = self.cf_of(class) {
            for f in declared_fields(&cf) {
                if public_only && f.flags & ACC_PUBLIC == 0 {
                    continue;
                }
                let h = self.make_field_obj(class, f.idx)?;
                items.push(Slot::I(h));
            }
        }
        Ok(self.new_array("[Ljava/lang/reflect/Field;", items))
    }

    /// `Class.getDeclaredMethods0(publicOnly)`
    pub(super) fn refl_declared_methods(&mut self, class: &str, public_only: bool) -> Result<i32, VmError> {
        let mut items = Vec::new();
        if let Some(cf) = self.cf_of(class) {
            for m in declared_methods(&cf) {
                if public_only && m.flags & ACC_PUBLIC == 0 {
                    continue;
                }
                let h = self.make_method_obj(class, m.idx)?;
                items.push(Slot::I(h));
            }
        }
        Ok(self.new_array("[Ljava/lang/reflect/Method;", items))
    }

    /// `Class.getDeclaredConstructors0(publicOnly)`
    pub(super) fn refl_declared_ctors(&mut self, class: &str, public_only: bool) -> Result<i32, VmError> {
        let mut items = Vec::new();
        if let Some(cf) = self.cf_of(class) {
            for m in declared_ctors(&cf) {
                if public_only && m.flags & ACC_PUBLIC == 0 {
                    continue;
                }
                let h = self.make_ctor_obj(class, m.idx)?;
                items.push(Slot::I(h));
            }
        }
        Ok(self.new_array("[Ljava/lang/reflect/Constructor;", items))
    }

    /// 继承链上**所有** public 字段（父类在前，同名同类型去重）
    pub(super) fn refl_public_fields(&mut self, class: &str) -> Result<i32, VmError> {
        let mut items: Vec<i32> = Vec::new();
        let mut seen: Vec<(String, String)> = Vec::new();
        for c in self.store.chain(class) {
            let Some(cf) = self.cf_of(&c) else { continue };
            for f in declared_fields(&cf) {
                if f.flags & ACC_PUBLIC == 0 {
                    continue;
                }
                let key = (f.name.clone(), f.desc.clone());
                if seen.contains(&key) {
                    continue;
                }
                seen.push(key);
                let h = self.make_field_obj(&c, f.idx)?;
                items.push(h);
            }
        }
        let items = items.into_iter().map(Slot::I).collect();
        Ok(self.new_array("[Ljava/lang/reflect/Field;", items))
    }

    /// 继承链 + 接口上的 public 方法（本类在前；同名同参数去重，**与 JDK 的顺序一致**）
    pub(super) fn refl_public_methods(&mut self, class: &str) -> Result<i32, VmError> {
        let mut items: Vec<i32> = Vec::new();
        let mut seen: Vec<(String, String)> = Vec::new();
        let mut owners: Vec<String> = self.store.chain(class);
        owners.extend(self.store.iface_closure(class));
        // 接口的静态方法不算继承
        for c in owners {
            let Some(cf) = self.cf_of(&c) else { continue };
            for m in declared_methods(&cf) {
                if m.flags & ACC_PUBLIC == 0 {
                    continue;
                }
                let key = (m.name.clone(), m.desc.clone());
                if seen.contains(&key) {
                    continue;
                }
                seen.push(key);
                let h = self.make_method_obj(&c, m.idx)?;
                items.push(h);
            }
        }
        let items = items.into_iter().map(Slot::I).collect();
        Ok(self.new_array("[Ljava/lang/reflect/Method;", items))
    }

    /// 所有 public 构造器（不继承 —— 构造器没有继承）
    pub(super) fn refl_public_ctors(&mut self, class: &str) -> Result<i32, VmError> {
        let mut items = Vec::new();
        if let Some(cf) = self.cf_of(class) {
            for m in declared_ctors(&cf) {
                if m.flags & ACC_PUBLIC == 0 {
                    continue;
                }
                let h = self.make_ctor_obj(class, m.idx)?;
                items.push(Slot::I(h));
            }
        }
        Ok(self.new_array("[Ljava/lang/reflect/Constructor;", items))
    }

    /// 按名字找字段（`declared` = 只看本类）
    pub(super) fn refl_find_field(&mut self, class: &str, name: &str, declared: bool) -> Option<i32> {
        let chains: Vec<String> =
            if declared { vec![class.to_string()] } else { self.store.chain(class) };
        for c in chains {
            let cf = self.cf_of(&c)?;
            for f in declared_fields(&cf) {
                if f.name != name {
                    continue;
                }
                if !declared && f.flags & ACC_PUBLIC == 0 {
                    continue;
                }
                return self.make_field_obj(&c, f.idx).ok();
            }
        }
        None
    }

    /// 类描述符 → 已解析的 class 文件（数组与基本类型没有）—— 给同目录的注册函数用
    pub(super) fn cf_pub(&self, desc: &str) -> Option<Rc<ClassFile>> {
        self.cf_of(desc)
    }

    /// 引用句柄 → 它的运行时类名（不是对象就 None）
    pub(super) fn obj_class_name(&self, h: i32) -> Option<String> {
        match self.ref_kind(h) {
            RefKind::Obj(c) => Some(c),
            RefKind::Str => Some("java/lang/String".to_string()),
            _ => None,
        }
    }

    /// 按参数类型找构造器（构造器不继承，所以只有本类；`public_only` 给 `getConstructor` 用）
    pub(super) fn refl_find_ctor(
        &mut self,
        class: &str,
        want: &[String],
        public_only: bool,
    ) -> Option<i32> {
        let cf = self.cf_of(class)?;
        for m in declared_ctors(&cf) {
            if public_only && m.flags & ACC_PUBLIC == 0 {
                continue;
            }
            let (params, _) = split_desc(&m.desc);
            if params.len() == want.len() && params.iter().zip(want).all(|(a, b)| a == b) {
                return self.make_ctor_obj(class, m.idx).ok();
            }
        }
        None
    }

    /// 按名字 + 参数类型找方法（`declared` = 只看本类）
    pub(super) fn refl_find_method(        &mut self,
        class: &str,
        name: &str,
        want: &[String],
        declared: bool,
    ) -> Option<i32> {
        let chains: Vec<String> = if declared {
            vec![class.to_string()]
        } else {
            let mut v = self.store.chain(class);
            v.extend(self.store.iface_closure(class));
            v
        };
        for c in chains {
            let Some(cf) = self.cf_of(&c) else { continue };
            for m in declared_methods(&cf) {
                if m.name != name {
                    continue;
                }
                if !declared && m.flags & ACC_PUBLIC == 0 {
                    continue;
                }
                let (params, _) = split_desc(&m.desc);
                if params.len() == want.len() && params.iter().zip(want).all(|(a, b)| a == b) {
                    return self.make_method_obj(&c, m.idx).ok();
                }
            }
        }
        None
    }

    // -------------------------------------------------------------- 反射对象的构造

    /// 造一个 `java.lang.reflect.Field`
    pub(super) fn make_field_obj(&mut self, class: &str, idx: usize) -> Result<i32, VmError> {
        let cf = self.cf_of(class).ok_or_else(|| VmError {
            msg: format!("{class} 没有字节码，做不出 Field"),
            exc: None,
        })?;
        let f = &cf.fields[idx];
        let (name, ty_desc, flags) = (name_of(&cf, f), desc_of(&cf, f), f.access_flags);
        let sig = annot::signature(&cf, &f.attributes);
        if std::env::var_os("VMLESS_REFL_DEBUG").is_some() {
            eprintln!(
                "[refl] Field {class}[{idx}] fc={} idx_slot={:?} idx_mods={:?} idx_override={:?}",
                self.store.field_count("java/lang/reflect/Field"),
                self.store.field_index("java/lang/reflect/Field", "slot"),
                self.store.field_index("java/lang/reflect/Field", "modifiers"),
                self.store.field_index("java/lang/reflect/AccessibleObject", "override")
            );
        }
        let cls = self.class_obj(class);
        let name_h = self.str_new(name);
        let ty = self.desc_class(&ty_desc);
        let sig_h = match sig {
            Some(s) => self.str_new(s),
            None => 0,
        };
        self.new_instance_args(
            "java/lang/reflect/Field",
            "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;IZILjava/lang/String;[B)V",
            &[
                Slot::I(cls),
                Slot::I(name_h),
                Slot::I(ty),
                // 注意：**声明**的形参名是 `(int slot, boolean trustedFinal, int modifiers)`，
                // 但构造器体里写的槽是 `modifiers ← 4`、`trustedFinal ← 5`、`slot ← 6`
                // （javap -c 才能看出来）。按声明顺序传会把 modifiers 和 slot 对调 ——
                // 表现是 `getModifiers()` 恒为 0、`getInt()` 读到别的字段（踩过）
                Slot::I(i32::from(flags)), // modifiers
                Slot::I(0),                // trustedFinal
                Slot::I(idx as i32),       // slot
                Slot::I(sig_h),
                Slot::I(0), // annotations（注解在 Rust 侧解析，字节数组用不上）
            ],
        )
    }

    /// 造一个 `java.lang.reflect.Method`
    pub(super) fn make_method_obj(&mut self, class: &str, idx: usize) -> Result<i32, VmError> {
        let cf = self.cf_of(class).ok_or_else(|| VmError {
            msg: format!("{class} 没有字节码，做不出 Method"),
            exc: None,
        })?;
        let m = &cf.methods[idx];
        let (name, desc, flags) = (name_of(&cf, m), desc_of(&cf, m), m.access_flags);
        let sig = annot::signature(&cf, &m.attributes);
        let (params, ret) = split_desc(&desc);
        let cls = self.class_obj(class);
        let name_h = self.str_new(name);
        let mut param_items = Vec::with_capacity(params.len());
        for p in &params {
            let h = self.desc_class(p);
            param_items.push(Slot::I(h));
        }
        let params_h = self.new_array("[Ljava/lang/Class;", param_items);
        let ret_h = self.desc_class(&ret);
        let exc_h = self.new_array("[Ljava/lang/Class;", Vec::new());
        let sig_h = match sig {
            Some(s) => self.str_new(s),
            None => 0,
        };
        self.new_instance_args(
            "java/lang/reflect/Method",
            "(Ljava/lang/Class;Ljava/lang/String;[Ljava/lang/Class;Ljava/lang/Class;[Ljava/lang/Class;IILjava/lang/String;[B[B[B)V",
            &[
                Slot::I(cls),
                Slot::I(name_h),
                Slot::I(params_h),
                Slot::I(ret_h),
                Slot::I(exc_h),
                Slot::I(i32::from(flags)),
                Slot::I(idx as i32),
                Slot::I(sig_h),
                Slot::I(0),
                Slot::I(0),
                Slot::I(0),
            ],
        )
    }

    /// 造一个 `java.lang.reflect.Constructor`
    pub(super) fn make_ctor_obj(&mut self, class: &str, idx: usize) -> Result<i32, VmError> {
        let cf = self.cf_of(class).ok_or_else(|| VmError {
            msg: format!("{class} 没有字节码，做不出 Constructor"),
            exc: None,
        })?;
        let m = &cf.methods[idx];
        let (desc, flags) = (desc_of(&cf, m), m.access_flags);
        let sig = annot::signature(&cf, &m.attributes);
        let (params, _) = split_desc(&desc);
        let cls = self.class_obj(class);
        let mut param_items = Vec::with_capacity(params.len());
        for p in &params {
            let h = self.desc_class(p);
            param_items.push(Slot::I(h));
        }
        let params_h = self.new_array("[Ljava/lang/Class;", param_items);
        let exc_h = self.new_array("[Ljava/lang/Class;", Vec::new());
        let sig_h = match sig {
            Some(s) => self.str_new(s),
            None => 0,
        };
        self.new_instance_args(
            "java/lang/reflect/Constructor",
            "(Ljava/lang/Class;[Ljava/lang/Class;[Ljava/lang/Class;IILjava/lang/String;[B[B)V",
            &[
                Slot::I(cls),
                Slot::I(params_h),
                Slot::I(exc_h),
                Slot::I(i32::from(flags)),
                Slot::I(idx as i32),
                Slot::I(sig_h),
                Slot::I(0),
                Slot::I(0),
            ],
        )
    }

    /// 分配一个对象并**真的执行**构造器（native / 反射内部要造 Java 对象时用）
    pub(super) fn new_instance_args(
        &mut self,
        class: &str,
        desc: &str,
        args: &[Slot],
    ) -> Result<i32, VmError> {
        if !self.ensure_class_loaded(class) {
            // 独立模式里没有 java.base：这类对象造不出来，明确报错（不许静默回空对象）
            return err(format!("{class} 没有字节码（反射需要真 java.base）"));
        }
        self.ensure_init(class)?;

        let n = self.store.field_count(class);
        self.objects.push(vec![Slot::I(0); n.max(1)]);
        self.object_class.push(class.to_string());
        let h = self.objects.len() as i32;
        if let Some((ci, mi)) = self.store.method_in_class(class, "<init>", desc) {
            let mut all = vec![Slot::I(h)];
            all.extend_from_slice(args);
            self.depth += 1;
            let r = self.execute(ci, mi, desc, &all, true);
            self.depth -= 1;
            r?;
        }
        Ok(h)
    }

    // -------------------------------------------------------------- 反射对象的字段读取

    /// 反射对象（Field/Method/Constructor）→ (声明类内部名, 成员下标)
    pub(super) fn refl_member_of(&self, h: i32, obj_class: &str) -> Option<(String, usize)> {
        if !matches!(self.ref_kind(h), RefKind::Obj(ref c) if self.is_subclass(c, obj_class)) {
            return None;
        }
        let slots = self.objects.get(h as usize - 1)?;
        let get = |name: &str| -> Option<Slot> {
            let i = self.store.field_index(obj_class, name)?;
            slots.get(i).copied()
        };
        let cls = get("clazz").map(|s| s.as_i32())?;
        let decl = self.class_obj_name(cls)?;
        let idx = get("slot")?.as_i32();
        Some((decl, idx as usize))
    }

    /// 反射对象上的 `boolean` 字段（`override`：`setAccessible(true)` 的结果）
    fn refl_flag(&self, h: i32, obj_class: &str, field: &str) -> bool {
        let slots = match self.objects.get(h as usize - 1) {
            Some(s) => s,
            None => return false,
        };
        let i = match self.store.field_index(obj_class, field) {
            Some(i) => i,
            None => return false,
        };
        slots.get(i).map(|s| s.as_i32() != 0).unwrap_or(false)
    }

    /// 访问检查：`override` 已设 / public 成员且 public 类 / 与声明类同一个嵌套宿主
    ///
    /// 本 VM **不做** `SecurityManager` 与模块系统，所以规则只剩这三条（HotSpot 的
    /// `Reflection.verifyMemberAccess` 在没有安全管理器时也是这个形状）。
    fn check_access(
        &mut self,
        flags: u16,
        decl: &str,
        override_: bool,
        what: &str,
    ) -> Result<(), VmError> {
        if override_ || flags & ACC_PUBLIC != 0 {
            return Ok(());
        }
        let caller = self
            .store
            .name_of_index(self.last_caller)
            .unwrap_or_default();
        if !caller.is_empty() && nest_prefix(&caller) == nest_prefix(decl) {
            return Ok(());
        }
        Err(self.throw_msg(
            "java/lang/IllegalAccessException",
            format!("class {} cannot access a member of class {} with modifiers \"{what}\"", if caller.is_empty() { "?".to_string() } else { caller.replace('/', ".") }, decl.replace('/', ".")),
        ))
    }

    // -------------------------------------------------------------- Field.get / set

    /// 字段的槽位读（不做装箱）
    /// 这个句柄是不是字符串（字符串堆句柄，或真字节码 `new` 出来的 String 对象）
    fn kind_is_str(&self, h: i32) -> bool {
        match self.ref_kind(h) {
            RefKind::Str => true,
            RefKind::Obj(c) => c == "java/lang/String",
            _ => false,
        }
    }

    fn kind_is_null(&self, h: i32) -> bool {
        matches!(self.ref_kind(h), RefKind::Null)
    }

    fn field_read(&mut self, decl: &str, idx: usize, target: i32) -> Slot {
        let Some(cf) = self.cf_of(decl) else { return Slot::I(0) };
        let f = &cf.fields[idx];
        let name = name_of(&cf, f);
        if f.access_flags & ACC_STATIC != 0 {
            return self.statics.get(&(decl.to_string(), name)).copied().unwrap_or(Slot::I(0));
        }
        if target > 0 {
            if let Some(slot) = field_slot_index(self, decl, idx) {
                if let Some(slots) = self.objects.get(target as usize - 1) {
                    return slots.get(slot).copied().unwrap_or(Slot::I(0));
                }
            }
        }
        Slot::I(0)
    }

    fn field_write(&mut self, decl: &str, idx: usize, target: i32, v: Slot) {
        let Some(cf) = self.cf_of(decl) else { return };
        let f = &cf.fields[idx];
        let name = name_of(&cf, f);
        if f.access_flags & ACC_STATIC != 0 {
            self.statics.insert((decl.to_string(), name), v);
            return;
        }
        if target > 0 {
            if let Some(slot) = field_slot_index(self, decl, idx) {
                if let Some(slots) = self.objects.get_mut(target as usize - 1) {
                    if slot < slots.len() {
                        slots[slot] = v;
                    }
                }
            }
        }
    }

    /// `Field.get(Object)` 一族的公共实现：按字段类型装箱
    pub(super) fn field_get(&mut self, field: i32, target: i32, want: Option<char>) -> Result<Slot, VmError> {
        let (decl, idx) = self
            .refl_member_of(field, "java/lang/reflect/Field")
            .ok_or_else(|| VmError { msg: "不是 Field 对象".into(), exc: None })?;
        let cf = self.cf_of(&decl).ok_or_else(|| VmError { msg: format!("{decl} 没有字节码"), exc: None })?;
        let f = match cf.fields.get(idx) { Some(x) => x, None => return err("Field 的 slot 越界") };
        let flags = f.access_flags;
        let ty = desc_of(&cf, f);
        let override_ = self.refl_flag(field, "java/lang/reflect/Field", "override");
        let what = modifiers_text(flags);
        self.check_access(flags, &decl, override_, &what)?;
        if flags & ACC_STATIC == 0 && target == 0 {
            return Err(self.throw_msg("java/lang/NullPointerException", String::new()));
        }
        if ty.starts_with('[') || ty.starts_with('L') {
            if target != 0 && flags & ACC_STATIC == 0 && !self.is_instance(target, &decl) {
                return Err(self.throw_msg(
                    "java/lang/IllegalArgumentException",
                    "object is not an instance of declaring class".to_string(),
                ));
            }
        }
        let v = self.field_read(&decl, idx, target);
        let prim = prim_of(&ty);
        match (want, prim) {
            // `getInt` 等：类型必须吻合，否则按 Java 的语义报错
            (Some(w), Some(p)) if w == p => Ok(coerce_prim(v, p)),
            (Some(w), _) => Err(self.throw_msg(
                "java/lang/IllegalArgumentException",
                format!("field {} has type {}, requested {w}", f_name(&cf, idx), ty),
            )),
            // `get(Object)`：装箱
            (None, Some(p)) => {
                let cls = box_of(p).unwrap_or("java/lang/Integer");
                let hv = coerce_prim(v, p);
                Ok(Slot::I(self.new_wrapper(cls, hv)))
            }
            (None, None) => {
                if flags & ACC_STATIC == 0 && target == 0 {
                    return Err(self.throw_msg("java/lang/NullPointerException", String::new()));
                }
                Ok(v)
            }
        }
    }

    /// `Field.set(Object, Object)` 一族
    pub(super) fn field_set(
        &mut self,
        field: i32,
        target: i32,
        value: Slot,
        prim_arg: Option<char>,
    ) -> Result<Slot, VmError> {
        let (decl, idx) = self
            .refl_member_of(field, "java/lang/reflect/Field")
            .ok_or_else(|| VmError { msg: "不是 Field 对象".into(), exc: None })?;
        let cf = self.cf_of(&decl).ok_or_else(|| VmError { msg: format!("{decl} 没有字节码"), exc: None })?;
        let f = match cf.fields.get(idx) { Some(x) => x, None => return err("Field 的 slot 越界") };
        let flags = f.access_flags;
        let is_static = flags & ACC_STATIC != 0;
        let ty = desc_of(&cf, f);
        let override_ = self.refl_flag(field, "java/lang/reflect/Field", "override");
        self.check_access(flags, &decl, override_, &modifiers_text(flags))?;
        if !is_static && target == 0 {
            return Err(self.throw_msg("java/lang/NullPointerException", String::new()));
        }
        if !is_static && !self.kind_is_str(target) && !self.is_instance(target, &decl) {
            return Err(self.throw_msg(
                "java/lang/IllegalArgumentException",
                "object is not an instance of declaring class".to_string(),
            ));
        }
        let field_prim = prim_of(&ty);
        let v = match (prim_arg, field_prim) {
            // `setInt` 一族：实参已经是基本类型
            (Some(p), Some(q)) => {
                if p != q {
                    return Err(self.throw_msg(
                        "java/lang/IllegalArgumentException",
                        format!("field {} has type {ty}", f_name(&cf, idx)),
                    ));
                }
                coerce_prim(value, q)
            }
            // `set(Object, Object)`：按字段类型拆箱
            (None, Some(q)) => {
                if value.as_i32() == 0 && self.kind_is_null(value.as_i32()) {
                    return Err(self.throw_msg(
                        "java/lang/IllegalArgumentException",
                        "Can not set a primitive field to null".to_string(),
                    ));
                }
                match self.wrapper_value(value.as_i32()) {
                    Some(sv) => {
                        let want = box_of(q).unwrap_or("");
                        let got = match self.ref_kind(value.as_i32()) {
                            RefKind::Obj(c) => c,
                            _ => String::new(),
                        };
                        if !want.is_empty() && got != want {
                            return Err(self.throw_msg(
                                "java/lang/IllegalArgumentException",
                                format!("argument type mismatch"),
                            ));
                        }
                        widen_prim(sv, q)
                    }
                    None => {
                        return Err(self.throw_msg(
                            "java/lang/IllegalArgumentException",
                            "argument type mismatch".to_string(),
                        ))
                    }
                }
            }
            (None, None) => {
                let h = value.as_i32();
                if h != 0 && !ty.starts_with('[') && !self.is_instance(h, &internal_of_desc(&ty)) {
                    // 接口/类都要认；String 句柄的运时类是 java/lang/String
                    if !(self.kind_is_str(h) && ty == "Ljava/lang/String;") {
                        return Err(self.throw_msg(
                            "java/lang/IllegalArgumentException",
                            "argument type mismatch".to_string(),
                        ));
                    }
                }
                if h != 0 && ty.starts_with('[') && !self.is_instance(h, &ty) {
                    return Err(self.throw_msg(
                        "java/lang/IllegalArgumentException",
                        "argument type mismatch".to_string(),
                    ));
                }
                value
            }
            (Some(_), None) => {
                return Err(self.throw_msg(
                    "java/lang/IllegalArgumentException",
                    format!("field {} has type {ty}", f_name(&cf, idx)),
                ))
            }
        };
        self.field_write(&decl, idx, target, v);
        Ok(Slot::I(0))
    }

    // -------------------------------------------------------------- Method.invoke

    /// `Method.invoke(Object, Object...)`
    pub(super) fn method_invoke(&mut self, method: i32, target: i32, args_h: i32) -> Result<Slot, VmError> {
        let (decl, idx) = self
            .refl_member_of(method, "java/lang/reflect/Method")
            .ok_or_else(|| VmError { msg: "不是 Method 对象".into(), exc: None })?;
        let cf = self.cf_of(&decl).ok_or_else(|| VmError { msg: format!("{decl} 没有字节码"), exc: None })?;
        let m = match cf.methods.get(idx) { Some(x) => x, None => return err("Method 的 slot 越界") };
        let flags = m.access_flags;
        let name = name_of(&cf, m);
        let desc = desc_of(&cf, m);
        let override_ = self.refl_flag(method, "java/lang/reflect/Method", "override") ;
        let override_ = override_ || self.refl_flag(method, "java/lang/reflect/AccessibleObject", "override");
        self.check_access(flags, &decl, override_, &modifiers_text(flags))?;
        let (params, _) = split_desc(&desc);
        let args = self.args_array(args_h, params.len(), "arguments")?;
        let mut values = Vec::with_capacity(args.len() + 1);
        let is_static = flags & ACC_STATIC != 0;
        if !is_static {
            if target == 0 {
                return Err(self.throw_msg("java/lang/NullPointerException", String::new()));
            }
            if !self.kind_is_str(target) && !self.is_instance(target, &decl) {
                return Err(self.throw_msg(
                    "java/lang/IllegalArgumentException",
                    "object is not an instance of declaring class".to_string(),
                ));
            }
            values.push(Slot::I(target));
        }
        for (i, p) in params.iter().enumerate() {
            values.push(self.coerce_arg(args[i], p, i)?);
        }
        let runtime = if is_static {
            self.store.method_in_class(&decl, &name, &desc)
        } else {
            let actual = match self.ref_kind(target) {
                RefKind::Obj(c) => Some(c),
                RefKind::Str => Some("java/lang/String".to_string()),
                RefKind::Arr(_) => Some("java/lang/Object".to_string()),
                RefKind::Null => None,
            };
            actual
                .as_deref()
                .and_then(|c| self.store.resolve_method(c, &name, &desc))
                .or_else(|| self.store.method_in_class(&decl, &name, &desc))
        };
        let (ci, mi) = match runtime {
            Some(t) => t,
            None => {
                let e = self.throw("java/lang/AbstractMethodError");
                return Err(self.wrap_invoke_error(e));
            }
        };
        self.depth += 1;
        let r = self.execute(ci, mi, &desc, &values, !is_static);
        self.depth -= 1;
        let (_, ret) = split_desc(&desc);
        match r {
            // `Method.invoke` 的返回类型是 `Object`：基本类型返回值必须**装箱**
            // （不装箱就是拿 6 当对象句柄用 —— 打印出来会是 `class jdk.internal.reflect.ReflectionFactory`）
            Ok(v) => match prim_of(&ret) {
                Some('V') => Ok(Slot::I(0)),
                Some(p) => {
                    let cls = box_of(p).unwrap_or("java/lang/Integer");
                    let boxed = self.new_wrapper(cls, coerce_prim(v, p));
                    Ok(Slot::I(boxed))
                }
                None => Ok(v),
            },
            Err(e) => Err(self.wrap_invoke_error(e)),
        }
    }

    /// `Object[]` 实参数组 → 槽（长度不符按 Java 的语义报 `IllegalArgumentException`）
    fn args_array(&mut self, args_h: i32, want: usize, what: &str) -> Result<Vec<Slot>, VmError> {
        let got = if args_h == 0 {
            0
        } else {
            self.arrays.get((-args_h - 1) as usize).map(|a| a.len()).unwrap_or(0)
        };
        if got != want {
            return Err(self.throw_msg(
                "java/lang/IllegalArgumentException",
                format!("wrong number of {what}: {got} expected: {want}"),
            ));
        }
        if args_h == 0 {
            return Ok(Vec::new());
        }
        Ok(self.arrays.get((-args_h - 1) as usize).cloned().unwrap_or_default())
    }

    /// 反射调用的实参转换：拆箱 → 宽化基本类型；引用类型要能赋给形参
    fn coerce_arg(&mut self, v: Slot, param: &str, i: usize) -> Result<Slot, VmError> {
        let h = v.as_i32();
        match prim_of(param) {
            Some(p) => {
                if h == 0 && self.kind_is_null(h) {
                    return Err(self.throw_msg(
                        "java/lang/IllegalArgumentException",
                        format!("argument {} cannot be null", i + 1),
                    ));
                }
                match self.wrapper_value(h) {
                    Some(sv) => {
                        let got = match self.ref_kind(h) {
                            RefKind::Obj(c) => box_of(p).map(|b| c.as_str() == b).unwrap_or(false),
                            _ => false,
                        };
                        if !got {
                            return Err(self.throw_msg(
                                "java/lang/IllegalArgumentException",
                                "argument type mismatch".to_string(),
                            ));
                        }
                        // 宽化：`Integer` 传给 `long` 形参是允许的
                        Ok(widen_prim(sv, p))
                    }
                    None => Err(self.throw_msg(
                        "java/lang/IllegalArgumentException",
                        "argument type mismatch".to_string(),
                    )),
                }
            }
            None => {
                if h == 0 {
                    return Ok(v);
                }
                let want = internal_of_desc(param);
                let ok = if param.starts_with('[') {
                    self.is_instance(h, param)
                } else {
                    self.is_instance(h, &want)
                };
                if !ok {
                    return Err(self.throw_msg(
                        "java/lang/IllegalArgumentException",
                        "argument type mismatch".to_string(),
                    ));
                }
                Ok(v)
            }
        }
    }

    /// 目标方法抛的异常 → `InvocationTargetException`
    fn wrap_invoke_error(&mut self, e: VmError) -> VmError {
        match e.exc {
            Some(h) => match self.new_instance_args(
                "java/lang/reflect/InvocationTargetException",
                "(Ljava/lang/Throwable;)V",
                &[Slot::I(h)],
            ) {
                Ok(ite) => VmError {
                    msg: format!("InvocationTargetException（cause = {}）", self.exc_text(h)),
                    exc: Some(ite),
                },
                Err(fail) if std::env::var_os("VMLESS_REFL_DEBUG").is_some() => {
                    eprintln!("[refl] 包装 InvocationTargetException 失败: {}", fail.msg);
                    e
                }
                Err(fail) => VmError {
                    msg: format!("{}（包装 InvocationTargetException 失败: {}）", e.msg, fail.msg),
                    exc: e.exc,
                },
            },
            None => e,
        }
    }

    // -------------------------------------------------------------- Constructor.newInstance

    /// `Constructor.newInstance(Object...)`
    pub(super) fn ctor_new_instance(&mut self, ctor: i32, args_h: i32) -> Result<Slot, VmError> {
        let (decl, idx) = self
            .refl_member_of(ctor, "java/lang/reflect/Constructor")
            .ok_or_else(|| VmError { msg: "不是 Constructor 对象".into(), exc: None })?;
        let cf = self.cf_of(&decl).ok_or_else(|| VmError { msg: format!("{decl} 没有字节码"), exc: None })?;
        let m = match cf.methods.get(idx) { Some(x) => x, None => return err("Constructor 的 slot 越界") };
        let flags = m.access_flags;
        let desc = desc_of(&cf, m);
        let override_ = self.refl_flag(ctor, "java/lang/reflect/Constructor", "override");
        let override_ = override_ || self.refl_flag(ctor, "java/lang/reflect/AccessibleObject", "override");
        self.check_access(flags, &decl, override_, &modifiers_text(flags))?;
        // 抽象类 / 接口不能实例化
        if cf.access_flags & ACC_ABSTRACT != 0 || cf.access_flags & ACC_INTERFACE != 0 {
            return Err(self.throw_msg(
                "java/lang/InstantiationException",
                format!("{}", decl.replace('/', ".")),
            ));
        }
        self.ensure_init(&decl)?;
        let (params, _) = split_desc(&desc);
        let args = self.args_array(args_h, params.len(), "arguments")?;
        let mut values = Vec::with_capacity(args.len());
        for (i, p) in params.iter().enumerate() {
            values.push(self.coerce_arg(args[i], p, i)?);
        }
        let n = self.store.field_count(&decl);
        self.objects.push(vec![Slot::I(0); n.max(1)]);
        self.object_class.push(decl.clone());
        let h = self.objects.len() as i32;
        let mut all = vec![Slot::I(h)];
        all.extend(values);
        let target = match self.store.method_in_class(&decl, "<init>", &desc) {
            Some(t) => t,
            None => return err(format!("{decl} 没有 {desc} 构造器")),
        };
        self.depth += 1;
        let r = self.execute(target.0, target.1, &desc, &all, true);
        self.depth -= 1;
        match r {
            Ok(_) => Ok(Slot::I(h)),
            Err(e) => Err(self.wrap_invoke_error(e)),
        }
    }

    // -------------------------------------------------------------- 注解

    /// 一批注解 → `Annotation[]`
    pub(super) fn ann_array(&mut self, anns: &[Ann]) -> Result<i32, VmError> {
        let mut items = Vec::with_capacity(anns.len());
        for a in anns {
            let h = self.make_ann_obj(a)?;
            items.push(Slot::I(h));
        }
        Ok(self.new_array("[Ljava/lang/annotation/Annotation;", items))
    }

    /// 成员的注解表（`attrs` 是字段/方法/类的属性表）
    pub(super) fn anns_of(&self, class: &str, attrs: &[crate::Attribute]) -> Vec<Ann> {
        match self.cf_of(class) {
            Some(cf) => annot::visible_annotations(&cf, attrs),
            None => Vec::new(),
        }
    }

    /// 按类型取一个注解（`null` = 没有）
    pub(super) fn ann_find(&mut self, class: &str, attrs: &[crate::Attribute], want: &str) -> Result<i32, VmError> {
        let want_key = format!("L{want};");
        let anns = self.anns_of(class, attrs);
        match anns.iter().find(|a| a.ty == want_key) {
            Some(a) => self.make_ann_obj(a),
            None => Ok(0),
        }
    }

    // -------------------------------------------------------------- 方法参数 / 注解

    /// `Executable.getParameterAnnotations()` → `Annotation[][]`
    pub(super) fn param_annotations(&mut self, exec: i32, obj_class: &str) -> Result<i32, VmError> {
        let (decl, idx) = match self.refl_member_of(exec, obj_class) {
            Some(t) => t,
            None => return err("不是 Executable 对象"),
        };
        let cf = match self.cf_of(&decl) {
            Some(c) => c,
            None => return err(format!("{decl} 没有字节码")),
        };
        let m = match cf.methods.get(idx) { Some(x) => x, None => return err("Executable 的 slot 越界") };
        let (params, _) = split_desc(&desc_of(&cf, m));
        let per = annot::visible_parameter_annotations(&cf, m.attributes.as_slice());
        // Java 的规则：只有 `RuntimeVisibleParameterAnnotations` 覆盖到的参数才有注解对象；
        // 参数个数可能比数组短（合成参数），按 javac 的语义缺的补空数组
        let mut items = Vec::with_capacity(params.len());
        for i in 0..params.len() {
            let anns = per.get(i).cloned().unwrap_or_default();
            let h = self.ann_array(&anns)?;
            items.push(Slot::I(h));
        }
        Ok(self.new_array("[Ljava/lang/annotation/Annotation;", items))
    }

    // -------------------------------------------------------------- 运行期类加载

    /// `Class.forName0` 的本体：按 classpath 加载，找不到 → `ClassNotFoundException`
    pub(super) fn refl_for_name(&mut self, binary: &str, initialize: bool) -> Result<i32, VmError> {
        let name = binary.trim();
        if name.is_empty() {
            return Err(self.throw_msg(
                "java/lang/ClassNotFoundException",
                String::new(),
            ));
        }
        // 数组：`[Ljava.lang.String;` / `[I` —— 不进类表，直接造类对象
        if name.starts_with('[') {
            let desc = name.replace('.', "/");
            return Ok(self.class_obj(&desc));
        }
        let internal = name.replace('.', "/");
        if !self.store.has_class(&internal) {
            let Some(cp) = self.cp.take() else {
                return Err(self.throw_msg("java/lang/ClassNotFoundException", name.to_string()));
            };
            let r = self.store.load_closure(&cp, &internal);
            self.cp = Some(cp);
            match r {
                Ok(added) => {
                    // 新类可能带来新的字段布局：类对象缓存里已有的不受影响，
                    // 但它们的槽位是按下标存的，布局重建后仍一致（父类字段在前）
                    let _ = added;
                }
                Err(_) => {
                    return Err(self.throw_msg("java/lang/ClassNotFoundException", name.to_string()));
                }
            }
        }
        if initialize {
            self.ensure_init(&internal)?;
        }
        Ok(self.class_obj(&internal))
    }

    /// `Class.getInterfaces0()`：直接父接口（本类 = `interfaces` 表）
    pub(super) fn refl_interfaces(&mut self, class: &str) -> Result<i32, VmError> {
        let names = self.store.interfaces_of(class);
        let mut items = Vec::with_capacity(names.len());
        for n in names {
            let h = self.class_obj(&n);
            items.push(Slot::I(h));
        }
        Ok(self.new_array("[Ljava/lang/Class;", items))
    }

    /// `getDeclaringClass0()` / `getSimpleBinaryName0()` 用的 `InnerClasses` 解析
    fn inner_class_entry(&self, class: &str) -> Option<(u16, u16, u16)> {
        let cf = self.cf_of(class)?;
        let ic_name = cf.constant_pool.iter().enumerate().find_map(|(i, e)| match e {
            Some(CpEntry::Utf8(b)) if b == b"InnerClasses" => Some(i as u16),
            _ => None,
        })?;
        let attr = cf.attributes.iter().find(|a| a.name_index == ic_name)?;
        let info = &attr.info;
        if info.len() < 2 {
            return None;
        }
        let n = u16::from_be_bytes([info[0], info[1]]) as usize;
        for k in 0..n {
            let o = 2 + k * 8;
            if o + 8 > info.len() {
                break;
            }
            let rd = |i: usize| u16::from_be_bytes([info[i], info[i + 1]]);
            let inner = rd(o);
            // inner_class_info_index 指向本类才轮到它
            let is_me = match cf.constant_pool.get(inner as usize).and_then(|e| e.as_ref()) {
                Some(CpEntry::Class(n)) => cf.utf8(*n).map(|b| b == class.as_bytes()).unwrap_or(false),
                _ => false,
            };
            if is_me {
                return Some((rd(o + 2), rd(o + 4), rd(o + 6)));
            }
        }
        None
    }

    pub(super) fn refl_declaring_class(&mut self, class: &str) -> Result<i32, VmError> {
        let Some((outer, _, _)) = self.inner_class_entry(class) else { return Ok(0) };
        if outer == 0 {
            return Ok(0);
        }
        let cf = match self.cf_of(class) {
            Some(c) => c,
            None => return Ok(0),
        };
        let name = match cf.constant_pool.get(outer as usize).and_then(|e| e.as_ref()) {
            Some(CpEntry::Class(n)) => cf.utf8(*n).map(|b| String::from_utf8_lossy(b).into_owned()),
            _ => None,
        };
        match name {
            Some(n) => Ok(self.class_obj(&n)),
            None => Ok(0),
        }
    }

    /// `InnerClasses` 里属于本类的那一项：`(outer_class_info_index, inner_name_index, access_flags)`
    pub(super) fn refl_inner_flags(&self, class: &str) -> Option<u16> {
        self.inner_class_entry(class).map(|(_, _, f)| f)
    }

    /// `getSimpleBinaryName0()`：内部类的「简单二进制名」（`Outer$Inner` → `Inner`）
    pub(super) fn refl_simple_binary_name(&mut self, class: &str) -> Result<i32, VmError> {
        let Some((outer, simple, _)) = self.inner_class_entry(class) else { return Ok(0) };
        if outer == 0 {
            return Ok(0);
        }
        let cf = match self.cf_of(class) {
            Some(c) => c,
            None => return Ok(0),
        };
        let text = if simple == 0 {
            // 匿名类：简单名是空串
            String::new()
        } else {
            match cf.utf8(simple) {
                Some(b) => String::from_utf8_lossy(b).into_owned(),
                None => return Ok(0),
            }
        };
        Ok(self.str_new(text))
    }

    // -------------------------------------------------------------- 名字（JLS 口径）

    /// `Class.getSimpleName()`：数组是 `元素名[]`，内部类是简单名，顶层类是去包名
    pub(super) fn refl_simple_name(&mut self, class: &str) -> String {
        if let Some(elem) = class.strip_prefix('[') {
            let inner = self.refl_simple_name(&internal_of_desc(elem));
            return format!("{inner}[]");
        }
        if let Some((outer, simple, _)) = self.inner_class_entry(class) {
            if outer != 0 {
                if simple == 0 {
                    return String::new(); // 匿名类
                }
                if let Some(cf) = self.cf_of(class) {
                    if let Some(b) = cf.utf8(simple) {
                        return String::from_utf8_lossy(b).into_owned();
                    }
                }
            }
        }
        let binary = self.class_binary_name(class);
        match binary.rfind('.') {
            Some(i) => binary[i + 1..].to_string(),
            None => binary,
        }
    }

    /// `Class.getCanonicalName()`：内部类是 `Outer.Inner`，匿名类是 null
    pub(super) fn refl_canonical_name(&mut self, class: &str) -> Option<String> {
        if let Some(elem) = class.strip_prefix('[') {
            let inner = self.refl_canonical_name(&internal_of_desc(elem))?;
            return Some(format!("{inner}[]"));
        }
        if let Some((outer, _, _)) = self.inner_class_entry(class) {
            if outer != 0 {
                let decl = match self.refl_declaring_class(class) {
                    Ok(h) if h != 0 => self.class_obj_name(h),
                    _ => None,
                };
                match decl {
                    Some(d) => {
                        let base = self.refl_canonical_name(&d)?;
                        return Some(format!("{base}.{}", self.refl_simple_name(class)));
                    }
                    None => return None,
                }
            }
        }
        Some(self.class_binary_name(class))
    }
}

fn f_name(cf: &ClassFile, idx: usize) -> String {
    name_of(cf, &cf.fields[idx])
}

/// 访问标志 → `Modifier.toString` 风格的文本（只用于异常消息）
fn modifiers_text(flags: u16) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for (bit, text) in [
        (ACC_PUBLIC, "public"),
        (ACC_PRIVATE, "private"),
        (ACC_PROTECTED, "protected"),
        (ACC_STATIC, "static"),
        (ACC_FINAL, "final"),
        (ACC_ABSTRACT, "abstract"),
    ] {
        if flags & bit != 0 {
            parts.push(text);
        }
    }
    parts.join(" ")
}

/// `String.hashCode`
fn java_str_hash(s: &str) -> i32 {
    let mut h = 0i32;
    for c in s.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(i32::from(c));
    }
    h
}

/// 把一个槽按基本类型 `p` 的语义取值（宽化/窄化都在这里统一）
fn coerce_prim(v: Slot, p: char) -> Slot {
    match p {
        'J' => Slot::L(v.as_i64()),
        'F' => Slot::Fl(v.as_f32()),
        'D' => Slot::F(v.as_f64()),
        'Z' => Slot::I(i32::from(v.as_i32() != 0)),
        'B' => Slot::I((v.as_i32() as i8) as i32),
        'C' => Slot::I((v.as_i32() as u16) as i32),
        'S' => Slot::I((v.as_i32() as i16) as i32),
        _ => Slot::I(v.as_i32()),
    }
}

/// 无符号/有符号的窄化：包装类里的值 → 形参类型（`Byte` → `int` 等）
fn widen_prim(v: Slot, p: char) -> Slot {
    coerce_prim(v, p)
}

// ================================================================== 注册入口
//
// 两类分开登记（判据见 `native_reg.rs` 的头注释）：
// - `r.add`：OpenJDK 里**本来就是 `native`** 的（`forName0` / `getDeclared*0` …）
// - `r.add_override`：OpenJDK 里是**字节码**、但语义必须由 VM 给的（反射访问器一族）

pub(super) fn register(r: &mut super::native_reg::NativeRegistry) {
    const CLS: &str = "java/lang/Class";
    r.add(CLS, "forName0", "(Ljava/lang/String;ZLjava/lang/ClassLoader;Ljava/lang/Class;)Ljava/lang/Class;", cls_for_name0);
    r.add(CLS, "getDeclaredFields0", "(Z)[Ljava/lang/reflect/Field;", cls_declared_fields0);
    r.add(CLS, "getDeclaredMethods0", "(Z)[Ljava/lang/reflect/Method;", cls_declared_methods0);
    r.add(CLS, "getDeclaredConstructors0", "(Z)[Ljava/lang/reflect/Constructor;", cls_declared_ctors0);
    r.add(CLS, "getInterfaces0", "()[Ljava/lang/Class;", cls_interfaces0);
    r.add(CLS, "getDeclaringClass0", "()Ljava/lang/Class;", cls_declaring_class0);
    r.add(CLS, "getSimpleBinaryName0", "()Ljava/lang/String;", cls_simple_binary_name0);
    // 类级泛型签名（`class Box<T>`）：`sun.reflect.generics` 靠它解析（`getGenericSuperclass` 等）
    r.add(CLS, "getGenericSignature0", "()Ljava/lang/String;", cls_generic_signature0);

    // ---- Class 的成员查询（字节码方法，改由 VM 直接给结果，避开 ReflectionData/过滤器）----
    const FIELD_ARR: &str = "()[Ljava/lang/reflect/Field;";
    const METHOD_ARR: &str = "()[Ljava/lang/reflect/Method;";
    const CTOR_ARR: &str = "()[Ljava/lang/reflect/Constructor;";
    r.add_override(CLS, "getDeclaredFields", FIELD_ARR, cls_get_declared_fields);
    r.add_override(CLS, "getFields", FIELD_ARR, cls_get_fields);
    r.add_override(CLS, "getDeclaredMethods", METHOD_ARR, cls_get_declared_methods);
    r.add_override(CLS, "getMethods", METHOD_ARR, cls_get_methods);
    r.add_override(CLS, "getDeclaredConstructors", CTOR_ARR, cls_get_declared_ctors);
    r.add_override(CLS, "getConstructors", CTOR_ARR, cls_get_ctors);
    r.add_override(CLS, "getDeclaredField", "(Ljava/lang/String;)Ljava/lang/reflect/Field;", cls_get_declared_field);
    r.add_override(CLS, "getField", "(Ljava/lang/String;)Ljava/lang/reflect/Field;", cls_get_field);
    r.add_override(
        CLS,
        "getDeclaredMethod",
        "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
        cls_get_declared_method,
    );
    r.add_override(
        CLS,
        "getMethod",
        "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
        cls_get_method,
    );
    r.add_override(
        CLS,
        "getDeclaredConstructor",
        "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
        cls_get_declared_ctor,
    );
    r.add_override(
        CLS,
        "getConstructor",
        "([Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
        cls_get_ctor,
    );
    r.add_override(CLS, "getInterfaces", "()[Ljava/lang/Class;", cls_get_interfaces);
    r.add_override(CLS, "getSimpleName", "()Ljava/lang/String;", cls_get_simple_name);
    r.add_override(CLS, "getCanonicalName", "()Ljava/lang/String;", cls_get_canonical_name);
    r.add_override(CLS, "getTypeName", "()Ljava/lang/String;", cls_get_type_name);
    r.add_override(CLS, "toString", "()Ljava/lang/String;", cls_to_string);
    // `StringJoiner.toString()`：见文件末尾那段注释（`JLA.join` 依赖 initPhase1）
    r.add_override(
        "java/util/StringJoiner",
        "toString",
        "()Ljava/lang/String;",
        string_joiner_to_string_op,
    );

    // ---- AccessibleObject：访问检查与注解（Field/Method/Constructor 共用）----
    const AO: &str = "java/lang/reflect/AccessibleObject";
    const ANN: &str = "Ljava/lang/annotation/Annotation;";
    for owner in [AO, "java/lang/reflect/Executable", "java/lang/reflect/Field", "java/lang/reflect/Method", "java/lang/reflect/Constructor"] {
        let owner: &'static str = match owner {
            "java/lang/reflect/AccessibleObject" => AO,
            "java/lang/reflect/Executable" => "java/lang/reflect/Executable",
            "java/lang/reflect/Field" => "java/lang/reflect/Field",
            "java/lang/reflect/Method" => "java/lang/reflect/Method",
            _ => "java/lang/reflect/Constructor",
        };
        r.add_override(owner, "getAnnotation", "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;", ao_get_annotation);
        r.add_override(owner, "getAnnotations", "()[Ljava/lang/annotation/Annotation;", ao_get_annotations);
        r.add_override(owner, "getDeclaredAnnotations", "()[Ljava/lang/annotation/Annotation;", ao_get_annotations);
        r.add_override(owner, "isAnnotationPresent", "(Ljava/lang/Class;)Z", ao_is_annotation_present);
        let _ = ANN;
    }
    // `Class` 上的注解接口（`Class implements AnnotatedElement`，但方法在类里自己声明了）
    r.add_override(CLS, "getAnnotation", "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;", cls_get_annotation);
    r.add_override(CLS, "getAnnotations", "()[Ljava/lang/annotation/Annotation;", cls_get_annotations);
    r.add_override(CLS, "getDeclaredAnnotations", "()[Ljava/lang/annotation/Annotation;", cls_get_annotations);
    r.add_override(CLS, "isAnnotationPresent", "(Ljava/lang/Class;)Z", cls_is_annotation_present);

    // ---- Field 的读 / 写 ----
    const FLD: &str = "java/lang/reflect/Field";
    r.add_override(FLD, "get", "(Ljava/lang/Object;)Ljava/lang/Object;", field_get_object);
    r.add_override(FLD, "getBoolean", "(Ljava/lang/Object;)Z", field_get_boolean);
    r.add_override(FLD, "getByte", "(Ljava/lang/Object;)B", field_get_byte);
    r.add_override(FLD, "getChar", "(Ljava/lang/Object;)C", field_get_char);
    r.add_override(FLD, "getShort", "(Ljava/lang/Object;)S", field_get_short);
    r.add_override(FLD, "getInt", "(Ljava/lang/Object;)I", field_get_int);
    r.add_override(FLD, "getLong", "(Ljava/lang/Object;)J", field_get_long);
    r.add_override(FLD, "getFloat", "(Ljava/lang/Object;)F", field_get_float);
    r.add_override(FLD, "getDouble", "(Ljava/lang/Object;)D", field_get_double);
    r.add_override(FLD, "set", "(Ljava/lang/Object;Ljava/lang/Object;)V", field_set_object);
    r.add_override(FLD, "setBoolean", "(Ljava/lang/Object;Z)V", field_set_boolean);
    r.add_override(FLD, "setByte", "(Ljava/lang/Object;B)V", field_set_byte);
    r.add_override(FLD, "setChar", "(Ljava/lang/Object;C)V", field_set_char);
    r.add_override(FLD, "setShort", "(Ljava/lang/Object;S)V", field_set_short);
    r.add_override(FLD, "setInt", "(Ljava/lang/Object;I)V", field_set_int);
    r.add_override(FLD, "setLong", "(Ljava/lang/Object;J)V", field_set_long);
    r.add_override(FLD, "setFloat", "(Ljava/lang/Object;F)V", field_set_float);
    r.add_override(FLD, "setDouble", "(Ljava/lang/Object;D)V", field_set_double);

    // ---- Method / Constructor 的调用 ----
    r.add_override(
        "java/lang/reflect/Method",
        "invoke",
        "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        method_invoke_op,
    );
    r.add_override(
        "java/lang/reflect/Constructor",
        "newInstance",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        ctor_new_instance_op,
    );
    // 参数注解：`Executable.getParameterAnnotations` 抽象，真正实现在 Method / Constructor
    for owner in ["java/lang/reflect/Method", "java/lang/reflect/Constructor"] {
        let owner: &'static str =
            if owner == "java/lang/reflect/Method" { "java/lang/reflect/Method" } else { "java/lang/reflect/Constructor" };
        r.add_override(owner, "getParameterAnnotations", "()[[Ljava/lang/annotation/Annotation;", exec_param_annotations);
    }
}

// ------------------------------------------------------------------ Class 的 native

fn cls_for_name0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    // `Class.forName(null)` 是 NPE（与 JDK 一致）；空串才是 ClassNotFoundException
    if f.arg_i(0) == 0 {
        return Err(vm.throw_msg("java/lang/NullPointerException", String::new()));
    }
    let text = vm.text_of(f.arg_i(0));
    let init = f.arg_i(1) != 0;
    vm.refl_for_name(&text, init).map(Slot::I)
}

fn cls_declared_fields0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    let h = vm.refl_declared_fields(&class, f.arg_i(0) != 0)?;
    Ok(Slot::I(h))
}

fn cls_declared_methods0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    let h = vm.refl_declared_methods(&class, f.arg_i(0) != 0)?;
    Ok(Slot::I(h))
}

fn cls_declared_ctors0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    let h = vm.refl_declared_ctors(&class, f.arg_i(0) != 0)?;
    Ok(Slot::I(h))
}

fn cls_interfaces0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    let h = vm.refl_interfaces(&class)?;
    Ok(Slot::I(h))
}

fn cls_declaring_class0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    vm.refl_declaring_class(&class).map(Slot::I)
}

fn cls_simple_binary_name0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    vm.refl_simple_binary_name(&class).map(Slot::I)
}

// ------------------------------------------------------------------ Class 的成员查询

fn cls_get_declared_fields(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    Ok(Slot::I(vm.refl_declared_fields(&class, false)?))
}

fn cls_get_fields(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    Ok(Slot::I(vm.refl_public_fields(&class)?))
}

fn cls_get_declared_methods(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    Ok(Slot::I(vm.refl_declared_methods(&class, false)?))
}

fn cls_get_methods(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    Ok(Slot::I(vm.refl_public_methods(&class)?))
}

fn cls_get_declared_ctors(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    Ok(Slot::I(vm.refl_declared_ctors(&class, false)?))
}

fn cls_get_ctors(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    Ok(Slot::I(vm.refl_public_ctors(&class)?))
}

fn cls_get_declared_field(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    let name = vm.text_of(f.arg_i(0));
    match vm.refl_find_field(&class, &name, true) {
        Some(h) => Ok(Slot::I(h)),
        None => {
            let msg = name.clone();
            Err(vm.throw_msg("java/lang/NoSuchFieldException", msg))
        }
    }
}

fn cls_get_field(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    let name = vm.text_of(f.arg_i(0));
    match vm.refl_find_field(&class, &name, false) {
        Some(h) => Ok(Slot::I(h)),
        None => {
            let msg = name.clone();
            Err(vm.throw_msg("java/lang/NoSuchFieldException", msg))
        }
    }
}

/// `getMethod(name, Class...)` 的参数类型数组（`Class[]` → **描述符**列表）
///
/// 类对象存的是内部名（`java/lang/String`），而方法的描述符里是 `Ljava/lang/String;` ——
/// 必须在这里换算，否则按名字找方法永远匹配不上对象类型的形参（踩过）。
pub(super) fn class_arg_list(vm: &mut Vm, h: i32) -> Vec<String> {
    if h == 0 {
        return Vec::new();
    }
    let items = vm.arrays.get((-h - 1) as usize).cloned().unwrap_or_default();
    items
        .iter()
        .filter_map(|s| vm.class_obj_name(s.as_i32()))
        .map(|n| {
            if n.len() == 1 || n.starts_with('[') {
                n
            } else {
                format!("L{n};")
            }
        })
        .collect()
}

fn cls_get_declared_method(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    let name = vm.text_of(f.arg_i(0));
    let types = class_arg_list(vm, f.arg_i(1));
    match vm.refl_find_method(&class, &name, &types, true) {
        Some(h) => Ok(Slot::I(h)),
        None => {
            let msg = sig_text(&class, &name, &types);
            Err(vm.throw_msg("java/lang/NoSuchMethodException", msg))
        }
    }
}

fn cls_get_method(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    let name = vm.text_of(f.arg_i(0));
    let types = class_arg_list(vm, f.arg_i(1));
    match vm.refl_find_method(&class, &name, &types, false) {
        Some(h) => Ok(Slot::I(h)),
        None => {
            let msg = sig_text(&class, &name, &types);
            Err(vm.throw_msg("java/lang/NoSuchMethodException", msg))
        }
    }
}

fn sig_text(class: &str, name: &str, types: &[String]) -> String {
    let args: Vec<String> =
        types.iter().map(|t| super::heap::java_class_name(&internal_of_desc(t))).collect();
    format!("{}.{}({})", class.replace('/', "."), name, args.join(", "))
}

fn cls_get_declared_ctor(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    let types = class_arg_list(vm, f.arg_i(0));
    match vm.refl_find_ctor(&class, &types, false) {
        Some(h) => Ok(Slot::I(h)),
        None => {
            let msg = sig_text(&class, "<init>", &types);
            Err(vm.throw_msg("java/lang/NoSuchMethodException", msg))
        }
    }
}

fn cls_get_ctor(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    let types = class_arg_list(vm, f.arg_i(0));
    match vm.refl_find_ctor(&class, &types, true) {
        Some(h) => Ok(Slot::I(h)),
        None => {
            let msg = sig_text(&class, "<init>", &types);
            Err(vm.throw_msg("java/lang/NoSuchMethodException", msg))
        }
    }
}

fn cls_get_interfaces(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    let h = vm.refl_interfaces(&class)?;
    Ok(Slot::I(h))
}

fn cls_get_simple_name(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    let text = vm.refl_simple_name(&class);
    Ok(Slot::I(vm.str_new(text)))
}

fn cls_get_canonical_name(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    match vm.refl_canonical_name(&class) {
        Some(text) => Ok(Slot::I(vm.str_new(text))),
        None => Ok(Slot::I(0)),
    }
}

fn cls_get_type_name(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    let text = match vm.refl_canonical_name(&class) {
        Some(t) => t,
        None => vm.refl_simple_name(&class),
    };
    Ok(Slot::I(vm.str_new(text)))
}

fn cls_to_string(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    // `Class.toString()` 的正规定义就是这三种前缀 + `getName()`：
    // 接口 → `interface `，基本类型 → 空，其余（含**数组**）→ `class `
    let flags = vm.store.class_file(&class).map(|cf| cf.access_flags).unwrap_or(0);
    let name = super::heap::java_class_name(&class);
    let text = if flags & ACC_INTERFACE != 0 {
        format!("interface {name}")
    } else if class.len() == 1 {
        name
    } else {
        format!("class {name}")
    };
    Ok(Slot::I(vm.str_new(text)))
}


// ------------------------------------------------------------------ 注解

/// 反射对象（Field/Method/Constructor）对应的属性表
///
/// 必须由调用点说明这是**字段**还是**方法**：`slot` 只是各自表里的下标，
/// 猜错了会读到另一个成员的注解（`@Inject` 全变 false、字段注解读到构造器的，踩过）。
fn member_attrs(vm: &Vm, decl: &str, idx: usize, is_method: bool) -> Vec<crate::Attribute> {
    let Some(cf) = vm.cf_pub(decl) else { return Vec::new() };
    let list = if is_method { &cf.methods } else { &cf.fields };
    list.get(idx).map(|m| m.attributes.clone()).unwrap_or_default()
}


fn ao_get_annotation(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let want = vm.class_obj_name(f.arg_i(0)).unwrap_or_default();
    let obj_class = vm.obj_class_name(this).unwrap_or_default();
    if want.is_empty() {
        return Err(vm.throw_msg("java/lang/NullPointerException", String::new()));
    }
    let Some((decl, idx)) = vm.refl_member_of(this, &obj_class) else { return Ok(Slot::I(0)) };
    let is_method = obj_class != "java/lang/reflect/Field";
    let attrs = member_attrs(vm, &decl, idx, is_method);
    if attrs.is_empty() {
        return Ok(Slot::I(0));
    }
    vm.ann_find(&decl, &attrs, &want).map(Slot::I)
}

fn ao_get_annotations(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let obj_class = vm.obj_class_name(this).unwrap_or_default();
    let Some((decl, idx)) = vm.refl_member_of(this, &obj_class) else {
        let h = vm.new_array("[Ljava/lang/annotation/Annotation;", Vec::new());
        return Ok(Slot::I(h));
    };
    let is_method = obj_class != "java/lang/reflect/Field";
    let attrs = member_attrs(vm, &decl, idx, is_method);
    let anns = vm.anns_of(&decl, &attrs);
    let h = vm.ann_array(&anns)?;
    Ok(Slot::I(h))
}

fn ao_is_annotation_present(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let v = ao_get_annotation(vm, f)?;
    Ok(Slot::I(i32::from(v.as_i32() != 0)))
}

fn cls_get_annotation(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    let want = vm.class_obj_name(f.arg_i(0)).unwrap_or_default();
    if want.is_empty() {
        return Err(vm.throw_msg("java/lang/NullPointerException", String::new()));
    }
    let Some(cf) = vm.cf_pub(&class) else { return Ok(Slot::I(0)) };
    let attrs = cf.attributes.clone();
    vm.ann_find(&class, &attrs, &want).map(Slot::I)
}

fn cls_get_annotations(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    let Some(cf) = vm.cf_pub(&class) else {
        let h = vm.new_array("[Ljava/lang/annotation/Annotation;", Vec::new());
        return Ok(Slot::I(h));
    };
    let attrs = cf.attributes.clone();
    let anns = vm.anns_of(&class, &attrs);
    let h = vm.ann_array(&anns)?;
    Ok(Slot::I(h))
}

fn cls_is_annotation_present(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let v = cls_get_annotation(vm, f)?;
    Ok(Slot::I(i32::from(v.as_i32() != 0)))
}

fn exec_param_annotations(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let obj_class = vm.obj_class_name(this).unwrap_or_default();
    let h = vm.param_annotations(this, &obj_class)?;
    Ok(Slot::I(h))
}

// ------------------------------------------------------------------ Field 的读 / 写

fn field_get_object(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (field, target) = (f.this(), f.arg_i(0));
    vm.field_get(field, target, None)
}

macro_rules! field_getter {
    ($name:ident, $prim:expr) => {
        fn $name(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
            let (field, target) = (f.this(), f.arg_i(0));
            vm.field_get(field, target, Some($prim))
        }
    };
}
field_getter!(field_get_boolean, 'Z');
field_getter!(field_get_byte, 'B');
field_getter!(field_get_char, 'C');
field_getter!(field_get_short, 'S');
field_getter!(field_get_int, 'I');
field_getter!(field_get_long, 'J');
field_getter!(field_get_float, 'F');
field_getter!(field_get_double, 'D');

fn field_set_object(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (field, target, value) = (f.this(), f.arg_i(0), f.arg(1));
    vm.field_set(field, target, value, None)
}

macro_rules! field_setter {
    ($name:ident, $prim:expr) => {
        fn $name(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
            let (field, target, value) = (f.this(), f.arg_i(0), f.arg(1));
            vm.field_set(field, target, value, Some($prim))
        }
    };
}
field_setter!(field_set_boolean, 'Z');
field_setter!(field_set_byte, 'B');
field_setter!(field_set_char, 'C');
field_setter!(field_set_short, 'S');
field_setter!(field_set_int, 'I');
field_setter!(field_set_long, 'J');
field_setter!(field_set_float, 'F');
field_setter!(field_set_double, 'D');

// ------------------------------------------------------------------ Method / Constructor

fn method_invoke_op(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (method, target, args_h) = (f.this(), f.arg_i(0), f.arg_i(1));
    vm.method_invoke(method, target, args_h)
}

fn ctor_new_instance_op(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (ctor, args_h) = (f.this(), f.arg_i(0));
    vm.ctor_new_instance(ctor, args_h)
}


// ================================================================== StringJoiner.toString
//
// `java.util.StringJoiner.toString()` 是 `JLA.join(prefix, suffix, delimiter, elts, size)`
// —— `JLA` 是 `SharedSecrets.getJavaLangAccess()`，而那个钩子是 `System.initPhase1()`
// 挂上去的（HotSpot 里由**启动器**调用，不是字节码）。本 VM 不走 initPhase1
// （它会把 Properties/FileInputStream 一整条链拖进来），所以这一条按语义直接给：
// `prefix + 元素按 delimiter 连接 + suffix`，`size == 0` 时回 `emptyValue`（有的话）。

impl Vm {
    fn sj_field(&self, h: i32, name: &str) -> Slot {
        let Some(i) = self.store.field_index("java/util/StringJoiner", name) else { return Slot::I(0) };
        self.objects.get(h as usize - 1).and_then(|s| s.get(i).copied()).unwrap_or(Slot::I(0))
    }

    /// `StringJoiner.toString()`
    pub(super) fn string_joiner_to_string(&mut self, h: i32) -> Result<String, VmError> {
        let prefix = self.sj_field(h, "prefix").as_i32();
        let delimiter = self.sj_field(h, "delimiter").as_i32();
        let suffix = self.sj_field(h, "suffix").as_i32();
        let elts = self.sj_field(h, "elts").as_i32();
        let size = self.sj_field(h, "size").as_i32();
        let empty_value = self.sj_field(h, "emptyValue").as_i32();
        if size == 0 && empty_value != 0 {
            return Ok(self.text_of(empty_value));
        }
        let mut out = self.text_of(prefix);
        let delim = self.text_of(delimiter);
        let items: Vec<i32> = if elts == 0 {
            Vec::new()
        } else {
            self.arrays
                .get((-elts - 1) as usize)
                .map(|a| a.iter().map(|s| s.as_i32()).collect())
                .unwrap_or_default()
        };
        for (i, it) in items.iter().enumerate().take(size.max(0) as usize) {
            if i > 0 {
                out.push_str(&delim);
            }
            out.push_str(&self.text_of(*it));
        }
        out.push_str(&self.text_of(suffix));
        Ok(out)
    }
}

fn string_joiner_to_string_op(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let text = vm.string_joiner_to_string(f.this())?;
    Ok(Slot::I(vm.str_new(text)))
}

/// 注解成员值 → **基本类型**槽（`int` / `long` / `boolean` … 的成员方法返回值）
fn ann_val_prim(v: &AnnVal, p: char) -> Slot {
    let as_int = match v {
        AnnVal::Bool(b) => i32::from(*b),
        AnnVal::Byte(b) => *b as i32,
        AnnVal::Char(c) => *c as i32,
        AnnVal::Short(s) => *s as i32,
        AnnVal::Int(i) => *i,
        // 其余类型对不上，按 0 兜底（畸形的 class 文件不值得 panic）
        _ => 0,
    };
    match (v, p) {
        (AnnVal::Long(l), 'J') => Slot::L(*l),
        (AnnVal::Float(f), 'F') => Slot::Fl(*f),
        (AnnVal::Double(d), 'D') => Slot::F(*d),
        (_, 'J') => Slot::L(as_int as i64),
        (_, 'F') => Slot::Fl(as_int as f32),
        (_, 'D') => Slot::F(as_int as f64),
        _ => Slot::I(as_int),
    }
}

/// `java.util.HashMap` 的**迭代顺序**（`AnnotationType.memberDefaults` 就是一个
/// `HashMap`，而 `Annotation.toString` 按它的迭代顺序打印成员）。
///
/// 规则：容量取「装得下 n 个键、负载因子 0.75」的最小 2 的幂；桶下标 =
/// `(cap-1) & (h ^ (h >>> 16))`；按桶下标升序遍历，同桶内按插入顺序。
fn hashmap_order(keys: &[String]) -> Vec<usize> {
    let n = keys.len();
    let mut cap = 1usize;
    while (cap as f64 * 0.75) < n as f64 {
        cap *= 2;
    }
    let mut items: Vec<(usize, usize)> = keys
        .iter()
        .enumerate()
        .map(|(i, k)| {
            let h = java_str_hash(k) as u32;
            let spread = h ^ (h >> 16);
            ((spread as usize) & (cap - 1), i)
        })
        .collect();
    items.sort_by_key(|(bucket, ins)| (*bucket, *ins));
    items.into_iter().map(|(_, i)| i).collect()
}

/// `AnnotationInvocationHandler.memberValueToString` 的等价实现
fn ann_val_source(v: &AnnVal) -> String {
    match v {
        AnnVal::Bool(b) => b.to_string(),
        AnnVal::Byte(b) => format!("(byte)0x{:02x}", *b as u8),
        AnnVal::Char(c) => format!("'{}'", quote_char(*c as u32, true)),
        AnnVal::Short(s) => s.to_string(),
        AnnVal::Int(i) => i.to_string(),
        AnnVal::Long(l) => format!("{l}L"),
        AnnVal::Float(f) => {
            if f.is_finite() {
                format!("{}f", crate::java_float_to_string(*f))
            } else if f.is_infinite() {
                if *f < 0.0 { "-1.0f/0.0f".into() } else { "1.0f/0.0f".into() }
            } else {
                "0.0f/0.0f".into()
            }
        }
        AnnVal::Double(d) => {
            if d.is_finite() {
                crate::java_double_to_string(*d)
            } else if d.is_infinite() {
                if *d < 0.0 { "-1.0/0.0".into() } else { "1.0/0.0".into() }
            } else {
                "0.0/0.0".into()
            }
        }
        AnnVal::Str(s) => {
            let mut out = String::from("\"");
            for ch in s.encode_utf16() {
                out.push_str(&quote_char(ch as u32, false));
            }
            out.push('"');
            out
        }
        AnnVal::Enum(_, name) => name.clone(),
        AnnVal::Class(_) => {
            // `toSourceString(Class)` = 规范名 + `.class`（匿名类的规范名是 null）
            let mut s = ann_val_class_name(v).unwrap_or_else(|| "<no canonical name>".into());
            s.push_str(".class");
            s
        }
        AnnVal::Anno(a) => {
            // 嵌套注解按 `toString()` 打印：成员顺序用 HashMap 序；只有一个 `value`
            // 成员时省略名字（与 `toStringImpl` 的 loneValue 规则一致）
            let ty = internal_of_desc(&a.ty);
            let mut vals: Vec<(String, AnnVal)> = a.pairs.clone();
            let order = hashmap_order(&vals.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>());
            vals = order.into_iter().map(|i| vals[i].clone()).collect();
            let lone = vals.len() == 1;
            let mut out = format!("@{}(", ty.replace('/', ".").replace('$', "."));
            for (i, (k, v)) in vals.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                if !(lone && k == "value") {
                    out.push_str(k);
                    out.push('=');
                }
                out.push_str(&ann_val_source(v));
            }
            out.push(')');
            out
        }
        AnnVal::Array(_) => {
            let items = match v {
                AnnVal::Array(items) => items,
                _ => unreachable!(),
            };
            let parts: Vec<String> = items.iter().map(ann_val_source).collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

/// `AnnVal::Class` 的规范名（基本类型直接是名字，引用类型是点号形式）
fn ann_val_class_name(v: &AnnVal) -> Option<String> {
    match v {
        AnnVal::Class(d) => Some(super::heap::java_class_name(&internal_of_desc(d))),
        _ => None,
    }
}



/// `AnnotationInvocationHandler.quote(ch, charContext)`：转义一个字符
///
/// 规则（JDK 25 源码逐条对照）：控制字符用 `\b \f \n \r \t`；单引号在字符上下文里转义、
/// 双引号在字符串上下文里转义（反之不转义）；反斜杠一律双写；可打印 ASCII 原样；
/// 其余走 `\uXXXX`。
fn quote_char(ch: u32, char_context: bool) -> String {
    const QUOTE: u32 = 0x27;
    const DQUOTE: u32 = 0x22;
    const BACKSLASH: u32 = 0x5c;
    match ch {
        0x08 => "\u{5c}b".to_string(),
        0x0c => "\u{5c}f".to_string(),
        0x0a => "\u{5c}n".to_string(),
        0x0d => "\u{5c}r".to_string(),
        0x09 => "\u{5c}t".to_string(),
        QUOTE if char_context => "\u{5c}'".to_string(),
        QUOTE => "'".to_string(),
        DQUOTE if !char_context => "\u{5c}\"".to_string(),
        DQUOTE => "\"".to_string(),
        BACKSLASH => "\u{5c}\u{5c}".to_string(),
        0x20..=0x7e => char::from_u32(ch).map(|x| x.to_string()).unwrap_or_default(),
        c => format!("\u{5c}u{:04x}", c),
    }
}

fn cls_generic_signature0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let class = vm.class_obj_name(f.this()).unwrap_or_default();
    let sig = match vm.cf_pub(&class) {
        Some(cf) => annot::signature(&cf, &cf.attributes),
        None => None,
    };
    match sig {
        Some(s) => Ok(Slot::I(vm.str_new(s))),
        None => Ok(Slot::I(0)),
    }
}

impl Vm {
    /// 保证类在类表里（没有就按 classpath 按需加载）。返回 false = 加载不到。
    ///
    /// 反射要造的对象（`Field` / `InvocationTargetException` / 各类异常）不一定出现在
    /// 入口类的常量池闭包里 —— 少了这一步 `new_instance_args` 会报「没有字节码」，
    /// 而那不是错误语义（真 JVM 会链接它）。独立模式（无 classpath）返回 false。
    pub(super) fn ensure_class_loaded(&mut self, name: &str) -> bool {
        if self.store.has_class(name) {
            return true;
        }
        if name.starts_with('[') || name.len() == 1 {
            return false;
        }
        let Some(cp) = self.cp.take() else { return false };
        let r = self.store.load_closure(&cp, name);
        self.cp = Some(cp);
        matches!(r, Ok(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_method_descriptors() {
        assert_eq!(split_desc("()V"), (vec![], "V".to_string()));
        let (p, r) = split_desc("(I[Ljava/lang/String;JD)Ljava/util/List;");
        assert_eq!(p, vec!["I", "[Ljava/lang/String;", "J", "D"]);
        assert_eq!(r, "Ljava/util/List;");
        let (p, r) = split_desc("([[I)V");
        assert_eq!(p, vec!["[[I"]);
        assert_eq!(r, "V");
    }

    #[test]
    fn maps_descriptors_to_class_names() {
        assert_eq!(internal_of_desc("Ljava/lang/String;"), "java/lang/String");
        assert_eq!(internal_of_desc("I"), "I");
        assert_eq!(internal_of_desc("[Ljava/lang/String;"), "[Ljava/lang/String;");
        assert_eq!(prim_of("J"), Some('J'));
        assert_eq!(prim_of("Ljava/lang/Object;"), None);
        assert_eq!(box_of('I'), Some("java/lang/Integer"));
    }

    #[test]
    fn nest_prefix_is_top_level_class() {
        assert_eq!(nest_prefix("pkg/Outer$Inner"), "pkg/Outer");
        assert_eq!(nest_prefix("pkg/Outer"), "pkg/Outer");
    }

    /// `AnnotationType.memberDefaults` 是 `HashMap`：`Annotation.toString` 按它的迭代顺序
    /// 打印成员。这里钉住「桶下标升序、同桶插入顺序」这条规则（顺序错了 toString 就不一致）。
    #[test]
    fn hashmap_iteration_order_matches_java() {
        // 6 个键 → 容量 8；按 (spread & 7) 分桶
        let keys: Vec<String> =
            ["color", "ratio", "sep", "tiny", "small", "nested"].iter().map(|s| s.to_string()).collect();
        let order = hashmap_order(&keys);
        // 顺序必须是 keys 的一个排列，且桶下标单调不减
        let mut seen = vec![false; keys.len()];
        let mut last_bucket = 0usize;
        for i in &order {
            assert!(!seen[*i], "同一个键出现两次");
            seen[*i] = true;
            let h = java_str_hash(&keys[*i]) as u32;
            let bucket = ((h ^ (h >> 16)) as usize) & 7;
            assert!(bucket >= last_bucket, "桶下标必须升序");
            last_bucket = bucket;
        }
        assert!(seen.iter().all(|x| *x));
    }

    #[test]
    fn quotes_annotation_strings_like_jdk() {
        assert_eq!(quote_char(0x5c, false), "\\\\"); // 反斜杠双写
        assert_eq!(quote_char(0x22, false), "\\\""); // 字符串上下文里双引号要转义
        assert_eq!(quote_char(0x22, true), "\""); // 字符上下文里不用
        assert_eq!(quote_char(0x27, true), "\\'"); // 字符上下文里单引号要转义
        assert_eq!(quote_char(0x27, false), "'"); // 字符串上下文里不用
        assert_eq!(quote_char(0x0a, false), "\\n"); // 控制字符用反斜杠转义
        assert_eq!(quote_char(0x01, false), "\\u0001"); // 不可打印 → \uXXXX
        assert_eq!(quote_char('a' as u32, false), "a");
    }
}
