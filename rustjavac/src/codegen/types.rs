//! 类型种类、描述符、验证类型与表达式类型推断。

use crate::ast::*;

use super::constpool::CpBuilder;
use super::ctx::Ctx;
use super::emit::Emit;
use super::error::*;

// 局部变量 / 表达式的类型种类
pub(crate) const K_INT: u8 = 0;
pub(crate) const K_LONG: u8 = 1;
pub(crate) const K_REF: u8 = 2;
pub(crate) const K_DOUBLE: u8 = 3;
/// boolean：JVM 里与 int 同一套 load/store/算术字节码，只有 println 的重载不同
/// （println(Z) 打 "true"/"false"）
pub(crate) const K_BOOL: u8 = 4;

pub(crate) fn kind_wide(k: u8) -> bool {
    k == K_LONG || k == K_DOUBLE
}

/// 类型 → 种类
pub(crate) fn kind_of(ty: &Type) -> u8 {
    match ty {
        Type::Prim("long") => K_LONG,
        Type::Prim("double") => K_DOUBLE,
        Type::Prim("boolean") => K_BOOL,
        Type::Prim(_) => K_INT,
        _ => K_REF,
    }
}

/// 按描述符首字符判断种类（字段、方法返回值用）
pub(crate) fn kind_from_desc(desc: &str) -> u8 {
    match desc.as_bytes().first().copied() {
        Some(b'J') => K_LONG,
        Some(b'D') => K_DOUBLE,
        Some(b'Z') => K_BOOL,
        Some(b'L') | Some(b'[') => K_REF,
        _ => K_INT,
    }
}

/// 二元/比较运算的公共类型：double > long > int
pub(crate) fn promote(a: u8, b: u8) -> u8 {
    if a == K_DOUBLE || b == K_DOUBLE {
        K_DOUBLE
    } else if a == K_LONG || b == K_LONG {
        K_LONG
    } else {
        K_INT
    }
}

/// 按描述符的返回值判断种类（用于 Call 表达式）
///
/// 必须看**返回类型那一段**（`)` 之后）：直接看描述符最后一个字符会把
/// `()Ljava/lang/String;` 的 `;` 当成 int。
pub(crate) fn kind_from_ret(desc: &str) -> u8 {
    let ret = desc.rsplit(')').next().unwrap_or("");
    kind_from_desc(ret)
}



pub(crate) const JAVA_LANG: &[&str] = &["String", "Object", "System", "Integer", "Long", "Short", "Byte", "Character",
    "Boolean", "Float", "Double", "Number", "Class", "Throwable", "Exception",
    "RuntimeException", "Error", "Math", "StringBuilder", "Iterable", "Comparable",
    "Runnable", "Thread", "Enum", "Record", "Void",
    // 异常体系：`catch (ArrayIndexOutOfBoundsException e)` 这类简写也要补成 java/lang/
    "ArithmeticException", "ArrayIndexOutOfBoundsException", "IndexOutOfBoundsException",
    "StringIndexOutOfBoundsException", "NullPointerException", "ClassCastException",
    "NegativeArraySizeException", "IllegalArgumentException", "IllegalStateException",
    "UnsupportedOperationException", "NumberFormatException", "CloneNotSupportedException",
    "InterruptedException", "NoSuchElementException", "StackOverflowError", "AutoCloseable", "Closeable", "Cloneable",
    "OutOfMemoryError", "AssertionError",
];

pub(crate) fn is_java_lang(name: &str) -> bool {
    JAVA_LANG.contains(&name)
}

/// 类型路径 → 内部类名（`RuntimeException` 这类 java.lang 简写补全）
pub(crate) fn internal_name(path: &[String]) -> String {
    if path.len() == 1 && is_java_lang(&path[0]) {
        format!("java/lang/{}", path[0])
    } else {
        path.join("/")
    }
}

/// 类型 → 描述符
pub fn descriptor(ty: &Type) -> String {
    match ty {
        Type::Prim("int") => "I".into(),
        Type::Prim("long") => "J".into(),
        Type::Prim("short") => "S".into(),
        Type::Prim("byte") => "B".into(),
        Type::Prim("char") => "C".into(),
        Type::Prim("boolean") => "Z".into(),
        Type::Prim("float") => "F".into(),
        Type::Prim("double") => "D".into(),
        Type::Prim("void") => "V".into(),
        Type::Prim(other) => format!("<{other}>"),
        Type::Named { path, .. } => {
            // java.lang 的类型可以简写，编译器要补全
            let joined = if path.len() == 1 && is_java_lang(&path[0]) {
                format!("java/lang/{}", path[0])
            } else {
                path.join("/")
            };
            format!("L{joined};")
        }
        Type::Array(inner) => format!("[{}", descriptor(inner)),
        Type::Var => "I".into(),
    }
}

