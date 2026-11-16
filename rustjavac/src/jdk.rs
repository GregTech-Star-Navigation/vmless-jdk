//! 内建 JDK 类的方法/字段表 —— 编译器、解释器、JIT 三处共用。
//!
//! `java.lang.String` / 五个包装类 / `java.lang.Object` / `java.lang.System` /
//! `java.util.Arrays` 没有 `.class` 在 classpath 上，所以：
//!
//! - **编译器**（`codegen::types`）靠这张表拿描述符（`s.length()` 该发什么签名）；
//! - **解释器**（`vm::natives`）与 **JIT**（`jit::rt`）靠**下标**即 op 号分发实现。
//!
//! 加一个方法 = 这里加一行 + 两个运行时各加一个分支。下标必须稳定：
//! `VIRTUALS` / `STATICS` 只能**追加**，中间插行会让已编译的 op 号错位。

/// 一个内建方法
pub struct B {
    pub class: &'static str,
    pub name: &'static str,
    pub desc: &'static str,
}

const fn b(class: &'static str, name: &'static str, desc: &'static str) -> B {
    B { class, name, desc }
}

/// 包装类 → 基本类型描述符
const WRAPPERS: &[(&str, &str)] = &[
    ("java/lang/Integer", "I"),
    ("java/lang/Long", "J"),
    ("java/lang/Double", "D"),
    ("java/lang/Boolean", "Z"),
    ("java/lang/Character", "C"),
];

/// 基本类型描述符 → 对应的拆箱方法名
pub fn unbox_method(prim: &str) -> &'static str {
    match prim {
        "I" => "intValue",
        "J" => "longValue",
        "D" => "doubleValue",
        "Z" => "booleanValue",
        "C" => "charValue",
        _ => "intValue",
    }
}

/// 类内部名是不是包装类 → 它的基本类型描述符
pub fn wrapper_prim(class: &str) -> Option<&'static str> {
    WRAPPERS.iter().find(|(c, _)| *c == class).map(|(_, p)| *p)
}

/// 基本类型描述符 → 包装类内部名（`I` → `java/lang/Integer`）
pub fn box_class(prim: &str) -> Option<&'static str> {
    WRAPPERS.iter().find(|(_, p)| *p == prim).map(|(c, _)| *c)
}

