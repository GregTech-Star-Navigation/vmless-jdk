//! 类型种类、描述符、验证类型与表达式类型推断。

use crate::ast::*;
use std::collections::HashSet;

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
/// float：占 1 槽（与 int 同宽），但走 f* 指令与 ITEM_Float 验证类型
pub(crate) const K_FLOAT: u8 = 5;

pub(crate) fn kind_wide(k: u8) -> bool {
    k == K_LONG || k == K_DOUBLE
}

/// 浮点字面量是不是 `1.5f` 这种 float 形态（`1.5` / `1.5d` 是 double）
pub(crate) fn is_float_suffix(s: &str) -> bool {
    s.ends_with('f') || s.ends_with('F')
}

/// 类型 → 种类
pub(crate) fn kind_of(ty: &Type) -> u8 {
    match ty {
        Type::Prim("long") => K_LONG,
        Type::Prim("double") => K_DOUBLE,
        Type::Prim("float") => K_FLOAT,
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
        Some(b'F') => K_FLOAT,
        Some(b'Z') => K_BOOL,
        Some(b'L') | Some(b'[') => K_REF,
        _ => K_INT,
    }
}

/// 二元/比较运算的公共类型：按 JLS 的数值提升链 double > float > long > int
pub(crate) fn promote(a: u8, b: u8) -> u8 {
    if a == K_DOUBLE || b == K_DOUBLE {
        K_DOUBLE
    } else if a == K_FLOAT || b == K_FLOAT {
        K_FLOAT
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
/// 种类（`K_INT` / `K_LONG` …）→ 基本类型描述符。装箱时用来挑 `Integer.valueOf` 之类。
pub(crate) fn prim_desc_of_kind(k: u8) -> &'static str {
    match k {
        K_LONG => "J",
        K_DOUBLE => "D",
        K_FLOAT => "F",
        K_BOOL => "Z",
        _ => "I",
    }
}

pub(crate) fn unwrap_prim(t: &Type) -> Option<&'static str> {    match t {
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
        } else if expr_kind(e, ctx) != K_REF {
            // 类型推不出来的**数值**表达式也必须装箱：放过这一条，栈上留一个 int
            // 交给 `Map.put(Object,Object)`，真 JVM 直接 VerifyError ——
            // 而且 `javap` 完全看不出来（描述符是对的，错的是没发 valueOf）。
            return emit_box(prim_desc_of_kind(expr_kind(e, ctx)), em, cp);
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


/// 类型的验证类型编码：int → 0，float → 0xFFFC，对象/数组 → 常量池里的类索引
pub(crate) fn ver_type(cp: &mut CpBuilder, ty: &Type) -> u16 {
    match ty {
        Type::Prim("long") => 0xFFFE, // 栈图里要写 ITEM_Long
        Type::Prim("double") => 0xFFFD, // 栈图里要写 ITEM_Double
        Type::Prim("float") => 0xFFFC, // 栈图里要写 ITEM_Float（占 1 槽）
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
        (K_INT, K_FLOAT) => {
            em.op(0x86); // i2f
        }
        (K_INT, K_DOUBLE) => {
            em.op(0x87); // i2d
            em.push(1);
        }
        (K_LONG, K_INT) => {
            em.op(0x88); // l2i
            em.pop(1);
        }
        (K_LONG, K_FLOAT) => {
            em.op(0x89); // l2f
            em.pop(1);
        }
        (K_LONG, K_DOUBLE) => {
            em.op(0x8A); // l2d
            em.push(0);
        }
        // f2i / f2l / f2d：float 只占 1 槽，所以出栈/入栈按目标宽度记账
        (K_FLOAT, K_INT) => {
            em.op(0x8B); // f2i
        }
        (K_FLOAT, K_LONG) => {
            em.op(0x8C); // f2l
            em.push(1);
        }
        (K_FLOAT, K_DOUBLE) => {
            em.op(0x8D); // f2d
            em.push(1);
        }
        (K_DOUBLE, K_INT) => {
            em.op(0x8E); // d2i
            em.pop(1);
        }
        (K_DOUBLE, K_LONG) => {
            em.op(0x8F); // d2l
            em.push(0);
        }
        (K_DOUBLE, K_FLOAT) => {
            em.op(0x90); // d2f
            em.pop(1);
        }
        _ => return err(format!("暂不支持的类型转换（种类 {from} → {to}）")),
    }
    Ok(())
}

/// 表达式 → 声明类型。能静态推出来的才给（数组元素宽度、anewarray/checkcast 的元素类要用）。
///
/// 推不出来返回 `None`：调用方按「int」这一老行为兜底，不要在这里猜。
/// `switch` 表达式的结果类型：取第一个「交值点」的静态类型
/// （箭头体的裸表达式或 `yield e;`）。都推不出来就让调用方按 Object 处理。
pub(crate) fn switch_expr_type(s: &Stmt, ctx: &Ctx) -> Option<Type> {
    let Stmt::Switch { cases, .. } = s else { return None };
    for c in cases {
        if c.arrow && c.body.len() == 1 {
            if let Stmt::Expr(e) = &c.body[0] {
                if let Some(t) = expr_type(e, ctx) {
                    return Some(t);
                }
            }
        }
        for st in &c.body {
            if let Some(t) = first_yield_ty(st, ctx) {
                return Some(t);
            }
        }
    }
    None
}

fn first_yield_ty(s: &Stmt, ctx: &Ctx) -> Option<Type> {
    match s {
        Stmt::Yield(e) => expr_type(e, ctx),
        Stmt::Block(v) => v.iter().find_map(|x| first_yield_ty(x, ctx)),
        Stmt::If { then, els, .. } => {
            first_yield_ty(then, ctx).or_else(|| els.as_ref().and_then(|e| first_yield_ty(e, ctx)))
        }
        Stmt::While { body, .. }
        | Stmt::DoWhile { body, .. }
        | Stmt::Labeled { body, .. }
        | Stmt::For { body, .. }
        | Stmt::ForEach { body, .. } => first_yield_ty(body, ctx),
        Stmt::Switch { cases, .. } => {
            cases.iter().flat_map(|c| c.body.iter()).find_map(|x| first_yield_ty(x, ctx))
        }
        Stmt::Try { body, catches, finally } => first_yield_ty(body, ctx)
            .or_else(|| catches.iter().find_map(|c| first_yield_ty(&c.body, ctx)))
            .or_else(|| finally.as_ref().and_then(|f| first_yield_ty(f, ctx))),
        _ => None,
    }
}

/// 报错用的接收者简写：`a.b` / `foo()` / `new X` —— 只为了让人看出是哪一处
pub(crate) fn recv_brief(e: &Expr) -> String {
    match e {
        Expr::Ident(n) => n.clone(),
        Expr::Field(a, b) => format!("{}.{}", recv_brief(a), b),
        Expr::Call { name, recv, .. } => match recv {
            Some(r) => format!("{}.{name}()", recv_brief(r)),
            None => format!("{name}()"),
        },
        Expr::Cast { ty, expr } => format!("({}){}", descriptor(ty), recv_brief(expr)),
        Expr::New { ty, .. } => "new".to_string() + &descriptor(ty),
        Expr::StrLit(_) => "\"…\"".into(),
        Expr::IntLit(s) => s.clone(),
        Expr::ArrayAccess { array, .. } => format!("{}[…]", recv_brief(array)),
        Expr::Cond { .. } => "?:".into(),
        Expr::Binary { op, .. } => format!("{op:?}"),
        _ => "?".into(),
    }
}

pub(crate) fn expr_type(e: &Expr, ctx: &Ctx) -> Option<Type> {
    match e {
        Expr::Ident(n) => ctx
            .local_ty
            .get(n)
            .cloned()
            .or_else(|| ctx.static_fields.get(n).map(|(_, d)| type_from_desc(d)))
            // 裸名字也可能是隐式 this 的字段（`new NameIter(data)` 里的 data）：
            // 漏了它，实参/字段的描述符会退化成「String」这种瞎猜
            .or_else(|| ctx.fields.get(n).and_then(|i| ctx.field_tys.get(*i)).cloned())
            // 也可能是静态导入进来的常量（`import static …ChronoField.*;` 之后
            // 的 `YEAR` / `MILLISECONDS`）—— 少了这条它们当接收者时类型是 None
            .or_else(|| static_import_field(n, ctx).map(|(_, d)| type_from_desc(&d))),
        Expr::Field(r, n) => {
            // 静态字段：`ClassName.f`
            if let Some((_, d)) = static_field_access(e, ctx) {
                return Some(type_from_desc(&d));
            }
            // 接收者静态类型已知就按它的字段表查（含继承）；没有 classpath 时
            // （verify.sh 的独立模式）回落到 jdk.rs 的内建字段表
            if let Some(t) = expr_type(r, ctx) {
                for owner in type_name_candidates(&t, ctx) {
                    if let Some(d) = ctx.type_fields.get(&owner).and_then(|f| f.get(n)) {
                        return Some(type_from_desc(d));
                    }
                    if let Some(d) = crate::jdk::field_desc(&owner, n) {
                        return Some(type_from_desc(d));
                    }
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
        Expr::FloatLit(s) => Some(if is_float_suffix(s) {
            Type::Prim("float")
        } else {
            Type::Prim("double")
        }),
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
                return None;
            }
            // 泛型方法的**静态**返回类型：`List<String>.get(int)` 擦除后是 Object，
            // 但源码里它是 String —— 不代回去，`l.get(0).isEmpty()` 就在 Object 上
            // 找不到 isEmpty（java.base 里占这一个桶的四成）。
            //
            // **只在擦除后的返回是 Object 时才查签名**：`generic_ret` 要递归求接收者
            // 的类型，而接收者本身又可能是调用 —— 对 `sb.append(a).append(b).append(c)`
            // 这种链，无条件查会让整条链的 `expr_type` 退化成子树重算，
            // 批量编译 java.base 的墙钟时间实测因此涨到 3 倍。
            // 返回 Object 的调用只占少数，条件化之后收益还在、代价回到原样。
            if ret == "Ljava/lang/Object;" || ret.starts_with("TT;") {
                if let Some(t) = recv.as_deref().and_then(|r| generic_ret(r, name, &desc, ctx)) {
                    if !matches!(t, Type::Var) {
                        return Some(t);
                    }
                }
            }
            Some(type_from_desc(ret))
        }
        // `switch` 表达式：结果类型就是交值点的类型
        Expr::SwitchExpr(s) => switch_expr_type(s, ctx),
        // 三元：两侧一致就用它，否则按 `cond_type` 的规则挑一个
        Expr::Cond { then, els, .. } => super::expr::cond_type(then, els, ctx),
        // 字符串拼接的结果类型是 String（描述符/println 重载都靠它）。
        // **必须排在下面那条通用 Binary 之前**：`"a" + 1` 是 String 不是 int。
        Expr::Binary { op: BinOp::Add, .. } if is_string_val(e, ctx) => {
            Some(Type::Named { path: vec!["String".into()], args: vec![] })
        }
        // 算术/位运算/比较的结果类型。
        //
        // 少了这一条，`m.put(i * 7919, …)` 的键、`f(a + 1)` 的实参都**推不出类型**，
        // 于是 `coerce_to(Object, …)` 走「不知道就放过」那条路 —— **不装箱**，
        // 运行时 `Map.put` 收到一个 int，HotSpot 直接 VerifyError。
        // 比全是坑更糟的是它静默：javap 看着没错，跑起来才炸。
        Expr::Binary { op, lhs, rhs } => match op {
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Eq | BinOp::Ne | BinOp::And
            | BinOp::Or => Some(Type::Prim("boolean")),
            _ => Some(match promote(num_kind(lhs, ctx), num_kind(rhs, ctx)) {
                K_LONG => Type::Prim("long"),
                K_FLOAT => Type::Prim("float"),
                K_DOUBLE => Type::Prim("double"),
                K_BOOL => Type::Prim("boolean"),
                _ => Type::Prim("int"),
            }),
        },
        Expr::Unary { op, expr, .. } => match op {
            UnOp::Not => Some(Type::Prim("boolean")),
            _ => Some(match num_kind(expr, ctx) {
                K_LONG => Type::Prim("long"),
                K_FLOAT => Type::Prim("float"),
                K_DOUBLE => Type::Prim("double"),
                K_BOOL => Type::Prim("boolean"),
                _ => Type::Prim("int"),
            }),
        },
        Expr::InstanceOf { .. } => Some(Type::Prim("boolean")),
        // 字符串字面量的类型是 String：三元的结果类型、实参描述符都要靠它，
        // 漏了会让「引用槽」被声明成 integer（HotSpot: Inconsistent stackmap frames）
        Expr::StrLit(_) => Some(Type::Named { path: vec!["String".into()], args: vec![] }),
        // class 字面量：`String.class` 是 `Class<String>`（擦除后就是 Class）
        Expr::ClassLit(_) => Some(Type::Named {
            path: vec!["java".into(), "lang".into(), "Class".into()],
            args: vec![],
        }),
        Expr::MethodRef { name, .. } if name == "class" => Some(Type::Named {
            path: vec!["java".into(), "lang".into(), "Class".into()],
            args: vec![],
        }),
        // lambda / 方法引用的静态类型就是它的**目标**功能性接口（多表达式的类型
        // 完全由上下文决定）。没有目标就返回 None —— 让调用点按「未知引用」处理。
        Expr::Lambda { .. } | Expr::LambdaStmt { .. } => ctx
            .lambda_target
            .borrow()
            .clone()
            .map(|t| Type::Named { path: t.iface.split('/').map(|s| s.to_string()).collect(), args: t.args.clone() }),
        Expr::MethodRef { .. } => ctx
            .lambda_target
            .borrow()
            .clone()
            .map(|t| Type::Named { path: t.iface.split('/').map(|s| s.to_string()).collect(), args: t.args.clone() }),
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
        Expr::FloatLit(s) => {
            if is_float_suffix(s) { K_FLOAT } else { K_DOUBLE }
        }
        Expr::Cast { ty, .. } => kind_of(ty),
        // 裸名字既可能是局部变量，也可能是隐式 this 的字段（`return d * 2.0;`）：
        // 不查字段表的话 long/double 字段会被当成 int，多插一条 i2d 直接 VerifyError
        Expr::Ident(n) => ctx.local_kind.get(n).copied().unwrap_or_else(|| {
            ctx.fields
                .get(n)
                .and_then(|i| ctx.field_descs.get(*i))
                .map(|d| kind_from_desc(d))
                .or_else(|| ctx.static_fields.get(n).map(|(_, d)| kind_from_desc(d)))
                .or_else(|| static_import_field(n, ctx).map(|(_, d)| kind_from_desc(&d)))
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
        // `{1,2}` / `ArrayInit` 也是**引用**（它算出的是一个数组）。漏了这一条，
        // `int[][] m = {{1,2},{3,4}}` 的内层数组会被当成 int，于是在它上面发
        // `Integer.valueOf(I)` —— 栈上明明是 `[I`，HotSpot 直接
        // `VerifyError: Bad type on operand stack`。
        Expr::StrLit(_) | Expr::TextBlock(_) | Expr::This | Expr::Super | Expr::New { .. }
        | Expr::ArrayNew { .. } | Expr::ArrayInit(_) | Expr::Null => K_REF,
        // class 字面量是 java.lang.Class 的引用
        Expr::ClassLit(_) => K_REF,
        Expr::MethodRef { name, .. } if name == "class" => K_REF,
        // lambda / 方法引用都是引用（invokedynamic 压进栈的是一个接口实例）
        Expr::Lambda { .. } | Expr::LambdaStmt { .. } | Expr::MethodRef { .. } => K_REF,
        Expr::InstanceOf { .. } | Expr::BoolLit(_) => K_BOOL,
        // switch 表达式的种类：交值点的静态类型（推不出来按引用）
        Expr::SwitchExpr(s) => switch_expr_type(s, ctx).map(|t| kind_of(&t)).unwrap_or(K_REF),
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

/// `X.class` 里的类型名 → 内部名。
///
/// 与 `type_internal` 的区别：这里按**上下文里已知的类**把包前缀与嵌套类型（`$`）补全。
/// 因为表达式位置上的类名只经过 `qualify` 的 `imported_class` 处理 ——
/// 同包的 `BufferedInputStream.class` 还是裸名，`Map.Entry.class` 是「外层.嵌套」点分形式。
pub(crate) fn class_operand_name(ty: &Type, ctx: &Ctx) -> Option<String> {
    let path = match ty {
        Type::Named { path, .. } => path,
        // `String[].class` 的 CONSTANT_Class 名字就是数组描述符
        Type::Array(_) => return Some(descriptor(ty)),
        _ => return None,
    };
    let known = |n: &str| {
        ctx.type_fields.contains_key(n) || ctx.iface_types.contains(n) || ctx.type_ctors.contains_key(n)
    };
    let pkg = ctx.class_name.rsplit_once('/').map(|(p, _)| p.to_string());
    let base = if path.len() == 1 { internal_name(path) } else { path.join("/") };
    if known(&base) {
        return Some(base);
    }
    if path.len() == 1 {
        if let Some(p) = &pkg {
            let q = format!("{p}/{base}");
            if known(&q) {
                return Some(q);
            }
        }
    }
    // 前缀当类型、其余按 `$` 拼：`Outer.Inner` / `java.util.Map.Entry`
    for split in 1..path.len() {
        let head = if split == 1 {
            match &pkg {
                Some(p) if known(&format!("{p}/{}", path[0])) => format!("{p}/{}", path[0]),
                _ => path[0].clone(),
            }
        } else {
            path[..split].join("/")
        };
        let j = format!("{head}${}", path[split..].join("$"));
        if known(&j) {
            return Some(j);
        }
    }
    Some(base)
}

/// 从一组**同名的重载描述符**里挑一个：参数个数 → 形状（引用/数值）→ 精确命中数。
///
/// 只按名字存一个描述符是不行的：`CipherCore.update` 有 4 个重载，
/// `Arrays.fill` 有十来种，挑错那个会连带把实参按错的类型转换，报出一堆假错误。
pub(crate) fn pick_desc(cands: &[String], args: &[Expr], ctx: &Ctx) -> Option<String> {
    let mut best: Option<(i32, &String)> = None;
    for d in cands {
        // 形状 + lambda 约束 + 精确命中，全部交给 `arg_score` —— 构造器与普通方法
        // **必须用同一套判据**，否则 `new Thread(() -> {})` 这种会在重载里撞运气
        let Some(score) = super::expr::arg_score(&desc_params(d), args, ctx) else {
            continue;
        };
        if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            best = Some((score, d));
        }
    }
    if let Some((score, d)) = best {
        // 固定参数候选的**最优分**先留着，和可变参数候选比一比：
        // 「形状对上但类型不对」（1 分）不该压过「固定部分精确命中」（2 分），
        // 否则 `Paths.get("x")` 会挑 `Paths.get(URI)` 而不是
        // `Paths.get(String, String...)` —— 编出来是个非法 class。
        if score >= 2 {
            return Some(d.clone());
        }
    }
    // 固定参数候选择不出合适的（比如 0 分里混着形状不对的）：再试可变参数。
    //
    // `Paths.get(String, String...)` 与 `Paths.get(URI)` 就是这一类：实参一个
    // String，固定参数版只有 `(URI)`（形状对不上），可变参数版才是正确的。
    let mut vbest: Option<(i32, &String)> = None;
    for d in cands {
        let ps = desc_params(d);
        let Some(last) = ps.last() else { continue };
        if !last.starts_with('[') {
            continue;
        }
        let fixed = ps.len() - 1;
        if args.len() < fixed {
            continue;
        }
        // 形式一：末位实参就是那个数组（`m(arr)`），交给固定参数路径更合适
        if args.len() == ps.len()
            && super::expr::arg_looks_like_array_pub(&args[fixed], last, ctx)
        {
            continue;
        }
        let mut score = 0i32;
        let mut ok = true;
        for (i, p) in ps[..fixed].iter().enumerate() {
            let want_ref = p.starts_with('L') || p.starts_with('[');
            let ad = expr_type(&args[i], ctx).map(|t| descriptor(&t));
            let got_ref = match ad.as_deref() {
                Some(dd) => dd.starts_with('L') || dd.starts_with('['),
                None => expr_kind(&args[i], ctx) == K_REF,
            };
            if want_ref != got_ref {
                ok = false;
                break;
            }
            if ad.as_deref() == Some(p.as_str()) {
                score += 2;
            }
        }
        if !ok {
            continue;
        }
        // 尾部实参要能装进元素类型
        let elem = &last[1..];
        for a in &args[fixed..] {
            let ad = match expr_type(a, ctx).map(|t| descriptor(&t)) {
                Some(dd) => dd,
                None => continue,
            };
            let want_ref = elem.starts_with('L') || elem.starts_with('[');
            let got_ref = ad.starts_with('L') || ad.starts_with('[');
            if want_ref != got_ref {
                ok = false;
                break;
            }
            if ad == elem {
                score += 1;
            }
        }
        if !ok {
            continue;
        }
        if vbest.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            vbest = Some((score, d));
        }
    }
    // 可变参数候选分数更高才替换固定参数那个（平手优先固定参数）
    match (best, vbest) {
        (Some((bs, bd)), Some((vs, vd))) => {
            if vs > bs {
                Some(vd.clone())
            } else {
                Some(bd.clone())
            }
        }
        (Some((_, bd)), None) => Some(bd.clone()),
        (None, Some((_, vd))) => Some(vd.clone()),
        (None, None) => cands
            .iter()
            .find(|d| desc_params(d).len() == args.len())
            .cloned()
            .or_else(|| cands.first().cloned()),
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
        if let Some(d) = ctx.static_methods.get(name).and_then(|c| pick_desc(c, args, ctx)) {
            return Some(d);
        }
        // 从父类/外层类继承来的静态方法
        if let Some(d) = ctx.inherited_statics.get(name).and_then(|(_, ds)| pick_desc(ds, args, ctx)) {
            return Some(d);
        }
        // `import static p.C.m;` 之后就是裸名字调用，没有接收者可查
        if let Some((_, d)) = static_import_call(name, args, ctx) {
            return Some(d);
        }
        // 裸名字的 `getClass()` / `hashCode()` / `toString()`：它们来自 Object
        if let Some(d) = ctx.instance_methods.get(name).and_then(|c| pick_desc(c, args, ctx)) {
            return Some(d);
        }
        return crate::jdk::object_protocol(name, args.len());
    };
    // super.m()：从**父类**开始解析（否则会调到自己，覆写就成了死循环）
    if matches!(r, Expr::Super) {
        return ctx.type_methods.get(&ctx.super_name).and_then(|m| m.get(name)).and_then(|c| pick_desc(c, args, ctx));
    }
    // ClassName.staticMethod(...)
    if let Some((_, d)) = static_call(r, name, args, ctx) {
        return Some(d);
    }
    // 接收者的静态类型能推出来：按候选内部名逐个试符号表，再试内建表。
    //
    // **两个都要试、且要按同一个名字试**：有 classpath 时符号表里有真正的
    // `java/lang/String`；没有 classpath（verify.sh 的独立模式）时符号表里没有它，
    // 全靠 `builtin_method_desc` 那张内建表给描述符。任何一侧漏掉都会让
    // `s.length()` 这种最基本的东西编不过。
    if let Some(t) = expr_type(r, ctx) {
        for owner in type_name_candidates(&t, ctx) {
            if let Some(cands) = ctx.type_methods.get(&owner).and_then(|m| m.get(name)) {
                let vis = visible_overloads(&owner, name, cands, ctx);
                if let Some(d) = pick_desc(&vis, args, ctx) {
                    return Some(d);
                }
            }
            if let Some(d) = builtin_method_desc(&owner, name, args, ctx) {
                return Some(d);
            }
        }
    }
    // 包装类/字符串的接收者静态类型可能是 `Object`（泛型擦除）：协议方法仍要能编
    ctx.instance_methods.get(name).and_then(|c| pick_desc(c, args, ctx))
}

/// 简单类名 → 要试的内部名候选（**顺序就是名字解析顺序**）。
///
/// `sema::qualify` 只补 `import` 进来的名字 —— 同包的简单类名故意原样留着（怕和
/// 同名局部变量混淆），所以这里得按 JLS 6.5.5.1 的顺序补齐：
/// 本类/外层类的成员类型 → 同包 → `java.lang`。
///
/// ⚠️ 别在开头用 `internal_name([cls])`：它会把裸名 `Record` 直接翻成
/// `java/lang/Record`（java.lang 名字表），于是 `sun/security/ssl/Record` 这种
/// **同包同名类**永远轮不到（SSL 那一大片 `Record.getInt16(...)` 就是这么挂的）。
pub(crate) fn class_name_forms(cls: &str, ctx: &Ctx) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |out: &mut Vec<String>, s: String| {
        if !out.contains(&s) {
            out.push(s);
        }
    };
    if cls.contains('/') {
        push(&mut out, cls.to_string());
        // 嵌套类型写成 `a/b/Outer/Inner`：把尾部每个大写开头的段用 `$` 拼回去
        let segs: Vec<&str> = cls.split('/').collect();
        for split in (1..segs.len()).rev() {
            if !segs[split..].iter().all(|s| s.chars().next().is_some_and(|c| c.is_ascii_uppercase())) {
                break;
            }
            push(&mut out, format!("{}${}", segs[..split].join("/"), segs[split..].join("$")));
        }
        return out;
    }
    // 本类的成员类型（`Holder.INSTANCE` 里的 Holder 常常就是当前类里的嵌套类），
    // 再一层层往外找外层类
    let mut cur = ctx.class_name.clone();
    push(&mut out, format!("{cur}${cls}"));
    while let Some(i) = cur.rfind('$') {
        cur.truncate(i);
        push(&mut out, format!("{cur}${cls}"));
    }
    // 同包
    if let Some((pkg, _)) = ctx.class_name.rsplit_once('/') {
        push(&mut out, format!("{pkg}/{cls}"));
    }
    // 裸名本身：默认包（无 package）里的类就叫 `Float2`；`qualify` 已经把
    // 嵌套类型写成 `Mix2$Kind` 这种内部名形态，那时裸名就是符号表里的键
    push(&mut out, cls.to_string());
    // 隐式导入的 `java.lang`（`ModuleLayer.boot()` / `Character.digit(...)`）
    push(&mut out, format!("java/lang/{cls}"));
    out
}

/// 一个类型在符号表（`ctx.type_*`）里可能登记的内部名候选，最可能的在前。
///
/// 语义层留下的类型名可能是点分形式（`java.security.Provider.Service`）、裸名
/// （`Class`）、或嵌套类型（`Map.Entry`），而符号表的键全是内部名
/// （`java/security/Provider$Service`、`java/lang/Class`、`java/util/Map$Entry`）。
/// 不列候选就只能按字符串硬匹配，命不中就报「未找到实例方法」。
pub(crate) fn type_name_candidates(ty: &Type, ctx: &Ctx) -> Vec<String> {
    let path = match ty {
        Type::Named { path, .. } => path,
        Type::Array(_) => return vec![descriptor(ty)],
        _ => return Vec::new(),
    };
    let mut out: Vec<String> = Vec::new();
    let mut push = |out: &mut Vec<String>, s: String| {
        if !out.contains(&s) {
            out.push(s);
        }
    };
    let base = internal_name(path);
    push(&mut out, base.clone());
    // 嵌套类型：尾部每段都以大写开头时，把最后一个 `/` 换成 `$`
    for split in (1..path.len()).rev() {
        if !path[split..].iter().all(|s| s.chars().next().is_some_and(|c| c.is_ascii_uppercase())) {
            break;
        }
        push(&mut out, format!("{}${}", path[..split].join("/"), path[split..].join("$")));
    }
    if path.len() == 1 {
        let n = path[0].clone();
        if let Some((pkg, _)) = ctx.class_name.rsplit_once('/') {
            push(&mut out, format!("{pkg}/{n}"));
        }
        // 隐式导入的 `java.lang`
        push(&mut out, format!("java/lang/{n}"));
        // 本单元里的嵌套类型：`Holder` → `pkg/Outer$Holder`
        push(&mut out, format!("{}${n}", ctx.class_name));
    }
    out
}

/// 泛型方法的**静态**返回类型：把接收者的类型实参代进声明处的返回签名。
///
/// ```text
/// List<String> l;  l.get(0)      →  E := String    → String
/// Map<K,V> m;      m.get(k)      →  V := V'        → V'
/// Map<String,Integer> m; m.keySet() → Set<String>
/// ```
///
/// 代不回去就返回 `None`，调用方退回擦除后的类型 —— 也就是现状，不会更差。
pub(crate) fn generic_ret(recv: &Expr, name: &str, desc: &str, ctx: &Ctx) -> Option<Type> {
    let recv_ty = expr_type(recv, ctx)?;
    generic_ret_of(&recv_ty, name, desc, ctx)
}

/// `generic_ret` 的核心：接收者类型已知时按它查（转发调用点用这个）
pub(crate) fn generic_ret_of(recv_ty: &Type, name: &str, desc: &str, ctx: &Ctx) -> Option<Type> {
    let sig = generic_sig(recv_ty, name, desc, ctx)?;
    let (targs, tparams, sig) = sig;
    let map = |n: &str| -> Option<Type> { tparams.iter().position(|p| p == n).map(|i| targs[i].clone()) };
    Some(crate::sema::generic::subst(&sig.ret, &map))
}

/// 泛型方法**第 `i` 个形参**的静态类型（实参位置的 lambda 要靠它拿类型实参）。
///
/// `thenComparing(Function<? super T,? extends U>)` 在 `Comparator<Emp>` 上就是
/// `Function<Emp,?> `—— lambda 的形参于是是 `Emp` 而不是 `Object`。
pub(crate) fn generic_param_ty(
    recv_ty: &Type,
    name: &str,
    desc: &str,
    i: usize,
    ctx: &Ctx,
) -> Option<Type> {
    let (targs, tparams, sig) = generic_sig(recv_ty, name, desc, ctx)?;
    let p = sig.params.get(i)?;
    if !p.has_var() {
        return None;
    }
    let map = |n: &str| -> Option<Type> { tparams.iter().position(|p| p == n).map(|k| targs[k].clone()) };
    Some(crate::sema::generic::subst(p, &map))
}

/// 找到「接收者类型 `recv_ty` 上 `name(desc)` 这个方法的泛型签名」
/// → `(接收者的类型实参, 声明类的类型参数名, 方法签名)`
fn generic_sig(
    recv_ty: &Type,
    name: &str,
    desc: &str,
    ctx: &Ctx,
) -> Option<(Vec<Type>, Vec<String>, crate::sema::generic::MethodSig)> {
    let targs = match recv_ty {
        Type::Named { args, .. } if !args.is_empty() => args.clone(),
        _ => return None,
    };
    for owner in type_name_candidates(recv_ty, ctx) {
        let Some(table) = ctx.type_method_generics.get(&owner) else { continue };
        let Some(cands) = table.get(name) else { continue };
        let Some((_, sig, tparams)) = cands.iter().find(|(d, _, _)| d == desc) else { continue };
        // 类型参数按**位置**对应类型实参。声明类与接收者是同一个类时这必然正确；
        // 声明在祖先上时（`List.get` 之于 `ArrayList`）两边参数个数与含义一致
        // （`E` 之于 `E`、`K,V` 之于 `K,V`），按位置也对。
        if tparams.len() != targs.len() {
            return None;
        }
        return Some((targs, tparams.clone(), sig.clone()));
    }
    None
}

/// 两个内部名是不是同一个包（默认包算一个包）
pub(crate) fn same_package(a: &str, b: &str) -> bool {
    let pa = a.rsplit_once('/').map(|(p, _)| p);
    let pb = b.rsplit_once('/').map(|(p, _)| p);
    pa == pb
}

/// `a` 能不能赋给 `b`（只看引用，沿父类型 BFS；查不到就当不成立）
///
/// 重载解析必须用得上它：`new TreeSet<>(someSet)` 的 `someSet` 是 `Set`，
/// 候选 `TreeSet(Comparator)` / `TreeSet(Collection)` / `TreeSet(SortedSet)`
/// 全是「一个引用参数」—— 只按形状比等于没比，最后全看类文件里的声明顺序，
/// 撞上 `TreeSet(Comparator)` 就是编译通过、运行时 ClassCastException。
pub(crate) fn is_assignable(a: &str, b: &str, ctx: &Ctx) -> bool {
    if a == b {
        return true;
    }
    let mut seen: HashSet<&str> = HashSet::new();
    let mut queue: Vec<&str> = vec![a];
    let mut steps = 0;
    while let Some(cur) = queue.pop() {
        if !seen.insert(cur) {
            continue;
        }
        if cur == b {
            return true;
        }
        // 上限：避免巨型继承链上把编译时间吃光（java.base 最深也就十几层）
        steps += 1;
        if steps > 64 {
            break;
        }
        if let Some(ss) = ctx.type_supers.get(cur) {
            for s in ss.iter() {
                queue.push(s);
            }
        }
    }
    false
}

/// 从「接收者类 `owner` 上的同名重载」里去掉**跨包不可见**的那些。
///
/// 同包一律放行（零拷贝直接返回）；跨包时按 `type_hidden` 过滤 ——
/// `new TreeSet<>(someSet)` 曾经挑中包私有的 `TreeSet(NavigableMap)`，
/// 编出来是合法 class 但真 JVM 抛 `IllegalAccessError`。
pub(crate) fn visible_overloads<'a>(
    owner: &str,
    name: &str,
    cands: &'a [String],
    ctx: &Ctx,
) -> std::borrow::Cow<'a, [String]> {
    if same_package(owner, &ctx.class_name) {
        return std::borrow::Cow::Borrowed(cands);
    }
    let Some(hidden) = ctx.type_hidden.get(owner) else {
        return std::borrow::Cow::Borrowed(cands);
    };
    let keep: Vec<String> = cands
        .iter()
        .filter(|d| !hidden.contains(&format!("{name}\u{0}{d}")))
        .cloned()
        .collect();
    // 全被过滤掉时退回原表：宁可报下游的具体错误，也不要一句「找不到方法」
    if keep.is_empty() {
        return std::borrow::Cow::Borrowed(cands);
    }
    std::borrow::Cow::Owned(keep)
}

/// 类型 → 符号表里真实存在的内部名（先直拼，再试嵌套 `$` / 包前缀 / `java.lang`）
pub(crate) fn resolve_type_owner(ty: &Type, ctx: &Ctx) -> Option<String> {    type_name_candidates(ty, ctx).into_iter().find(|c| {
        ctx.type_methods.contains_key(c)
            || ctx.type_fields.contains_key(c)
            || ctx.type_static_methods.contains_key(c)
            || ctx.iface_types.contains(c)
    })
}

/// `a.b.C` 这种点分链 → 段列表（只认 Ident/Field 组成、不含局部变量的式子）
fn expr_dotted_path(e: &Expr) -> Option<Vec<String>> {
    match e {
        Expr::Ident(n) => Some(vec![n.clone()]),
        Expr::Field(inner, seg) => {
            let mut v = expr_dotted_path(inner)?;
            v.push(seg.clone());
            Some(v)
        }
        _ => None,
    }
}

/// 接收者可能是「类名 + 嵌套段」：`Opcode.Kind.ARRAY_LOAD` 里的 `Opcode.Kind` 是嵌套
/// 类型 `java/lang/classfile/Opcode$Kind`，不是「Opcode 的实例字段 Kind」。
/// 返回所有要试的所有者内部名（查不到就空，调用方按实例访问继续走）。
pub(crate) fn class_owner_forms(e: &Expr, ctx: &Ctx) -> Vec<String> {
    match e {
        Expr::Ident(n) => {
            if ctx.lookup(n).is_some() {
                Vec::new()
            } else {
                class_name_forms(n, ctx)
            }
        }
        // 只有「看起来像类型名」的段才拼 `$`：字段/方法是小写开头，嵌套类型是大写
        Expr::Field(inner, seg) if seg.chars().next().is_some_and(|c| c.is_ascii_uppercase()) => {
            let mut out: Vec<String> =
                class_owner_forms(inner, ctx).into_iter().map(|o| format!("{o}${seg}")).collect();
            // 全限定类名：`jdk.internal.misc.Unsafe.getUnsafe()` 里的 `jdk.internal.misc.Unsafe`
            // 会被解析成点分 Field 链，而前缀全是包段（小写）—— 那不是嵌套类型，是包名，
            // 只能按 `/` 拼。少了这条，全限定静态调用一律报「未找到实例方法」。
            if let Some(path) = expr_dotted_path(e) {
                if path.len() > 1
                    && path[..path.len() - 1].iter().all(|s| s.chars().next().is_some_and(|c| c.is_ascii_lowercase()))
                {
                    let joined = path.join("/");
                    if !out.contains(&joined) {
                        out.push(joined);
                    }
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

/// 静态导入的**方法**：`requireNonNull(x)` 这种没有接收者的调用。
pub(crate) fn static_import_call(name: &str, args: &[Expr], ctx: &Ctx) -> Option<(String, String)> {
    for owner in static_import_owners(name, ctx) {
        if let Some(d) = ctx
            .type_static_methods
            .get(&owner)
            .and_then(|m| m.get(name))
            .and_then(|c| pick_desc(c, args, ctx))
        {
            return Some((owner, d));
        }
        if let Some(d) = builtin_static_desc(&owner, name, args, ctx) {
            return Some((owner, d));
        }
    }
    None
}

/// 静态导入的**字段/常量**：`UTF_8` / `ERA` / `PRECISION` 这种裸名字。
pub(crate) fn static_import_field(name: &str, ctx: &Ctx) -> Option<(String, String)> {
    for owner in static_import_owners(name, ctx) {
        if let Some(v) = ctx.type_static_fields.get(&owner).and_then(|m| m.get(name)) {
            return Some(v.clone());
        }
        if let Some(d) = crate::jdk::field_desc(&owner, name) {
            return Some((owner, d.to_string()));
        }
    }
    None
}

/// 一个裸成员名可能要试的声明类：具名静态导入在前，通配静态导入在后
fn static_import_owners(name: &str, ctx: &Ctx) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(v) = ctx.static_imports.get(name) {
        out.extend(v.iter().cloned());
    }
    if let Some(v) = ctx.static_imports.get("*") {
        for o in v {
            if !out.contains(o) {
                out.push(o.clone());
            }
        }
    }
    out
}

/// 静态字段访问：`ClassName.f`，或者本类的静态字段名 `count`。
///
/// 返回 (声明类, 描述符)。`ClassName` 只有在不是局部变量名时才当类名。
pub(crate) fn static_field_access(e: &Expr, ctx: &Ctx) -> Option<(String, String)> {
    match e {
        Expr::Ident(n) => ctx.static_fields.get(n).cloned().or_else(|| static_import_field(n, ctx)),
        Expr::Field(recv, f) => {
            for owner in class_owner_forms(recv, ctx) {
                // 值里带**声明类**：继承来的静态字段要用父类的名字（getstatic 的 owner）
                if let Some(v) = ctx.type_static_fields.get(&owner).and_then(|m| m.get(f)) {
                    return Some(v.clone());
                }
                // java.lang.* 的内建常量：`Integer.MAX_VALUE` / `Long.MIN_VALUE`
                if let Some(d) = crate::jdk::field_desc(&owner, f) {
                    return Some((owner, d.to_string()));
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
    for owner in class_owner_forms(recv, ctx) {
        if let Some(d) = ctx.type_static_methods.get(&owner).and_then(|m| m.get(name)).and_then(|c| pick_desc(c, args, ctx)) {
            return Some((owner, d));
        }
        // 内建静态方法：`Integer.parseInt(s)` / `System.currentTimeMillis()`
        if let Some(d) = builtin_static_desc(&owner, name, args, ctx) {
            return Some((owner, d));
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