/// 类型是不是 `String`（`java.lang.String` 简写成 `String` 也算）
pub(crate) fn is_string_type(ty: &Type) -> bool {
    matches!(ty, Type::Named { path, .. } if path.last().map(|s| s.as_str()) == Some("String"))
}

/// 表达式作为**值**是不是 String：字符串拼接的判定就靠它
/// （Java 的规则：`+` 任意一侧是 String 就是拼接，左结合的）
pub(crate) fn is_string_val(e: &Expr, ctx: &Ctx) -> bool {
    match e {
        Expr::StrLit(_) | Expr::TextBlock(_) => true,
        Expr::Binary { op: BinOp::Add, lhs, rhs } => is_string_val(lhs, ctx) || is_string_val(rhs, ctx),
        Expr::Cond { then, els, .. } => is_string_val(then, ctx) || is_string_val(els, ctx),
        Expr::Cast { ty, .. } => is_string_type(ty),
        _ => expr_type(e, ctx).map(|t| is_string_type(&t)).unwrap_or(false),
    }
}

/// 描述符 → 参数占的**槽数**（long/double 各占 2 槽；不含接收者）。
///
/// 栈深记账必须用它而不是参数个数：`f(true, 7, 10000000000L)` 是 3 个参数、
/// 4 个槽，按参数个数弹会把深度永久算少 1，后面任何一个「比较结果当值」的
/// `bool_from_branch` 就会误判「栈不空」而报错。
pub(crate) fn desc_slots(desc: &str) -> u16 {
    let bytes = desc.as_bytes();
    let mut i = 1;
    let mut n = 0u16;
    while i < bytes.len() && bytes[i] != b')' {
        match bytes[i] {
            b'[' => {
                while i < bytes.len() && bytes[i] == b'[' {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'L' {
                    while i < bytes.len() && bytes[i] != b';' {
                        i += 1;
                    }
                }
                i += 1;
                n += 1;
            }
            b'L' => {
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1;
                n += 1;
            }
            b'J' | b'D' => {
                i += 1;
                n += 2;
            }
            _ => {
                i += 1;
                n += 1;
            }
        }
    }
    n
}

/// 描述符 → 形参描述符列表（`(ILjava/lang/String;)V` → `["I", "Ljava/lang/String;"]`）。
///
/// 可变参数打包与实参的隐式 checkcast 都按形参描述符逐位对齐，所以要能把参数段切开。
pub(crate) fn desc_params(desc: &str) -> Vec<String> {
    let bytes = desc.as_bytes();
    let mut i = if bytes.first() == Some(&b'(') { 1 } else { 0 };
    let mut out = Vec::new();
    while i < bytes.len() && bytes[i] != b')' {
        let start = i;
        while i < bytes.len() && bytes[i] == b'[' {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'L' {
            while i < bytes.len() && bytes[i] != b';' {
                i += 1;
            }
            i += 1;
        } else {
            i += 1;
        }
        out.push(desc[start..i.min(bytes.len())].to_string());
    }
    out
}

/// 表达式的**装箱前**类型：基本类型描述符（`I`/`J`/`D`/`Z`/`C`）。
///
/// 包装类（`Integer` 等）当作它的基本类型看 —— 数值上下文里要先拆箱。
pub(crate) fn unwrap_prim(t: &Type) -> Option<&'static str> {
    match t {
        Type::Prim("int") => Some("I"),
        Type::Prim("long") => Some("J"),
        Type::Prim("double") => Some("D"),
        Type::Prim("boolean") => Some("Z"),
        Type::Prim("char") => Some("C"),
        Type::Prim("short") => Some("S"),
        Type::Prim("byte") => Some("B"),
        Type::Prim("float") => Some("F"),
        Type::Named { path, .. } => crate::jdk::wrapper_prim(&internal_name(path)),
        _ => None,
    }
}

/// 表达式静态类型是不是包装类（`Integer` / `Long` / …）
pub(crate) fn is_wrapper_ty(t: &Type) -> bool {
    matches!(t, Type::Named { path, .. } if crate::jdk::wrapper_prim(&internal_name(path)).is_some())
}

/// 数算上下文里的种类：包装类按它的基本类型算（`Integer + int` 就是 int 加法）。
///
/// 其余一律用 `expr_kind`（字节码层面的种类）—— `expr_type` 对三元这类表达式
/// 只是「占位」（`c ? int : long` 给的是 int），拿它做数值提升会多插一条 i2l。
pub(crate) fn num_kind(e: &Expr, ctx: &Ctx) -> u8 {
    if let Some(t) = expr_type(e, ctx) {
        if is_wrapper_ty(&t) {
            return kind_from_desc(unwrap_prim(&t).unwrap_or("I"));
        }
    }
    expr_kind(e, ctx)
}

/// 编一个**数值上下文**里的操作数：包装类先拆箱（`invokevirtual intValue()I`）。
///
/// 返回它实际压在栈上的种类（已拆箱的按基本类型算）。
pub(crate) fn emit_num(
    e: &Expr,
    em: &mut Emit,
    ctx: &Ctx,
    cp: &mut CpBuilder,
) -> Result<u8, CompileError> {
    let wrapper = expr_type(e, ctx).filter(|t| is_wrapper_ty(t));
    super::expr::compile_expr(e, em, ctx, cp)?;
    let Some(ty) = wrapper else {
        return Ok(num_kind(e, ctx));
    };
    let owner = type_internal(&ty).unwrap_or_else(|| "java/lang/Integer".into());
    let prim = crate::jdk::wrapper_prim(&owner).unwrap_or("I");
    let desc = format!("(){}", prim);
    let idx = cp.method_ref(&owner, crate::jdk::unbox_method(prim), &desc);
    em.op_u2(0xB6, idx); // invokevirtual
    em.pop(1);
    em.push(1);
    Ok(kind_from_desc(prim))
}

/// 数值上下文的实参：可能已经被 `spill` 进了脏槽（表达式自带跳转时）。
///
/// 走脏槽那条路要先取回引用再拆箱 —— 直接 `emit_num` 会把表达式算第二遍。
pub(crate) fn emit_num_val(
    a: &Expr,
    sp: &Option<super::expr::Spilled>,
    em: &mut Emit,
    ctx: &Ctx,
    cp: &mut CpBuilder,
) -> Result<u8, CompileError> {
    let Some(s) = sp else { return emit_num(a, em, ctx, cp) };
    super::expr::unspill(s, em)?;
    let Some(ty) = expr_type(a, ctx).filter(|t| is_wrapper_ty(t)) else {
        return Ok(num_kind(a, ctx));
    };
    let owner = type_internal(&ty).unwrap_or_else(|| "java/lang/Integer".into());
    let prim = emit_unbox_of(&owner, em, cp)?;
    Ok(kind_from_desc(prim))
}

/// 装箱：把一个基本类型的值包成包装类对象（`invokestatic Integer.valueOf(I)`）。
///
/// 只在栈上已经压了那个基本类型的值时调用（压栈 1 槽，出栈 1 槽）。
pub(crate) fn emit_box(
    prim: &str,
    em: &mut Emit,
    cp: &mut CpBuilder,
) -> Result<(), CompileError> {
    let Some(cls) = crate::jdk::box_class(prim) else { return Ok(()) };
    let desc = format!("({prim})L{cls};");
    let idx = cp.method_ref(cls, "valueOf", &desc);
    em.op_u2(0xB8, idx); // invokestatic
    // 参数按槽数弹：long/double 占 2 槽
    let wide = prim == "J" || prim == "D";
    if wide {
        em.pop(2);
        em.push(1);
    }
    Ok(())
}

/// 拆箱：栈顶是包装类引用 → 基本类型的值（`invokevirtual intValue()I`）。
pub(crate) fn emit_unbox_of(
    owner: &str,
    em: &mut Emit,
    cp: &mut CpBuilder,
) -> Result<&'static str, CompileError> {
    let prim = crate::jdk::wrapper_prim(owner).unwrap_or("I");
    let desc = format!("(){}", prim);
    let idx = cp.method_ref(owner, crate::jdk::unbox_method(prim), &desc);
    em.op_u2(0xB6, idx); // invokevirtual
    em.pop(1);
    if prim == "J" || prim == "D" {
        em.push(2);
    } else {
        em.push(1);
    }
    Ok(prim)
}