/// 虚方法表（下标 = JIT/解释器的 op 号）
pub const VIRTUALS: &[B] = &[
    // ---- java.lang.String ----
    b("java/lang/String", "length", "()I"),
    b("java/lang/String", "charAt", "(I)C"),
    b("java/lang/String", "substring", "(I)Ljava/lang/String;"),
    b("java/lang/String", "substring", "(II)Ljava/lang/String;"),
    b("java/lang/String", "indexOf", "(Ljava/lang/String;)I"),
    b("java/lang/String", "indexOf", "(I)I"),
    b("java/lang/String", "indexOf", "(Ljava/lang/String;I)I"),
    b("java/lang/String", "contains", "(Ljava/lang/CharSequence;)Z"),
    b("java/lang/String", "startsWith", "(Ljava/lang/String;)Z"),
    b("java/lang/String", "endsWith", "(Ljava/lang/String;)Z"),
    b("java/lang/String", "toUpperCase", "()Ljava/lang/String;"),
    b("java/lang/String", "toLowerCase", "()Ljava/lang/String;"),
    b("java/lang/String", "trim", "()Ljava/lang/String;"),
    b("java/lang/String", "replace", "(CC)Ljava/lang/String;"),
    b(
        "java/lang/String",
        "replace",
        "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Ljava/lang/String;",
    ),
    b("java/lang/String", "isEmpty", "()Z"),
    b("java/lang/String", "compareTo", "(Ljava/lang/String;)I"),
    b("java/lang/String", "concat", "(Ljava/lang/String;)Ljava/lang/String;"),
    b("java/lang/String", "split", "(Ljava/lang/String;)[Ljava/lang/String;"),
    b("java/lang/String", "equals", "(Ljava/lang/Object;)Z"),
    b("java/lang/String", "hashCode", "()I"),
    b("java/lang/String", "toString", "()Ljava/lang/String;"),
    // ---- java.lang.Integer ----
    b("java/lang/Integer", "intValue", "()I"),
    b("java/lang/Integer", "longValue", "()J"),
    b("java/lang/Integer", "doubleValue", "()D"),
    b("java/lang/Integer", "toString", "()Ljava/lang/String;"),
    b("java/lang/Integer", "equals", "(Ljava/lang/Object;)Z"),
    b("java/lang/Integer", "hashCode", "()I"),
    // ---- java.lang.Long ----
    b("java/lang/Long", "longValue", "()J"),
    b("java/lang/Long", "intValue", "()I"),
    b("java/lang/Long", "doubleValue", "()D"),
    b("java/lang/Long", "toString", "()Ljava/lang/String;"),
    b("java/lang/Long", "equals", "(Ljava/lang/Object;)Z"),
    b("java/lang/Long", "hashCode", "()I"),
    // ---- java.lang.Double ----
    b("java/lang/Double", "doubleValue", "()D"),
    b("java/lang/Double", "intValue", "()I"),
    b("java/lang/Double", "longValue", "()J"),
    b("java/lang/Double", "toString", "()Ljava/lang/String;"),
    b("java/lang/Double", "equals", "(Ljava/lang/Object;)Z"),
    b("java/lang/Double", "hashCode", "()I"),
    // ---- java.lang.Boolean ----
    b("java/lang/Boolean", "booleanValue", "()Z"),
    b("java/lang/Boolean", "toString", "()Ljava/lang/String;"),
    b("java/lang/Boolean", "equals", "(Ljava/lang/Object;)Z"),
    b("java/lang/Boolean", "hashCode", "()I"),
    // ---- java.lang.Character ----
    b("java/lang/Character", "charValue", "()C"),
    b("java/lang/Character", "intValue", "()I"),
    b("java/lang/Character", "toString", "()Ljava/lang/String;"),
    b("java/lang/Character", "equals", "(Ljava/lang/Object;)Z"),
    b("java/lang/Character", "hashCode", "()I"),
    // ---- java.lang.StringBuilder（显式 new 出来的那些；拼接近径见 codegen::expr::emit_concat）----
    b("java/lang/StringBuilder", "append", "(Ljava/lang/String;)Ljava/lang/StringBuilder;"),
    b("java/lang/StringBuilder", "append", "(Ljava/lang/Object;)Ljava/lang/StringBuilder;"),
    b("java/lang/StringBuilder", "append", "(I)Ljava/lang/StringBuilder;"),
    b("java/lang/StringBuilder", "append", "(J)Ljava/lang/StringBuilder;"),
    b("java/lang/StringBuilder", "append", "(D)Ljava/lang/StringBuilder;"),
    b("java/lang/StringBuilder", "append", "(Z)Ljava/lang/StringBuilder;"),
    b("java/lang/StringBuilder", "append", "(C)Ljava/lang/StringBuilder;"),
    b("java/lang/StringBuilder", "toString", "()Ljava/lang/String;"),
    // ---- java.lang.Object：所有类都继承的三个协议方法 ----
    b("java/lang/Object", "hashCode", "()I"),
    b("java/lang/Object", "equals", "(Ljava/lang/Object;)Z"),
    b("java/lang/Object", "toString", "()Ljava/lang/String;"),
];

