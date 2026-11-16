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
    "InterruptedException", "NoSuchElementException", "StackOverflowError",
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
        Expr::Ident(n) => ctx.local_ty.get(n).cloned(),
        Expr::Field(_, n) => ctx.fields.get(n).and_then(|i| ctx.field_tys.get(*i)).cloned(),
        Expr::Cast { ty, .. } => Some(ty.clone()),
        Expr::New { ty, .. } => Some(ty.clone()),
        Expr::ArrayNew { elem, .. } => Some(Type::Array(Box::new(elem.clone()))),
        Expr::ArrayAccess { array, .. } => match expr_type(array, ctx)? {
            Type::Array(inner) => Some(*inner),
            _ => None,
        },
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

/// 表达式 → 类型种类（只用于选字节码，不做严格类型检查）
pub(crate) fn expr_kind(e: &Expr, ctx: &Ctx) -> u8 {
    match e {
        Expr::IntLit(s) => {
            if s.ends_with('L') || s.ends_with('l') { K_LONG } else { K_INT }
        }
        Expr::FloatLit(_) => K_DOUBLE,
        Expr::Cast { ty, .. } => kind_of(ty),
        Expr::Ident(n) => ctx.local_kind.get(n).copied().unwrap_or(K_INT),
        Expr::Binary { op, lhs, rhs } => match op {
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Eq | BinOp::Ne | BinOp::And
            | BinOp::Or => K_BOOL,
            _ => promote(expr_kind(lhs, ctx), expr_kind(rhs, ctx)),
        },
        Expr::Unary { expr, .. } => expr_kind(expr, ctx),
        Expr::ArrayAccess { array, .. } => elem_kind(array, ctx),
        Expr::Call { recv, name, .. } => {
            let desc = if recv.is_some() {
                ctx.instance_methods.get(name.as_str())
            } else {
                ctx.static_methods.get(name.as_str())
            };
            desc.map(|d| kind_from_ret(d)).unwrap_or(K_INT)
        }
        Expr::StrLit(_) | Expr::This | Expr::New { .. } | Expr::ArrayNew { .. } | Expr::Null => K_REF,
        Expr::InstanceOf { .. } | Expr::BoolLit(_) => K_BOOL,
        Expr::Field(_, name) => ctx
            .fields
            .get(name)
            .and_then(|i| ctx.field_descs.get(*i))
            .map(|d| kind_from_desc(d))
            .unwrap_or(K_INT),
        _ => K_INT,
    }
}