/// 赋值/传参/返回处的隐式转换：数值拓宽收窄、泛型擦除后的 checkcast，
/// 以及**自动装箱 / 拆箱**（Java 的 boxing conversion）。
///
/// 关键的一条：`int n = box.get()`（泛型擦除后 `get()` 返回 `Object`）要编成
/// 「checkcast Integer + intValue()」—— 只压一个引用再 `istore` 会 VerifyError。
pub(crate) fn coerce_to(
    target: &Type,
    e: &Expr,
    ctx: &Ctx,
    em: &mut Emit,
    cp: &mut CpBuilder,
) -> Result<(), CompileError> {
    let tk = kind_of(target);
    let src = expr_type(e, ctx);
    // ---- 1) 目标是包装类、值是基本类型 → 装箱 ----
    if is_wrapper_ty(target) {
        if let Some(p) = src.as_ref().and_then(unwrap_prim).filter(|d| !d.starts_with('L')) {
            if matches!(src.as_ref(), Some(Type::Prim(_))) {
                return emit_box(p, em, cp);
            }
        }
        return Ok(());
    }
    // ---- 2) 目标是引用类型 ----
    if tk == K_REF {
        // 值是基本类型（`Object o = 5;` / `List.add(5)`）→ 装箱
        if let Some(t) = src.as_ref() {
            if kind_of(t) != K_REF {
                return emit_box(unwrap_prim(t).unwrap_or("I"), em, cp);
            }
        }
        if descriptor(target) == "Ljava/lang/Object;" {
            return Ok(());
        }
        let Some(src) = src else { return Ok(()) };
        if descriptor(&src) != "Ljava/lang/Object;" {
            return Ok(());
        }
        let cls = ver_type(cp, target);
        em.op_u2(0xC0, cls); // checkcast
        return Ok(());
    }
    // ---- 3) 目标是基本类型 ----
    let prim = match target {
        Type::Prim(p) => unwrap_prim(&Type::Prim(p)).unwrap_or("I"),
        _ => "I",
    };
    // 值是包装类 → 直接拆箱
    if matches!(src.as_ref(), Some(t) if is_wrapper_ty(t)) {
        let owner = crate::jdk::box_class(prim).unwrap_or("java/lang/Integer");
        let cls = cp.class(owner);
        em.op_u2(0xC0, cls); // checkcast 包装类
        emit_unbox_of(owner, em, cp)?;
        return Ok(());
    }
    // 栈上是引用（泛型擦除后的 Object）→ checkcast 包装类再拆箱
    if expr_kind(e, ctx) == K_REF {
        let owner = crate::jdk::box_class(prim).unwrap_or("java/lang/Integer");
        let cls = cp.class(owner);
        em.op_u2(0xC0, cls); // checkcast
        emit_unbox_of(owner, em, cp)?;
        return Ok(());
    }
    // 基本类型：按**字节码层面的种类**拓宽/收窄（不用 expr_type：三元只是占位）
    coerce(expr_kind(e, ctx), tk, em)
}