/// 静态方法表（下标 = op 号）
pub const STATICS: &[B] = &[
    b("java/lang/Integer", "parseInt", "(Ljava/lang/String;)I"),
    b("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;"),
    b("java/lang/Integer", "toString", "(I)Ljava/lang/String;"),
    b("java/lang/Long", "valueOf", "(J)Ljava/lang/Long;"),
    b("java/lang/Long", "parseLong", "(Ljava/lang/String;)J"),
    b("java/lang/Long", "toString", "(J)Ljava/lang/String;"),
    b("java/lang/Double", "valueOf", "(D)Ljava/lang/Double;"),
    b("java/lang/Double", "parseDouble", "(Ljava/lang/String;)D"),
    b("java/lang/Double", "toString", "(D)Ljava/lang/String;"),
    b("java/lang/Boolean", "valueOf", "(Z)Ljava/lang/Boolean;"),
    b("java/lang/Character", "valueOf", "(C)Ljava/lang/Character;"),
    b(
        "java/lang/System",
        "arraycopy",
        "(Ljava/lang/Object;ILjava/lang/Object;II)V",
    ),
    b("java/lang/System", "currentTimeMillis", "()J"),
    b("java/util/Arrays", "toString", "([I)Ljava/lang/String;"),
    b("java/util/Arrays", "sort", "([I)V"),
    b("java/lang/System", "nanoTime", "()J"),
];

/// 静态字段：(类, 字段名, 描述符)
pub const FIELDS: &[(&str, &str, &str)] = &[
    ("java/lang/Integer", "MAX_VALUE", "I"),
    ("java/lang/Integer", "MIN_VALUE", "I"),
    ("java/lang/Long", "MAX_VALUE", "J"),
    ("java/lang/Long", "MIN_VALUE", "J"),
    ("java/lang/Double", "MAX_VALUE", "D"),
    ("java/lang/Double", "MIN_VALUE", "D"),
    ("java/lang/Double", "NaN", "D"),
    ("java/lang/Character", "MAX_VALUE", "C"),
    ("java/lang/Character", "MIN_VALUE", "C"),
    ("java/lang/Boolean", "TRUE", "Ljava/lang/Boolean;"),
    ("java/lang/Boolean", "FALSE", "Ljava/lang/Boolean;"),
    // float 的边界值：验 `println(float)` 的十进制形态要用（**只能追加**，下标即 op 号）
    ("java/lang/Float", "MAX_VALUE", "F"),
    ("java/lang/Float", "MIN_VALUE", "F"),
    ("java/lang/Float", "NaN", "F"),
];

/// 虚方法的 op 号（没登记就是 None）
pub fn vop(class: &str, name: &str, desc: &str) -> Option<u32> {
    VIRTUALS
        .iter()
        .position(|x| x.class == class && x.name == name && x.desc == desc)
        .map(|i| i as u32)
}

/// 静态方法的 op 号
pub fn sop(class: &str, name: &str, desc: &str) -> Option<u32> {
    STATICS
        .iter()
        .position(|x| x.class == class && x.name == name && x.desc == desc)
        .map(|i| i as u32)
}

/// 静态字段的描述符
pub fn field_desc(class: &str, name: &str) -> Option<&'static str> {
    FIELDS.iter().find(|(c, f, _)| *c == class && *f == name).map(|(_, _, d)| *d)
}

/// `(类, 方法名)` 的全部候选（重载靠调用点按实参挑）
pub fn virtuals_of<'a>(class: &'a str, name: &'a str) -> impl Iterator<Item = &'static B> + 'a {
    VIRTUALS.iter().filter(move |x| x.class == class && x.name == name)
}

/// `(类, 方法名)` 的静态方法候选
pub fn statics_of<'a>(class: &'a str, name: &'a str) -> impl Iterator<Item = &'static B> + 'a {
    STATICS.iter().filter(move |x| x.class == class && x.name == name)
}

/// `java.lang.Object` 的协议方法描述符（`toString` / `equals` / `hashCode`）。
///
/// 任何类都继承这三个方法，但我们的 `Object` 没有 `.class`：调用点解析不到
/// 具体实现时要靠它拿到签名（运行时再按「有没有覆写」决定走虚分派还是内建）。
pub fn object_protocol(name: &str, argc: usize) -> Option<String> {
    match (name, argc) {
        ("toString", 0) => Some("()Ljava/lang/String;".to_string()),
        ("hashCode", 0) => Some("()I".to_string()),
        ("equals", 1) => Some("(Ljava/lang/Object;)Z".to_string()),
        _ => None,
    }
}
