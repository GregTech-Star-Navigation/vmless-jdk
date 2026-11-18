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
    // ================= 以下只能追加 =================
    // ---- Throwable：三个基类各登一份（JIT 的 builtin_vop 不沿继承链走，
    //      所以 `catch (RuntimeException e) { e.getMessage() }` 必须命中 RuntimeException）----
    b("java/lang/Throwable", "getMessage", "()Ljava/lang/String;"),
    b("java/lang/Throwable", "toString", "()Ljava/lang/String;"),
    b("java/lang/Throwable", "printStackTrace", "()V"),
    b("java/lang/Exception", "getMessage", "()Ljava/lang/String;"),
    b("java/lang/Exception", "toString", "()Ljava/lang/String;"),
    b("java/lang/Exception", "printStackTrace", "()V"),
    b("java/lang/RuntimeException", "getMessage", "()Ljava/lang/String;"),
    b("java/lang/RuntimeException", "toString", "()Ljava/lang/String;"),
    b("java/lang/RuntimeException", "printStackTrace", "()V"),
    b("java/lang/Error", "getMessage", "()Ljava/lang/String;"),
    b("java/lang/Error", "toString", "()Ljava/lang/String;"),
    b("java/lang/Error", "printStackTrace", "()V"),
    // 各具体异常类也要**逐个**登记：JIT 的 `builtin_vop` 只看引用类、不爬继承链，
    // 所以 `catch (IllegalStateException e) { e.getMessage() }` 必须能直接命中。
    // （解释器那边有爬链，见 `vm::natives::is_throwable_chain`。）
    b("java/lang/IllegalStateException", "getMessage", "()Ljava/lang/String;"),
    b("java/lang/IllegalStateException", "toString", "()Ljava/lang/String;"),
    b("java/lang/IllegalStateException", "printStackTrace", "()V"),
    b("java/lang/IllegalArgumentException", "getMessage", "()Ljava/lang/String;"),
    b("java/lang/IllegalArgumentException", "toString", "()Ljava/lang/String;"),
    b("java/lang/IllegalArgumentException", "printStackTrace", "()V"),
    b("java/lang/NullPointerException", "getMessage", "()Ljava/lang/String;"),
    b("java/lang/NullPointerException", "toString", "()Ljava/lang/String;"),
    b("java/lang/NullPointerException", "printStackTrace", "()V"),
    b("java/lang/ArithmeticException", "getMessage", "()Ljava/lang/String;"),
    b("java/lang/ArithmeticException", "toString", "()Ljava/lang/String;"),
    b("java/lang/ArithmeticException", "printStackTrace", "()V"),
    b("java/lang/IndexOutOfBoundsException", "getMessage", "()Ljava/lang/String;"),
    b("java/lang/IndexOutOfBoundsException", "toString", "()Ljava/lang/String;"),
    b("java/lang/IndexOutOfBoundsException", "printStackTrace", "()V"),
    b("java/lang/ArrayIndexOutOfBoundsException", "getMessage", "()Ljava/lang/String;"),
    b("java/lang/ArrayIndexOutOfBoundsException", "toString", "()Ljava/lang/String;"),
    b("java/lang/ArrayIndexOutOfBoundsException", "printStackTrace", "()V"),
    b("java/lang/StringIndexOutOfBoundsException", "getMessage", "()Ljava/lang/String;"),
    b("java/lang/StringIndexOutOfBoundsException", "toString", "()Ljava/lang/String;"),
    b("java/lang/StringIndexOutOfBoundsException", "printStackTrace", "()V"),
    b("java/lang/ClassCastException", "getMessage", "()Ljava/lang/String;"),
    b("java/lang/ClassCastException", "toString", "()Ljava/lang/String;"),
    b("java/lang/ClassCastException", "printStackTrace", "()V"),
    b("java/lang/NumberFormatException", "getMessage", "()Ljava/lang/String;"),
    b("java/lang/NumberFormatException", "toString", "()Ljava/lang/String;"),
    b("java/lang/NumberFormatException", "printStackTrace", "()V"),
    b("java/lang/UnsupportedOperationException", "getMessage", "()Ljava/lang/String;"),
    b("java/lang/UnsupportedOperationException", "toString", "()Ljava/lang/String;"),
    b("java/lang/UnsupportedOperationException", "printStackTrace", "()V"),
    b("java/util/NoSuchElementException", "getMessage", "()Ljava/lang/String;"),
    b("java/util/NoSuchElementException", "toString", "()Ljava/lang/String;"),
    b("java/util/NoSuchElementException", "printStackTrace", "()V"),
    b("java/lang/StackOverflowError", "getMessage", "()Ljava/lang/String;"),
    b("java/lang/StackOverflowError", "toString", "()Ljava/lang/String;"),
    b("java/lang/StackOverflowError", "printStackTrace", "()V"),
    b("java/lang/AssertionError", "getMessage", "()Ljava/lang/String;"),
    b("java/lang/AssertionError", "toString", "()Ljava/lang/String;"),
    b("java/lang/AssertionError", "printStackTrace", "()V"),
    // ---- ArrayList（与 List 接口同一套实现；静态类型决定发到哪个 owner）----
    b("java/util/ArrayList", "add", "(Ljava/lang/Object;)Z"),
    b("java/util/ArrayList", "get", "(I)Ljava/lang/Object;"),
    b("java/util/ArrayList", "size", "()I"),
    b("java/util/ArrayList", "isEmpty", "()Z"),
    b("java/util/ArrayList", "remove", "(I)Ljava/lang/Object;"),
    b("java/util/ArrayList", "contains", "(Ljava/lang/Object;)Z"),
    b("java/util/ArrayList", "indexOf", "(Ljava/lang/Object;)I"),
    b("java/util/ArrayList", "clear", "()V"),
    b("java/util/ArrayList", "toArray", "()[Ljava/lang/Object;"),
    b("java/util/ArrayList", "iterator", "()Ljava/util/Iterator;"),
    b("java/util/ArrayList", "toString", "()Ljava/lang/String;"),
    b("java/util/List", "add", "(Ljava/lang/Object;)Z"),
    b("java/util/List", "get", "(I)Ljava/lang/Object;"),
    b("java/util/List", "size", "()I"),
    b("java/util/List", "isEmpty", "()Z"),
    b("java/util/List", "remove", "(I)Ljava/lang/Object;"),
    b("java/util/List", "contains", "(Ljava/lang/Object;)Z"),
    b("java/util/List", "indexOf", "(Ljava/lang/Object;)I"),
    b("java/util/List", "clear", "()V"),
    b("java/util/List", "toArray", "()[Ljava/lang/Object;"),
    b("java/util/List", "iterator", "()Ljava/util/Iterator;"),
    b("java/util/List", "toString", "()Ljava/lang/String;"),
    // ---- HashMap / Map ----
    b("java/util/HashMap", "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"),
    b("java/util/HashMap", "get", "(Ljava/lang/Object;)Ljava/lang/Object;"),
    b("java/util/HashMap", "containsKey", "(Ljava/lang/Object;)Z"),
    b("java/util/HashMap", "remove", "(Ljava/lang/Object;)Ljava/lang/Object;"),
    b("java/util/HashMap", "size", "()I"),
    b("java/util/HashMap", "isEmpty", "()Z"),
    b("java/util/HashMap", "clear", "()V"),
    b("java/util/HashMap", "keySet", "()Ljava/util/Set;"),
    b("java/util/HashMap", "values", "()Ljava/util/Collection;"),
    b("java/util/HashMap", "entrySet", "()Ljava/util/Set;"),
    b("java/util/HashMap", "toString", "()Ljava/lang/String;"),
    b("java/util/Map", "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"),
    b("java/util/Map", "get", "(Ljava/lang/Object;)Ljava/lang/Object;"),
    b("java/util/Map", "containsKey", "(Ljava/lang/Object;)Z"),
    b("java/util/Map", "remove", "(Ljava/lang/Object;)Ljava/lang/Object;"),
    b("java/util/Map", "size", "()I"),
    b("java/util/Map", "isEmpty", "()Z"),
    b("java/util/Map", "clear", "()V"),
    b("java/util/Map", "keySet", "()Ljava/util/Set;"),
    b("java/util/Map", "values", "()Ljava/util/Collection;"),
    b("java/util/Map", "entrySet", "()Ljava/util/Set;"),
    b("java/util/Map", "toString", "()Ljava/lang/String;"),
    // ---- keySet()/values()/entrySet() 返回的视图（同一套实现）----
    b("java/util/Set", "iterator", "()Ljava/util/Iterator;"),
    b("java/util/Set", "size", "()I"),
    b("java/util/Set", "isEmpty", "()Z"),
    b("java/util/Set", "contains", "(Ljava/lang/Object;)Z"),
    b("java/util/Set", "toString", "()Ljava/lang/String;"),
    b("java/util/Collection", "iterator", "()Ljava/util/Iterator;"),
    b("java/util/Collection", "size", "()I"),
    b("java/util/Collection", "isEmpty", "()Z"),
    b("java/util/Collection", "contains", "(Ljava/lang/Object;)Z"),
    b("java/util/Collection", "toString", "()Ljava/lang/String;"),
    // ---- Map.Entry（entrySet 的元素）----
    b("java/util/Map$Entry", "getKey", "()Ljava/lang/Object;"),
    b("java/util/Map$Entry", "getValue", "()Ljava/lang/Object;"),
    b("java/util/Map$Entry", "toString", "()Ljava/lang/String;"),
    // 真字节码模式下 `getClass` 是 native（native_reg 里有）；独立模式下 `java.*`
    // 没有字节码，只能靠这条内建兜住 —— lambda 的 `getClass()` 就落在这里
    b("java/lang/Object", "getClass", "()Ljava/lang/Class;"),
    // `Class.getName()`：独立模式下 `java.lang.Class` 没有字节码，而它是 lambda
    // 自省（`obj.getClass().getName()`）的必经一步
    b("java/lang/Class", "getName", "()Ljava/lang/String;"),
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
    // ================= 以下只能追加 =================
    // ---- String.valueOf：8 个重载 ----
    b("java/lang/String", "valueOf", "(Ljava/lang/Object;)Ljava/lang/String;"),
    b("java/lang/String", "valueOf", "(Z)Ljava/lang/String;"),
    b("java/lang/String", "valueOf", "(C)Ljava/lang/String;"),
    b("java/lang/String", "valueOf", "(I)Ljava/lang/String;"),
    b("java/lang/String", "valueOf", "(J)Ljava/lang/String;"),
    b("java/lang/String", "valueOf", "(F)Ljava/lang/String;"),
    b("java/lang/String", "valueOf", "(D)Ljava/lang/String;"),
    b("java/lang/String", "valueOf", "([C)Ljava/lang/String;"),
    // ---- String.format（可变参数由调用点打包成 Object[]）----
    b("java/lang/String", "format", "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/String;"),
    // ---- Math ----
    b("java/lang/Math", "abs", "(I)I"),
    b("java/lang/Math", "abs", "(J)J"),
    b("java/lang/Math", "abs", "(D)D"),
    b("java/lang/Math", "abs", "(F)F"),
    b("java/lang/Math", "max", "(II)I"),
    b("java/lang/Math", "max", "(JJ)J"),
    b("java/lang/Math", "max", "(DD)D"),
    b("java/lang/Math", "min", "(II)I"),
    b("java/lang/Math", "min", "(JJ)J"),
    b("java/lang/Math", "min", "(DD)D"),
    b("java/lang/Math", "sqrt", "(D)D"),
    b("java/lang/Math", "pow", "(DD)D"),
    b("java/lang/Math", "floor", "(D)D"),
    b("java/lang/Math", "ceil", "(D)D"),
    b("java/lang/Math", "round", "(D)J"),
    b("java/lang/Math", "round", "(F)I"),
    b("java/lang/Math", "signum", "(D)D"),
    // 固定种子 LCG（与 HotSpot 的流不同，**不能进三路一致的样例**）
    b("java/lang/Math", "random", "()D"),
    // ---- Integer：进制转换与比较 ----
    b("java/lang/Integer", "toHexString", "(I)Ljava/lang/String;"),
    b("java/lang/Integer", "toBinaryString", "(I)Ljava/lang/String;"),
    b("java/lang/Integer", "toOctalString", "(I)Ljava/lang/String;"),
    b("java/lang/Integer", "compare", "(II)I"),
    b("java/lang/Long", "toHexString", "(J)Ljava/lang/String;"),
    b("java/lang/Long", "compare", "(JJ)I"),
    // ---- Character：字符分类（ASCII 范围内与 HotSpot 一致）----
    b("java/lang/Character", "isDigit", "(C)Z"),
    b("java/lang/Character", "isLetter", "(C)Z"),
    b("java/lang/Character", "isLetterOrDigit", "(C)Z"),
    b("java/lang/Character", "isUpperCase", "(C)Z"),
    b("java/lang/Character", "isLowerCase", "(C)Z"),
    b("java/lang/Character", "isWhitespace", "(C)Z"),
    b("java/lang/Character", "toUpperCase", "(C)C"),
    b("java/lang/Character", "toLowerCase", "(C)C"),
    b("java/lang/Character", "getNumericValue", "(C)I"),
    // ---- Arrays：每种元素类型各登一条 ----
    // 重载挑选按「精确描述符命中数」打分，形状相同的候选里只有**描述符完全一致**
    // 的那条能拿到分；所以 `String[]` 这类必须有自己的条目，否则会退化成 `[I`。
    b("java/util/Arrays", "toString", "([Ljava/lang/Object;)Ljava/lang/String;"),
    b("java/util/Arrays", "toString", "([J)Ljava/lang/String;"),
    b("java/util/Arrays", "toString", "([D)Ljava/lang/String;"),
    b("java/util/Arrays", "toString", "([Z)Ljava/lang/String;"),
    b("java/util/Arrays", "toString", "([C)Ljava/lang/String;"),
    b("java/util/Arrays", "fill", "([II)V"),
    b("java/util/Arrays", "fill", "([J)V"),
    b("java/util/Arrays", "fill", "([DD)V"),
    b("java/util/Arrays", "fill", "([ZZ)V"),
    b("java/util/Arrays", "fill", "([CC)V"),
    b("java/util/Arrays", "fill", "([Ljava/lang/Object;Ljava/lang/Object;)V"),
    b("java/util/Arrays", "sort", "([Ljava/lang/Object;)V"),
    b("java/util/Arrays", "sort", "([D)V"),
    b("java/util/Arrays", "sort", "([J)V"),
    b("java/util/Arrays", "equals", "([I[I)Z"),
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
    // 基本类型的 `Class` 对象：`MethodType.methodType(int.class, …)` 在字节码里是
    // `getstatic Integer.TYPE`，没有这几条独立模式拿到的就是 null
    ("java/lang/Integer", "TYPE", "Ljava/lang/Class;"),
    ("java/lang/Long", "TYPE", "Ljava/lang/Class;"),
    ("java/lang/Float", "TYPE", "Ljava/lang/Class;"),
    ("java/lang/Double", "TYPE", "Ljava/lang/Class;"),
    ("java/lang/Boolean", "TYPE", "Ljava/lang/Class;"),
    ("java/lang/Character", "TYPE", "Ljava/lang/Class;"),
    ("java/lang/Byte", "TYPE", "Ljava/lang/Class;"),
    ("java/lang/Short", "TYPE", "Ljava/lang/Class;"),
    ("java/lang/Void", "TYPE", "Ljava/lang/Class;"),
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
        ("getClass", 0) => Some("()Ljava/lang/Class;".to_string()),
        // 数组的 clone()：JVM 上数组的 clone 走 Object.clone 但返回类型是数组本身，
        // javac 也把它当 `()Ljava/lang/Object;` 处理（调用点再 checkcast）
        ("clone", 0) => Some("()Ljava/lang/Object;".to_string()),
        _ => None,
    }
}