pub(crate) fn method_descriptor(params: &[Param], ret: &Option<Type>) -> String {
    let mut s = String::from("(");
    for p in params {
        s.push_str(&descriptor(&p.ty));
    }
    s.push(')');
    s.push_str(&descriptor(ret.as_ref().unwrap_or(&Type::Prim("void"))));
    s
}


/// 类型的验证类型编码：int → 0，对象/数组 → 常量池里的类索引
pub(crate) fn ver_type(cp: &mut CpBuilder, ty: &Type) -> u16 {
    match ty {
        Type::Prim("long") => 0xFFFE, // 栈图里要写 ITEM_Long
        Type::Prim("double") => 0xFFFD, // 栈图里要写 ITEM_Double
        Type::Prim(_) => 0,
        Type::Array(inner) => {
            let d = descriptor(&Type::Array(inner.clone()));
            cp.class(&d)
        }
        Type::Named { path, .. } => {
            let joined = if path.len() == 1 && is_java_lang(&path[0]) {
                format!("java/lang/{}", path[0])
            } else {
                path.join("/")
            };
            cp.class(&joined)
        }
        Type::Var => 0,
    }
}

/// 把栈顶值从 `from` 种类转换成 `to` 种类（只做数值拓宽/收窄）
pub(crate) fn coerce(from: u8, to: u8, em: &mut Emit) -> Result<(), CompileError> {
    if from == to {
        return Ok(());
    }
    // boolean 与 int 在字节码层面完全相同，不需要任何转换指令
    if (from == K_BOOL && to == K_INT) || (from == K_INT && to == K_BOOL) {
        return Ok(());
    }
    match (from, to) {
        (K_INT, K_LONG) => {
            em.op(0x85); // i2l
            em.push(1);
        }
        (K_INT, K_DOUBLE) => {
            em.op(0x87); // i2d
            em.push(1);
        }
        (K_LONG, K_DOUBLE) => {
            em.op(0x8A); // l2d
            em.push(0);
        }
        (K_LONG, K_INT) => {
            em.op(0x88); // l2i
            em.pop(1);
        }
        (K_DOUBLE, K_INT) => {
            em.op(0x8E); // d2i
            em.pop(1);
        }
        (K_DOUBLE, K_LONG) => {
            em.op(0x8F); // d2l
            em.push(0);
        }
        _ => return err(format!("暂不支持的类型转换（种类 {from} → {to}）")),
    }
    Ok(())
}

/// 表达式 → 声明类型。能静态推出来的才给（数组元素宽度、anewarray/checkcast 的元素类要用）。
///
/// 推不出来返回 `None`：调用方按「int」这一老行为兜底，不要在这里猜。
pub(crate) fn expr_type(e: &Expr, ctx: &Ctx) -> Option<Type> {
    match e {
        Expr::Ident(n) => ctx
            .local_ty
            .get(n)
            .cloned()
            .or_else(|| ctx.static_fields.get(n).map(|(_, d)| type_from_desc(d)))
            // 裸名字也可能是隐式 this 的字段（`new NameIter(data)` 里的 data）：
            // 漏了它，实参/字段的描述符会退化成「String」这种瞎猜
            .or_else(|| ctx.fields.get(n).and_then(|i| ctx.field_tys.get(*i)).cloned()),
        Expr::Field(r, n) => {
            // 静态字段：`ClassName.f`
            if let Some((_, d)) = static_field_access(e, ctx) {
                return Some(type_from_desc(&d));
            }
            // 接收者静态类型已知就按它的字段表查（含继承）
            if let Some(owner) = expr_type(r, ctx).as_ref().and_then(type_internal) {
                if let Some(d) = ctx.type_fields.get(&owner).and_then(|f| f.get(n)) {
                    return Some(type_from_desc(d));
                }
            }
            ctx.fields.get(n).and_then(|i| ctx.field_tys.get(*i)).cloned()
        }
        Expr::Cast { ty, .. } => Some(ty.clone()),
        Expr::New { ty, .. } => Some(ty.clone()),
        // 字面量也要有静态类型：自动装箱（`Integer i = 5;`）靠它判断
        // 「值是基本类型、目标引用了」（漏了这条就不会插 valueOf）
        Expr::IntLit(s) => Some(if s.ends_with('L') || s.ends_with('l') {
            Type::Prim("long")
        } else {
            Type::Prim("int")
        }),
        Expr::FloatLit(_) => Some(Type::Prim("double")),
        Expr::BoolLit(_) => Some(Type::Prim("boolean")),
        Expr::CharLit(_) => Some(Type::Prim("char")),
        Expr::ArrayNew { elem, .. } => Some(Type::Array(Box::new(elem.clone()))),
        Expr::ArrayAccess { array, .. } => match expr_type(array, ctx)? {
            Type::Array(inner) => Some(*inner),
            _ => None,
        },
        // 方法调用：按返回类型（`Color.values()[i]` 这类链式访问要用它的元素类型）
        Expr::Call { recv, name, args, .. } => {
            let desc = call_desc(recv.as_deref(), name, args, ctx)?;
            let ret = desc.rsplit(')').next().unwrap_or("");
            if ret.is_empty() || ret == "V" {
                None
            } else {
                Some(type_from_desc(ret))
            }
        }
        // 三元：两侧一致就用它，否则按 `cond_type` 的规则挑一个
        Expr::Cond { then, els, .. } => super::expr::cond_type(then, els, ctx),
        // 字符串拼接的结果类型是 String（描述符/println 重载都靠它）
        Expr::Binary { op: BinOp::Add, .. } if is_string_val(e, ctx) => {
            Some(Type::Named { path: vec!["String".into()], args: vec![] })
        }
        // 字符串字面量的类型是 String：三元的结果类型、实参描述符都要靠它，
        // 漏了会让「引用槽」被声明成 integer（HotSpot: Inconsistent stackmap frames）
        Expr::StrLit(_) => Some(Type::Named { path: vec!["String".into()], args: vec![] }),
        _ => None,
    }
}

/// 数组表达式的元素种类（决定 iaload/laload/daload/aaload）
pub(crate) fn elem_kind(array: &Expr, ctx: &Ctx) -> u8 {
    match expr_type(array, ctx) {
        Some(Type::Array(inner)) => kind_of(&inner),
        _ => K_INT,
    }
}

/// 数组元素的 (读取, 写入) 指令。
///
/// boolean/byte 必须走 ba*、char 走 ca*、short 走 sa* —— 全用 ia* 会让
/// HotSpot 在 `boolean[]`/`char[]` 上报 "Bad type on operand stack"。
pub(crate) fn array_ops(array: &Expr, ctx: &Ctx) -> (u8, u8) {
    let elem = match expr_type(array, ctx) {
        Some(Type::Array(inner)) => *inner,
        _ => return (0x2E, 0x4F), // iaload / iastore
    };
    match &elem {
        Type::Prim("long") => (0x2F, 0x50),
        Type::Prim("float") => (0x30, 0x51),
        Type::Prim("double") => (0x31, 0x52),
        Type::Prim("boolean") | Type::Prim("byte") => (0x33, 0x54),
        Type::Prim("char") => (0x34, 0x55),
        Type::Prim("short") => (0x35, 0x56),
        _ if kind_of(&elem) == K_REF => (0x32, 0x53),
        _ => (0x2E, 0x4F),
    }
}

/// 表达式 → 类型种类（只用于选字节码，不做严格类型检查）
pub(crate) fn expr_kind(e: &Expr, ctx: &Ctx) -> u8 {
    match e {
        Expr::IntLit(s) => {
            if s.ends_with('L') || s.ends_with('l') { K_LONG } else { K_INT }
        }
        Expr::FloatLit(_) => K_DOUBLE,
        Expr::Cast { ty, .. } => kind_of(ty),
        // 裸名字既可能是局部变量，也可能是隐式 this 的字段（`return d * 2.0;`）：
        // 不查字段表的话 long/double 字段会被当成 int，多插一条 i2d 直接 VerifyError
        Expr::Ident(n) => ctx.local_kind.get(n).copied().unwrap_or_else(|| {
            ctx.fields
                .get(n)
                .and_then(|i| ctx.field_descs.get(*i))
                .map(|d| kind_from_desc(d))
                .or_else(|| ctx.static_fields.get(n).map(|(_, d)| kind_from_desc(d)))
                .unwrap_or(K_INT)
        }),
        Expr::Binary { op, lhs, rhs } => match op {
            // 字符串拼接：任意一侧是 String，结果就是 String（不看数值提升）
            BinOp::Add if is_string_val(e, ctx) => K_REF,
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Eq | BinOp::Ne | BinOp::And
            | BinOp::Or => K_BOOL,
            // 包装类按它的基本类型参与提升（`Integer + int` 就是 int 加法）
            _ => promote(num_kind(lhs, ctx), num_kind(rhs, ctx)),
        },
        Expr::Unary { expr, .. } => num_kind(expr, ctx),
        Expr::ArrayAccess { array, .. } => elem_kind(array, ctx),
        Expr::Call { recv, name, args, .. } => call_desc(recv.as_deref(), name, args, ctx)
            .map(|d| kind_from_ret(&d))
            .unwrap_or(K_INT),
        Expr::StrLit(_) | Expr::This | Expr::Super | Expr::New { .. } | Expr::ArrayNew { .. } | Expr::Null => K_REF,
        Expr::InstanceOf { .. } | Expr::BoolLit(_) => K_BOOL,
        // 三元：两侧都是 boolean 才保持 boolean，否则按数值提升（引用任一侧即引用）。
        // 包装类不算「引用侧」：`c ? box : 1` 的结果是 int（先拆箱）
        Expr::Cond { then, els, .. } => {
            let kt = expr_kind(then, ctx);
            let ke = expr_kind(els, ctx);
            let is_ref_side = |k: u8, e: &Expr| {
                k == K_REF && !matches!(expr_type(e, ctx), Some(t) if is_wrapper_ty(&t))
            };
            if is_ref_side(kt, then) || is_ref_side(ke, els) {
                K_REF
            } else if kt == K_BOOL && ke == K_BOOL {
                K_BOOL
            } else {
                promote(num_kind(then, ctx), num_kind(els, ctx))
            }
        }
        Expr::Field(r, name) => {
            // 静态字段按它的描述符定种类（否则 long/double/String 会被当成 int）
            if let Some((_, desc)) = static_field_access(e, ctx) {
                return kind_from_desc(&desc);
            }
            let (_, desc) = field_access(r, name, ctx);
            kind_from_desc(&desc)
        }
        _ => K_INT,
    }
}


// ---------- 继承相关的解析辅助 ----------

/// 描述符 → AST 类型。继承来的字段只有描述符（没有 AST），要还原成类型。
pub(crate) fn type_from_desc(desc: &str) -> Type {
    match desc.as_bytes().first().copied() {
        Some(b'I') => Type::Prim("int"),
        Some(b'J') => Type::Prim("long"),
        Some(b'S') => Type::Prim("short"),
        Some(b'B') => Type::Prim("byte"),
        Some(b'C') => Type::Prim("char"),
        Some(b'Z') => Type::Prim("boolean"),
        Some(b'F') => Type::Prim("float"),
        Some(b'D') => Type::Prim("double"),
        Some(b'V') => Type::Prim("void"),
        Some(b'[') => Type::Array(Box::new(type_from_desc(&desc[1..]))),
        _ => {
            let inner = desc.trim_start_matches('L').trim_end_matches(';');
            Type::Named { path: inner.split('/').map(|s| s.to_string()).collect(), args: vec![] }
        }
    }
}

/// 类型 → 内部类名（数组给描述符，原始类型给 None）
pub(crate) fn type_internal(ty: &Type) -> Option<String> {
    match ty {
        Type::Named { path, .. } => Some(internal_name(path)),
        Type::Array(_) => Some(descriptor(ty)),
        _ => None,
    }
}

/// java.lang.* 里我们**运行时真的实现了**的方法：类名 → (方法名, 描述符)。
///
/// 这些类没有 `.class` 在 classpath 上，光靠 `type_methods` 查不到签名；
/// 但解释器与 JIT 都内置了它们的实现（`String.equals` 用来写枚举的 valueOf）。
/// 加新方法时三处要一起加：这里、`vm::exec` 的分支、`jit::translate` 的调用点。
const BUILTIN_METHODS: &[(&str, &str, &str)] = &[
    // 只有**描述符**没有实现：运行期按虚分派走到用户自己的类（for-each 的迭代器协议）
    ("java/util/Iterator", "hasNext", "()Z"),
    ("java/util/Iterator", "next", "()Ljava/lang/Object;"),
    ("java/lang/Iterable", "iterator", "()Ljava/util/Iterator;"),
];

/// 内置的**接口**（没有 .class 在 classpath 上，但调用点该发 invokeinterface）
pub(crate) fn is_builtin_iface(owner: &str) -> bool {
    matches!(owner, "java/lang/Iterable" | "java/util/Iterator" | "java/lang/AutoCloseable" | "java/io/Closeable")
}

/// `java.lang.*` 内置方法的描述符（含重载：按实参挑）。
///
/// 没登记过的方法再按 `Object` 的三个协议方法兜底 —— 任何类都继承它们
/// （`p.hashCode()` 在没有覆写的类上也必须能编出来）。
pub(crate) fn builtin_method_desc(owner: &str, name: &str, args: &[Expr], ctx: &Ctx) -> Option<String> {
    if let Some((_, _, d)) = BUILTIN_METHODS
        .iter()
        .find(|(c, n, _)| *c == owner && *n == name)
    {
        return Some(d.to_string());
    }
    if let Some(d) = pick_jdk_overload(crate::jdk::virtuals_of(owner, name), args, ctx) {
        return Some(d);
    }
    crate::jdk::object_protocol(name, args.len())
}

/// 从一组候选里挑描述符：参数个数 → 形状（引用 vs 数值）→ **精确描述符命中数**。
///
/// 最后一档是必需的：`sb.append('y')` 的 `(C)` 与 `(I)` 形状相同，只按形状会挑中
/// `append(int)` 打成 121；按精确描述符比才选得到 `append(char)`。
fn pick_jdk_overload<'a>(
    cands: impl Iterator<Item = &'a crate::jdk::B>,
    args: &[Expr],
    ctx: &Ctx,
) -> Option<String> {
    let arg_descs: Vec<Option<String>> = args
        .iter()
        .map(|a| expr_type(a, ctx).map(|t| descriptor(&t)))
        .collect();
    let mut best: Option<(i32, String)> = None;
    for c in cands {
        let ps = desc_params(c.desc);
        if ps.len() != args.len() {
            continue;
        }
        let mut score = 0i32;
        let mut shape_ok = true;
        for (i, p) in ps.iter().enumerate() {
            let want_ref = p.starts_with('L') || p.starts_with('[');
            let got_ref = match arg_descs.get(i).and_then(|d| d.as_deref()) {
                Some(d) => d.starts_with('L') || d.starts_with('['),
                None => expr_kind(&args[i], ctx) == K_REF,
            };
            if want_ref != got_ref {
                shape_ok = false;
                break;
            }
            if arg_descs.get(i).and_then(|d| d.as_deref()) == Some(p.as_str()) {
                score += 1;
            }
        }
        if !shape_ok {
            continue;
        }
        if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            best = Some((score, c.desc.to_string()));
        }
    }
    best.map(|(_, d)| d)
}


/// `java.lang.*` 内置**静态**方法的描述符（`Integer.parseInt(...)` 这类没有 `.class`）
pub(crate) fn builtin_static_desc(owner: &str, name: &str, args: &[Expr], ctx: &Ctx) -> Option<String> {
    pick_jdk_overload(crate::jdk::statics_of(owner, name), args, ctx)
}

/// 一次方法调用的描述符。
///
/// 接收者的静态类型能推出来就用它查（`P p = ...; p.get()` 沿 P 的继承链），
/// 推不出来就退回本类链 —— 后者是历史行为，`this.f()` / 隐式 this 都走这里。
pub(crate) fn call_desc(recv: Option<&Expr>, name: &str, args: &[Expr], ctx: &Ctx) -> Option<String> {
    let Some(r) = recv else {
        // 无接收者：先看本类静态方法，再看（含继承的）实例方法 —— 后者等于 `this.m()`
        if let Some(d) = ctx.static_methods.get(name) {
            return Some(d.clone());
        }
        return ctx.instance_methods.get(name).cloned();
    };
    // super.m()：从**父类**开始解析（否则会调到自己，覆写就成了死循环）
    if matches!(r, Expr::Super) {
        return ctx.type_methods.get(&ctx.super_name).and_then(|m| m.get(name)).cloned();
    }
    // ClassName.staticMethod(...)
    if let Some((_, d)) = static_call(r, name, args, ctx) {
        return Some(d);
    }
    if let Some(owner) = expr_type(r, ctx).as_ref().and_then(type_internal) {
        if let Some(d) = ctx.type_methods.get(&owner).and_then(|m| m.get(name)) {
            return Some(d.clone());
        }
        if let Some(d) = builtin_method_desc(&owner, name, args, ctx) {
            return Some(d);
        }
    }
    // 包装类/字符串的接收者静态类型可能是 `Object`（泛型擦除）：协议方法仍要能编
    ctx.instance_methods.get(name).cloned()
}

/// 静态字段访问：`ClassName.f`，或者本类的静态字段名 `count`。
///
/// 返回 (声明类, 描述符)。`ClassName` 只有在不是局部变量名时才当类名。
pub(crate) fn static_field_access(e: &Expr, ctx: &Ctx) -> Option<(String, String)> {
    match e {
        Expr::Ident(n) => ctx.static_fields.get(n).cloned(),
        Expr::Field(recv, f) => {
            if let Expr::Ident(cls) = &**recv {
                if ctx.lookup(cls).is_none() {
                    let owner = internal_name(std::slice::from_ref(cls));
                    // 值里带**声明类**：继承来的静态字段要用父类的名字（getstatic 的 owner）
                    if let Some(v) = ctx.type_static_fields.get(&owner).and_then(|m| m.get(f)) {
                        return Some(v.clone());
                    }
                    // java.lang.* 的内建常量：`Integer.MAX_VALUE` / `Long.MIN_VALUE`
                    if let Some(d) = crate::jdk::field_desc(&owner, f) {
                        return Some((owner, d.to_string()));
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// 静态方法调用 `ClassName.m(...)`：返回 (声明类, 描述符)。
///
/// `ClassName` 只有在不是局部变量名时才当类名。
pub(crate) fn static_call(recv: &Expr, name: &str, args: &[Expr], ctx: &Ctx) -> Option<(String, String)> {
    if let Expr::Ident(cls) = recv {
        if ctx.lookup(cls).is_none() {
            let owner = internal_name(std::slice::from_ref(cls));
            if let Some(d) = ctx.type_static_methods.get(&owner).and_then(|m| m.get(name)) {
                return Some((owner, d.clone()));
            }
            // 内建静态方法：`Integer.parseInt(s)` / `System.currentTimeMillis()`
            if let Some(d) = builtin_static_desc(&owner, name, args, ctx) {
                return Some((owner, d));
            }
        }
    }
    None
}

/// 一次字段访问的 (引用类, 描述符)。
///
/// 引用类用**接收者的静态类型**（拿不到就本类）：`getfield P.x` 作用在 Q 实例上，
/// 偏移沿 P 的继承链算出来，与 Q 的布局一致（父类字段在前）。
pub(crate) fn field_access(recv: &Expr, name: &str, ctx: &Ctx) -> (String, String) {
    if let Some(owner) = expr_type(recv, ctx).as_ref().and_then(type_internal) {
        if let Some(d) = ctx.type_fields.get(&owner).and_then(|f| f.get(name)) {
            return (owner, d.clone());
        }
    }
    let desc = ctx
        .fields
        .get(name)
        .and_then(|i| ctx.field_descs.get(*i))
        .cloned()
        .unwrap_or_else(|| "I".to_string());
    (ctx.class_name.clone(), desc)
}
