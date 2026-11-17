//! 表达式编译。

use crate::ast::*;

use super::constpool::CpBuilder;
use super::ctx::Ctx;
use super::emit::{Emit, SCRATCH_SLOT};
use super::error::*;
use super::types::*;

pub(crate) fn is_println_call(e: &Expr) -> bool {
    matches!(e, Expr::Call { name, .. } if name == "println")
}

/// 实参列表 → 参数描述符串（`new T(...)` 与 `super(...)` 共用）
pub(crate) fn args_desc(args: &[Expr], ctx: &Ctx) -> String {
    args.iter()
        .map(|a| match expr_kind(a, ctx) {
            K_LONG => "J".to_string(),
            K_DOUBLE => "D".to_string(),
            K_REF => expr_type(a, ctx)
                .map(|t| descriptor(&t))
                .unwrap_or_else(|| "Ljava/lang/String;".to_string()),
            _ => "I".to_string(),
        })
        .collect()
}

/// println 的引用重载：声明类型确实是 String（或推不出来）才用 println(String)。
///
/// 声明成 `Object` 的局部变量，在有栈图的帧里验证器眼里就是 Object，
/// 这时发 `println(String)` 会直接 VerifyError —— 必须退回 `println(Object)`。
pub(crate) fn ref_println_desc(e: &Expr, ctx: &Ctx) -> &'static str {
    match expr_type(e, ctx) {
        Some(Type::Named { path, .. }) => {
            let joined = path.join("/");
            if joined == "String" || joined == "java/lang/String" {
                "(Ljava/lang/String;)V"
            } else {
                "(Ljava/lang/Object;)V"
            }
        }
        // 推不出来（方法调用的返回值等）就按老行为当 String
        _ => "(Ljava/lang/String;)V",
    }
}

/// 这个表达式是不是一次返回 void 的方法调用（语句位置上不能弹返回值）
pub(crate) fn is_void_call(e: &Expr, ctx: &Ctx) -> bool {
    let Expr::Call { recv, name, args, .. } = e else { return false };
    // super(...) / this(...) / super.m() 都是 invokespecial，没有返回值可弹
    if name == "super" || name == "this" {
        return true;
    }
    matches!(call_desc(recv.as_deref(), name, args, ctx), Some(d) if d.ends_with(")V"))
}

/// 这个表达式编成**值**时会不会自己产生跳转（比较、`&&`/`||`、三元）。
///
/// 栈图恒声明「操作数栈为空」，所以这种表达式必须从空栈开算。
/// 调用方若在它前面已经压了东西（`println(…)` 的 PrintStream、`a + (b<c)` 的 a、
/// 前面几个实参），就得先 `spill` 出去，算完再取回来。
pub(crate) fn needs_scratch(e: &Expr) -> bool {
    match e {
        Expr::Binary { op, lhs, rhs } => {
            matches!(
                op,
                BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Eq | BinOp::Ne
                    | BinOp::And | BinOp::Or
            ) || needs_scratch(lhs)
                || needs_scratch(rhs)
        }
        Expr::Unary { expr, .. } => needs_scratch(expr),
        Expr::Cast { expr, .. } => needs_scratch(expr),
        Expr::InstanceOf { expr, .. } => needs_scratch(expr),
        Expr::ArrayAccess { array, index } => needs_scratch(array) || needs_scratch(index),
        Expr::ArrayNew { dims, .. } => dims.iter().any(needs_scratch),
        Expr::ArrayInit(items) => items.iter().any(needs_scratch),
        Expr::New { args, .. } => args.iter().any(needs_scratch),
        Expr::Field(r, _) => needs_scratch(r),
        Expr::Call { recv, args, .. } => {
            recv.as_deref().map_or(false, needs_scratch) || args.iter().any(needs_scratch)
        }
        Expr::Assign { lhs, rhs, .. } => needs_scratch(lhs) || needs_scratch(rhs),
        Expr::Cond { .. } => true,
        _ => false,
    }
}

/// `{1, 2, 3}` / `new int[]{1, 2, 3}`：建数组 + 逐项写入，数组引用留在栈上。
///
/// 元素类型优先用**声明处的类型**（`int[] a = {…}` 的 `int[]`）；AST 里
/// `ArrayInit` 不带类型，所以拿不到声明类型时按元素字面量推断（`1` → int，
/// `1.0` → double，`"s"` → String，`null`/其它 → Object）。
pub(crate) fn array_init_elem(items: &[Expr], declared: Option<&Type>, ctx: &Ctx) -> Type {
    if let Some(Type::Array(inner)) = declared {
        return (**inner).clone();
    }
    // 没有声明类型时按元素推断：取所有元素里「最宽」的那个种类
    // （`{1, 2.5}` 是 double[]，只看第一个字面量会误判成 int[]）
    let mut k = K_INT;
    for it in items {
        let ik = num_kind(it, ctx);
        if ik == K_REF {
            return match expr_type(it, ctx) {
                Some(t @ Type::Named { .. }) => t,
                _ => Type::Named { path: vec!["Object".into()], args: vec![] },
            };
        }
        k = promote(k, ik);
    }
    match k {
        K_LONG => Type::Prim("long"),
        K_FLOAT => Type::Prim("float"),
        K_DOUBLE => Type::Prim("double"),
        K_BOOL => Type::Prim("boolean"),
        _ => Type::Prim("int"),
    }
}

pub(crate) fn emit_array_init(
    elem: &Type,
    items: &[Expr],
    em: &mut Emit,
    ctx: &Ctx,
    cp: &mut CpBuilder,
) -> Result<(), CompileError> {
    em.const_int(items.len() as i32);
    match elem {
        Type::Prim(p) => {
            let atype = match *p {
                "boolean" => 4u8,
                "char" => 5,
                "float" => 6,
                "double" => 7,
                "byte" => 8,
                "short" => 9,
                "int" => 10,
                "long" => 11,
                _ => return err(format!("数组初始化不支持元素类型 `{p}`")),
            };
            em.op(0xBC); // newarray
            em.bytes.push(atype);
        }
        _ => {
            let Some(cls) = type_internal(elem) else {
                return err(format!("数组初始化不支持的元素类型: {elem:?}"));
            };
            let idx = cp.class(&cls);
            em.op_u2(0xBD, idx); // anewarray
        }
    }
    em.pop(1); // newarray/anewarray 弹掉长度
    em.push(1); // 压回数组引用
    let store_op = match elem {
        Type::Prim("long") => 0x50,
        Type::Prim("float") => 0x51,
        Type::Prim("double") => 0x52,
        Type::Prim("boolean") | Type::Prim("byte") => 0x54,
        Type::Prim("char") => 0x55,
        Type::Prim("short") => 0x56,
        Type::Prim(_) => 0x4F,
        _ => 0x53, // aastore
    };
    let wide = matches!(elem, Type::Prim("long") | Type::Prim("double"));
    for (i, item) in items.iter().enumerate() {
        em.op(0x59); // dup（数组引用）
        em.push(1);
        em.const_int(i as i32);
        let sp = if needs_scratch(item) { Some(spill(item, em, ctx, cp)?) } else { None };
        emit_val(item, &sp, em, ctx, cp)?;
        coerce_to(elem, item, ctx, em, cp)?;
        em.op(store_op);
        em.pop(if wide { 4 } else { 3 });
    }
    Ok(())
}

/// 在**已知声明类型**的位置编一个初始化表达式：`int[] a = {1,2,3};` 的元素类型
/// 只能从声明处拿到（`ArrayInit` 本身不带类型），其余表达式照常编。
pub(crate) fn emit_init(
    ty: &Type,
    e: &Expr,
    em: &mut Emit,
    ctx: &Ctx,
    cp: &mut CpBuilder,
) -> Result<(), CompileError> {
    if let (Type::Array(_), Expr::ArrayInit(items)) = (ty, e) {
        let elem = array_init_elem(items, Some(ty), ctx);
        return emit_array_init(&elem, items, em, ctx, cp);
    }
    compile_expr(e, em, ctx, cp)
}

/// Java 整数字面量 → i64：十进制 / `0x` 十六进制 / `0b` 二进制 / `0` 开头八进制，
/// 允许 `_` 分隔符与 `L` 后缀。`0xFFFFFFFF` 这类「超出 int 但合法」的按位截断成 i32。
pub(crate) fn parse_int_literal(s: &str) -> Option<i64> {
    let t: String = s.trim().chars().filter(|c| *c != '_').collect();
    let (body, radix) = if let Some(r) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        (r, 16)
    } else if let Some(r) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
        (r, 2)
    } else if t.len() > 1 && t.starts_with('0') && t.chars().all(|c| c.is_ascii_digit() || c == 'L' || c == 'l') {
        (&t[1..], 8)
    } else {
        (&t[..], 10)
    };
    let body = body.trim_end_matches(['L', 'l']);
    if body.is_empty() {
        return Some(0);
    }
    i64::from_str_radix(body, radix).ok()
}

/// 表达式可不可以「再算一次」：读-改-写要把 lvalue 的接收者算两遍（读一次、写一次），
/// 只有纯表达式才等价于 Java 的「接收者只求值一次」。
pub(crate) fn is_pure_readable(e: &Expr) -> bool {
    match e {
        Expr::Ident(_)
        | Expr::This
        | Expr::Super
        | Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::StrLit(_)
        | Expr::TextBlock(_)
        | Expr::CharLit(_)
        | Expr::BoolLit(_)
        | Expr::Null => true,
        Expr::Field(r, _) => is_pure_readable(r),
        Expr::ArrayAccess { array, index } => is_pure_readable(array) && is_pure_readable(index),
        _ => false,
    }
}

/// `x++` / `--x` → `x = x ± 1`。值没人要的场景（语句、for 的 update）都走这里：
/// 赋值语句那几条 arm 已经处理了「数组元素 / 字段 / 局部变量」三种 lvalue。
pub(crate) fn inc_dec_to_assign(op: UnOp, target: &Expr) -> Result<Expr, CompileError> {
    let bin = match op {
        UnOp::PreInc | UnOp::PostInc => BinOp::Add,
        UnOp::PreDec | UnOp::PostDec => BinOp::Sub,
        _ => return err("inc_dec_to_assign：不是自增/自减"),
    };
    if !is_pure_readable(target) {
        return err(format!(
            "自增/自减的左侧要能重算一次（读一次写一次），这里做不到：{target:?}"
        ));
    }
    Ok(Expr::Assign {
        op: None,
        lhs: Box::new(target.clone()),
        rhs: Box::new(Expr::Binary {
            op: bin,
            lhs: Box::new(target.clone()),
            rhs: Box::new(Expr::IntLit("1".into())),
        }),
    })
}

/// `x += e` → `x = x + e`。隐式窄化由赋值处的 `coerce_to` 负责（JLS 15.26.2 的语义），
/// 字符串拼接则由 `Binary Add` 的 concat 分支负责。
pub(crate) fn compound_to_assign(op: BinOp, lhs: &Expr, rhs: &Expr) -> Result<Expr, CompileError> {
    if !is_pure_readable(lhs) {
        return err(format!(
            "复合赋值的左侧要能重算一次（读一次写一次），这里做不到：{lhs:?}"
        ));
    }
    Ok(Expr::Assign {
        op: None,
        lhs: Box::new(lhs.clone()),
        rhs: Box::new(Expr::Binary {
            op,
            lhs: Box::new(lhs.clone()),
            rhs: Box::new(rhs.clone()),
        }),
    })
}

/// 一个「先算进脏槽、稍后再取回来」的值
pub(crate) struct Spilled {
    slot: u8,
    kind: u8,
    /// 借之前该槽的验证类型，还的时候恢复回去
    saved: u16,
}

/// 把表达式算进脏槽（要求当前操作数栈为空），返回取回用的句柄
pub(crate) fn spill(
    e: &Expr,
    em: &mut Emit,
    ctx: &Ctx,
    cp: &mut CpBuilder,
) -> Result<Spilled, CompileError> {
    let kind = expr_kind(e, ctx);
    let slot = em.alloc_scratch()?;
    let saved = em.local_type_of(slot);
    compile_expr(e, em, ctx, cp)?;
    em.store_slot(slot, kind)?;
    // 槽的验证类型：能推出静态类型就用它，推不出来按种类给最保守的那个
    // （引用一律声明成 Object，局部变量表里的引用类型不参与 getfield 校验）
    let vt = match (expr_type(e, ctx), kind) {
        (Some(t), _) => ver_type(cp, &t),
        (None, K_LONG) => 0xFFFE,
        (None, K_DOUBLE) => 0xFFFD,
        (None, K_FLOAT) => 0xFFFC,
        (None, K_REF) => cp.class("java/lang/Object"),
        (None, _) => 0,
    };
    em.set_local_type(slot as usize, vt);
    Ok(Spilled { slot, kind, saved })
}

/// 把脏槽里的值取回操作数栈，并归还槽
pub(crate) fn unspill(sp: &Spilled, em: &mut Emit) -> Result<(), CompileError> {
    em.load_slot(sp.slot, sp.kind)?;
    em.set_local_type(sp.slot as usize, sp.saved);
    em.free_scratch(sp.slot);
    Ok(())
}

/// 实参里凡是「会分叉」的先算进脏槽：必须在压接收者/前面的实参**之前**做
fn hoist_args(
    args: &[Expr],
    em: &mut Emit,
    ctx: &Ctx,
    cp: &mut CpBuilder,
) -> Result<Vec<Option<Spilled>>, CompileError> {
    let mut out = Vec::with_capacity(args.len());
    for a in args {
        out.push(if needs_scratch(a) { Some(spill(a, em, ctx, cp)?) } else { None });
    }
    Ok(out)
}

/// 按 `hoist_args` 的结果编一个实参（取回脏槽里的值，或当场求值）
pub(crate) fn emit_val(
    a: &Expr,
    sp: &Option<Spilled>,
    em: &mut Emit,
    ctx: &Ctx,
    cp: &mut CpBuilder,
) -> Result<(), CompileError> {
    match sp {
        Some(s) => unspill(s, em),
        None => compile_expr(a, em, ctx, cp),
    }
}

/// 实参的静态类型看着就是那个数组（`m(new String[]{"a"})` 不该再包一层）
fn arg_looks_like_array(a: &Expr, param_desc: &str, ctx: &Ctx) -> bool {
    match expr_type(a, ctx) {
        Some(t) => {
            let d = descriptor(&t);
            d == param_desc || d == "Ljava/lang/Object;"
        }
        // 推不出来时按种类判：非引用的值（`sum(1)` 的 1）不可能是数组，必须打包
        None => expr_kind(a, ctx) == K_REF,
    }
}

/// 发一组实参：可变参数的尾部打包成数组 + 泛型擦除后的隐式 checkcast。
///
/// 压的槽数**必须**正好等于 `desc_slots(desc)`，调用点的栈深记账（`em.pop`）依赖这一点。
pub(crate) fn emit_call_args(
    args: &[Expr],
    desc: &str,
    sp: &[Option<Spilled>],
    em: &mut Emit,
    ctx: &Ctx,
    cp: &mut CpBuilder,
) -> Result<(), CompileError> {
    let params = desc_params(desc);
    let n = params.len();
    let argc = args.len();
    let mut fixed = argc;
    let mut pack: Option<Type> = None;
    if n > 0 && params[n - 1].starts_with('[') && argc >= n - 1 {
        let pdesc = params[n - 1].clone();
        let many = argc > n;
        // 实参个数刚好等于形参个数：给的是单元素（`m("a")`）还是整个数组（`m(arr)`）
        let single = argc == n && !arg_looks_like_array(&args[n - 1], &pdesc, ctx);
        // 一个实参都没给（`m()`）：形参比实参多一个且最后一个是数组 → 打包成空数组。
        // 这里不做类型检查，所以 `m(String[] a)` 被写成 `m()` 也会被当成可变参数
        // （已知的宽松；报错会变成静默容忍，但正常代码不受影响）。
        let empty = argc + 1 == n;
        if many || single || empty {
            fixed = n - 1;
            pack = Some(type_from_desc(&pdesc[1..]));
        }
    }
    for (i, a) in args.iter().enumerate().take(fixed.min(argc)) {
        emit_val(a, &sp[i], em, ctx, cp)?;
        if let Some(pd) = params.get(i) {
            let target = type_from_desc(pd);
            coerce_to(&target, a, ctx, em, cp)?;
        }
    }
    if let Some(elem) = pack {
        pack_tail(&args[fixed.min(argc)..], &sp[fixed.min(argc)..], &elem, em, ctx, cp)?;
    }
    Ok(())
}

/// 把尾部实参装成数组压栈（栈上净增 1 槽）。
///
/// 每个元素都从脏槽取（`hoist_args` 已经把会分叉的表达式算好了），
/// 所以 `aastore` 之前没有跳转点 —— 栈图上「数组还在栈上」这件事是安全的。
fn pack_tail(
    tail: &[Expr],
    sp: &[Option<Spilled>],
    elem: &Type,
    em: &mut Emit,
    ctx: &Ctx,
    cp: &mut CpBuilder,
) -> Result<(), CompileError> {
    em.const_int(tail.len() as i32);
    let store: u8 = match elem {
        Type::Prim("int") => {
            em.op(0xBC);
            em.bytes.push(10); // newarray T_INT
            0x4F
        }
        Type::Prim("long") => {
            em.op(0xBC);
            em.bytes.push(11);
            0x50
        }
        Type::Prim("double") => {
            em.op(0xBC);
            em.bytes.push(7);
            0x52
        }
        Type::Prim("boolean") => {
            em.op(0xBC);
            em.bytes.push(4);
            0x54
        }
        Type::Prim("char") => {
            em.op(0xBC);
            em.bytes.push(5);
            0x55
        }
        Type::Prim("byte") => {
            em.op(0xBC);
            em.bytes.push(8);
            0x54
        }
        Type::Prim("short") => {
            em.op(0xBC);
            em.bytes.push(9);
            0x56
        }
        _ => {
            let cls = ver_type(cp, elem);
            em.op_u2(0xBD, cls); // anewarray
            0x53
        }
    };
    em.pop(1);
    em.push(1);
    for (i, a) in tail.iter().enumerate() {
        em.op(0x59); // dup
        em.push(1);
        em.const_int(i as i32);
        emit_val(a, &sp[i], em, ctx, cp)?;
        em.op(store);
        em.pop(3);
    }
    Ok(())
}

/// 按实参挑构造器描述符。
///
/// 泛型擦除后 `new GenBox<>("x")` 的构造器是 `(Ljava/lang/Object;)V` ——
/// 拿实参类型现拼出来的是 `(Ljava/lang/String;)V`，那个方法根本不存在
/// （HotSpot 的报错是 `NoSuchMethodError: 'void GenBox.<init>(java.lang.String)'`）。
/// 所以先按「精确匹配」试，再退回「形参个数相同里最贴的那个」。
fn pick_ctor(owner: &str, args: &[Expr], ctx: &Ctx) -> Option<String> {
    let list = ctx.type_ctors.get(owner)?;
    let exact = format!("({})V", args_desc(args, ctx));
    if list.iter().any(|d| d == &exact) {
        return Some(exact);
    }
    let arg_descs: Vec<String> = args
        .iter()
        .map(|a| expr_type(a, ctx).map(|t| descriptor(&t)).unwrap_or_default())
        .collect();
    let mut best: Option<(i32, String)> = None;
    for d in list.iter() {
        let ps = desc_params(d);
        let n = ps.len();
        let mut score = -1i32;
        if n == args.len() {
            score = ps.iter().enumerate().filter(|(i, p)| arg_descs.get(*i) == Some(*p)).count() as i32;
        } else if n > 0 && ps[n - 1].starts_with('[') && args.len() + 1 >= n {
            score = 0; // 可变参数构造器
        }
        if score < 0 {
            continue;
        }
        if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            best = Some((score, d.clone()));
        }
    }
    best.map(|(_, d)| d)
}

pub(crate) fn compile_expr(e: &Expr, em: &mut Emit, ctx: &Ctx, cp: &mut CpBuilder) -> Result<(), CompileError> {
    match e {
        Expr::IntLit(s) => {
            // 十进制 / 0x / 0b / 0 开头八进制、下划线分隔、L 后缀都在这里归一
            let Some(v) = parse_int_literal(s) else {
                return err(format!("整数字面量 `{s}` 解析失败"));
            };
            if s.ends_with(['L', 'l']) {
                if v == 0 || v == 1 {
                    em.op(0x09 + v as u8); // lconst_0/1
                } else {
                    let idx = cp.long(v);
                    em.op_u2(0x14, idx); // ldc2_w
                }
                em.push(2);
                return Ok(());
            }
            let v32 = v as i32;
            if (-32768..=32767).contains(&v32) {
                em.const_int(v32);
            } else {
                // 超出 sipush 范围，走常量池 ldc_w
                let idx = cp.int(v32);
                em.op_u2(0x13, idx);
                em.push(1);
            }
            Ok(())
        }
        // 字符字面量：字节码里就是它的码点（char 与 int 同一套指令）
        Expr::CharLit(s) => {
            let c = s.chars().next().unwrap_or(0 as char) as i32;
            em.const_int(c);
            Ok(())
        }
        Expr::This => {
            em.op(0x2A); // aload_0
            em.push(1);
            Ok(())
        }
        Expr::Super => err("super 只能用在 super(...)、super.m()、super.x 里"),
        Expr::Field(recv, name) if name == "length" && !ctx.fields.contains_key(name) => {
            compile_expr(recv, em, ctx, cp)?;
            em.op(0xBE); // arraylength
            Ok(())
        }
        Expr::Field(recv, name) => {
            // 静态字段：`StaticCounter.count`（接收者是类名，不是对象）
            if let Some((owner, desc)) = static_field_access(e, ctx) {
                let f = cp.field_ref(&owner, name, &desc);
                em.op_u2(0xB2, f); // getstatic
                em.push(if kind_wide(kind_from_desc(&desc)) { 2 } else { 1 });
                return Ok(());
            }
            // super.x → 读父类字段，接收者还是 this
            let is_super = matches!(**recv, Expr::Super);
            if is_super {
                em.op(0x2A); // aload_0
                em.push(1);
            } else {
                compile_expr(recv, em, ctx, cp)?;
            }
            let (mut owner, desc) = field_access(recv, name, ctx);
            if is_super {
                owner = ctx.super_name.clone();
            }
            let f = cp.field_ref(&owner, name, &desc);
            em.op_u2(0xB4, f); // getfield
            em.pop(1);
            em.push(if kind_wide(kind_from_desc(&desc)) { 2 } else { 1 });
            Ok(())
        }
        Expr::ArrayNew { elem, dims } => {
            if dims.is_empty() {
                return err("数组创建至少要给一维长度");
            }
            for d in dims {
                compile_expr(d, em, ctx, cp)?;
            }
            // 多维：multianewarray，操作数是**整个数组**的描述符与维数
            if dims.len() > 1 {
                let mut ty = Type::Array(Box::new(elem.clone()));
                for _ in 1..dims.len() {
                    ty = Type::Array(Box::new(ty));
                }
                let cls = ver_type(cp, &ty);
                em.op(0xC5); // multianewarray
                em.bytes.extend_from_slice(&cls.to_be_bytes());
                em.bytes.push(dims.len() as u8);
                em.pop(dims.len() as u16);
                em.push(1);
                return Ok(());
            }
            // newarray 的操作数是元素类型码，引用元素走 anewarray（操作数是元素类）
            let atype = match elem {
                Type::Prim("int") => Some(10u8),    // T_INT
                Type::Prim("long") => Some(11),     // T_LONG
                Type::Prim("double") => Some(7),    // T_DOUBLE
                Type::Prim("boolean") => Some(4),   // T_BOOLEAN
                Type::Prim("char") => Some(5),      // T_CHAR
                Type::Prim("float") => Some(6),     // T_FLOAT
                Type::Prim("byte") => Some(8),      // T_BYTE
                Type::Prim("short") => Some(9),     // T_SHORT
                _ => None,
            };
            match atype {
                Some(a) => {
                    em.op(0xBC); // newarray
                    em.bytes.push(a);
                }
                None => {
                    if kind_of(elem) != K_REF {
                        return err(format!("不支持的数组元素类型: {elem:?}"));
                    }
                    let cls = ver_type(cp, elem);
                    em.op_u2(0xBD, cls); // anewarray
                }
            }
            em.pop(1);
            em.push(1);
            Ok(())
        }
        Expr::ArrayAccess { array, index } => {
            let k = elem_kind(array, ctx);
            let sa = if needs_scratch(array) { Some(spill(array, em, ctx, cp)?) } else { None };
            let si = if needs_scratch(index) { Some(spill(index, em, ctx, cp)?) } else { None };
            emit_val(array, &sa, em, ctx, cp)?;
            emit_val(index, &si, em, ctx, cp)?;
            em.op(array_ops(array, ctx).0);
            em.pop(2);
            em.push(if kind_wide(k) { 2 } else { 1 });
            Ok(())
        }
        Expr::New { ty, args } => {
            let owner = match ty {
                // java.lang 的类型可以简写：`new RuntimeException("x")` 要补成 java/lang/RuntimeException
                Type::Named { path, .. } => internal_name(path),
                _ => return err("只支持创建对象"),
            };
            let ctor_desc = pick_ctor(&owner, args, ctx).unwrap_or_else(|| format!("({})V", args_desc(args, ctx)));
            let cls = cp.class(&owner);
            let sp = hoist_args(args, em, ctx, cp)?;
            em.op_u2(0xBB, cls); // new
            em.push(1);
            em.op(0x59); // dup
            em.push(1);
            emit_call_args(args, &ctor_desc, &sp, em, ctx, cp)?;
            let init = cp.method_ref(&owner, "<init>", &ctor_desc);
            em.op_u2(0xB7, init); // invokespecial
            em.pop(desc_slots(&ctor_desc) + 1);
            Ok(())
        }
        Expr::Cast { ty, expr } => {
            // `new T[]{…}`：Cast 只是把元素类型带进来（parser 的包装），不是数值转换
            if let (Type::Array(_), Expr::ArrayInit(_)) = (ty, &**expr) {
                return emit_init(ty, expr, em, ctx, cp);
            }
            let from = expr_kind(expr, ctx);
            let to = kind_of(ty);
            compile_expr(expr, em, ctx, cp)?;
            if to == K_REF {
                // 引用转换不是数值转换：要发 checkcast（失败抛 ClassCastException）
                if from != K_REF {
                    return err("只能把引用值转成引用类型");
                }
                let cls = ver_type(cp, ty);
                em.op_u2(0xC0, cls); // checkcast
            } else {
                coerce(from, to, em)?;
            }
            Ok(())
        }
        Expr::InstanceOf { expr, ty, binding } => {
            if binding.is_some() {
                return err("暂不支持 instanceof 模式绑定（if (o instanceof T t)）");
            }
            compile_expr(expr, em, ctx, cp)?;
            let cls = ver_type(cp, ty);
            em.op_u2(0xC1, cls); // instanceof
            em.pop(1);
            em.push(1); // 压 0/1
            Ok(())
        }
        Expr::Null => {
            em.op(0x01); // aconst_null
            em.push(1);
            Ok(())
        }
        Expr::BoolLit(b) => {
            em.const_int(if *b { 1 } else { 0 });
            Ok(())
        }
        Expr::FloatLit(s) => {
            let raw = s.trim_end_matches(['d', 'D', 'f', 'F']);
            if is_float_suffix(s) {
                // Float 常量：精度必须按 f32 收（`1.0f/3.0f` 的十进制形态与 double 不同）
                let v: f32 = raw
                    .parse()
                    .map_err(|_| CompileError { msg: format!("float 字面量 `{s}` 解析失败") })?;
                em.const_float(v, cp);
                return Ok(());
            }
            let v: f64 = raw
                .parse()
                .map_err(|_| CompileError { msg: format!("浮点字面量 `{s}` 解析失败") })?;
            if v == 0.0 && !v.is_sign_negative() {
                em.op(0x0E); // dconst_0
            } else if v == 1.0 {
                em.op(0x0F); // dconst_1
            } else {
                let idx = cp.double(v);
                em.op_u2(0x14, idx); // ldc2_w
            }
            em.push(2);
            Ok(())
        }
        Expr::StrLit(s) => {
            let idx = cp.string(s);
            em.op_u2(0x13, idx);
            em.push(1);
            Ok(())
        }
        Expr::Ident(name) => {
            if let Some(slot) = ctx.lookup(name) {
                let kind = *ctx.local_kind.get(name).unwrap_or(&K_INT);
                em.load_slot(slot, kind)?;
                return Ok(());
            }
            // 隐式 this 字段
            if let Some(idx) = ctx.fields.get(name).copied() {
                em.op(0x2A); // aload_0
                em.push(1);
                let desc = ctx.field_descs[idx].clone();
                let owner = ctx.class_name.clone();
                let f = cp.field_ref(&owner, name, &desc);
                em.op_u2(0xB4, f); // getfield
                // 栈深要按字段宽度记账：少算一槽会让 max_stack 偏小（VerifyError）
                em.pop(1);
                em.push(if kind_wide(kind_from_desc(&desc)) { 2 } else { 1 });
                return Ok(());
            }
            // 静态字段（本类或继承来的）
            if let Some((owner, desc)) = ctx.static_fields.get(name).cloned() {
                let f = cp.field_ref(&owner, name, &desc);
                em.op_u2(0xB2, f); // getstatic
                em.push(if kind_wide(kind_from_desc(&desc)) { 2 } else { 1 });
                return Ok(());
            }
            err(format!("未定义的变量 `{name}`"))
        }
        // System.out.println(x)
        Expr::Call { recv: Some(r), name, args, .. } if name == "println" => {
            if let Expr::Field(boxed, field) = &**r {
                if field == "out" {
                    if let Expr::Ident(_) = &**boxed {
                        let out = cp.field_ref("java/lang/System", "out", "Ljava/io/PrintStream;");
                        if args.len() != 1 {
                            return err("println 目前只支持一个参数");
                        }
                        // 实参内部有跳转（`println(a == b)`）时先算进脏槽：
                        // PrintStream 一旦压上栈，比较的跳转点就压着它了
                        let sp = hoist_args(args, em, ctx, cp)?;
                        em.op_u2(0xB2, out);
                        em.push(1);
                        emit_val(&args[0], &sp[0], em, ctx, cp)?;
                        // 按实参类型选重载：int → (I)V，long → (J)V，
                        // double → (D)V，char → (C)V，引用 → (Ljava/lang/String;)V
                        // char 必须走 (C)V：打成 (I)V 会输出码点而不是字符
                        let is_char = matches!(expr_type(&args[0], ctx), Some(Type::Prim("char")));
                        let (desc, width) = if is_char {
                            ("(C)V", 1u16)
                        } else {
                            match expr_kind(&args[0], ctx) {
                                K_LONG => ("(J)V", 2u16),
                                K_DOUBLE => ("(D)V", 2),
                                K_FLOAT => ("(F)V", 1),
                                K_REF => (ref_println_desc(&args[0], ctx), 1),
                                K_BOOL => ("(Z)V", 1),
                                _ => ("(I)V", 1),
                            }
                        };
                        let println = cp.method_ref("java/io/PrintStream", "println", desc);
                        em.op_u2(0xB6, println);
                        em.pop(1 + width); // 接收者 + 实参占的槽数
                        return Ok(());
                    }
                }
            }
            err(format!("不支持的 println 形态: {name}"))
        }
        // this(...) → invokespecial 本类另一个 <init>（构造器委托）
        Expr::Call { recv: None, name, args, .. } if name == "this" => {
            if !ctx.in_ctor {
                return err("this(...) 只能出现在构造器里");
            }
            let desc = pick_ctor(&ctx.class_name, args, ctx)
                .unwrap_or_else(|| format!("({})V", args_desc(args, ctx)));
            let sp = hoist_args(args, em, ctx, cp)?;
            em.op(0x2A); // aload_0
            em.push(1);
            emit_call_args(args, &desc, &sp, em, ctx, cp)?;
            let idx = cp.method_ref(&ctx.class_name, "<init>", &desc);
            em.op_u2(0xB7, idx); // invokespecial
            em.pop(desc_slots(&desc) + 1);
            Ok(())
        }
        // super(...) → invokespecial 父类 <init>（父类构造器）
        Expr::Call { recv: Some(r), name, args, .. } if name == "super" && matches!(**r, Expr::Super) => {
            if !ctx.in_ctor {
                return err("super(...) 只能出现在构造器里");
            }
            let desc = pick_ctor(&ctx.super_name, args, ctx)
                .unwrap_or_else(|| format!("({})V", args_desc(args, ctx)));
            let sp = hoist_args(args, em, ctx, cp)?;
            em.op(0x2A); // aload_0
            em.push(1);
            emit_call_args(args, &desc, &sp, em, ctx, cp)?;
            let idx = cp.method_ref(&ctx.super_name, "<init>", &desc);
            em.op_u2(0xB7, idx); // invokespecial
            em.pop(desc_slots(&desc) + 1);
            Ok(())
        }
        // super.m() → invokespecial 父类实现（owner 必须是父类，否则解析到自己）
        Expr::Call { recv: Some(r), name, args, .. } if matches!(**r, Expr::Super) => {
            let desc = call_desc(Some(r), name, args, ctx)
                .ok_or_else(|| CompileError { msg: format!("父类里未找到方法 `{name}`") })?;
            let sp = hoist_args(args, em, ctx, cp)?;
            em.op(0x2A); // aload_0
            em.push(1);
            emit_call_args(args, &desc, &sp, em, ctx, cp)?;
            let idx = cp.method_ref(&ctx.super_name, name, &desc);
            em.op_u2(0xB7, idx); // invokespecial
            em.pop(desc_slots(&desc) + 1);
            if !desc.ends_with(")V") {
                em.push(if kind_wide(kind_from_ret(&desc)) { 2 } else { 1 });
            }
            Ok(())
        }
        // 调用对象方法 a.get()
        Expr::Call { recv: Some(r), name, args, .. } => {
            // 静态方法：`ClassName.m(...)` → invokestatic（owner 就是那个类）
            if let Some((owner, desc)) = static_call(r, name, args, ctx) {
                let sp = hoist_args(args, em, ctx, cp)?;
                emit_call_args(args, &desc, &sp, em, ctx, cp)?;
                // 接口的静态方法必须引用 InterfaceMethodref（否则 JVM 报
                // "must be InterfaceMethodref constant"）
                let idx = if ctx.iface_types.contains(&owner) {
                    cp.interface_method_ref(&owner, name, &desc)
                } else {
                    cp.method_ref(&owner, name, &desc)
                };
                em.op_u2(0xB8, idx); // invokestatic
                em.pop(desc_slots(&desc));
                if !desc.ends_with(")V") {
                    em.push(if kind_wide(kind_from_ret(&desc)) { 2 } else { 1 });
                }
                return Ok(());
            }
            let desc = call_desc(Some(r), name, args, ctx)
                .ok_or_else(|| CompileError { msg: format!("未找到实例方法 `{name}`") })?;
            let sp = hoist_args(args, em, ctx, cp)?;
            compile_expr(r, em, ctx, cp)?;
            emit_call_args(args, &desc, &sp, em, ctx, cp)?;
            // 引用类用接收者的**静态类型**：验证器要求接收者可赋给引用类。
            // 分派本身按运行时类走（虚方法覆写），引用类只决定从哪条链开始解析。
            let owner = expr_type(r, ctx).as_ref().and_then(type_internal).unwrap_or_else(|| ctx.class_name.clone());
            let n = args.len() as u16 + 1;
            if ctx.iface_types.contains(&owner) || is_builtin_iface(&owner) {
                // 接口方法调用必须是 invokeinterface：index, count(含 this), 0
                let idx = cp.interface_method_ref(&owner, name, &desc);
                em.op(0xB9);
                em.bytes.extend_from_slice(&idx.to_be_bytes());
                em.bytes.push(n as u8);
                em.bytes.push(0);
            } else {
                let idx = cp.method_ref(&owner, name, &desc);
                em.op_u2(0xB6, idx); // invokevirtual
            }
            em.pop(desc_slots(&desc) + 1);
            // 返回值占的槽数：long/double 占 2 槽
            if !desc.ends_with(")V") {
                em.push(if kind_wide(kind_from_ret(&desc)) { 2 } else { 1 });
            }
            Ok(())
        }
        // 调用本类（或父类）的方法：静态走 invokestatic，实例走 this 的 invokevirtual
        Expr::Call { name, args, .. } => {
            if let Some(desc) = ctx.static_methods.get(name.as_str()).cloned() {
                let sp = hoist_args(args, em, ctx, cp)?;
                emit_call_args(args, &desc, &sp, em, ctx, cp)?;
                let idx = cp.method_ref("<SELF>", name, &desc);
                em.op_u2(0xB8, idx);
                em.pop(desc_slots(&desc));
                if !desc.ends_with(")V") {
                    // 返回值占的槽数：long/double 占 2 槽
                    em.push(if kind_wide(kind_from_ret(&desc)) { 2 } else { 1 });
                }
                return Ok(());
            }
            // 无接收者的实例方法调用 = `this.m()`（父类的实现也算本类的可见方法）
            let desc = ctx
                .instance_methods
                .get(name.as_str())
                .ok_or_else(|| CompileError { msg: format!("未找到方法 `{name}`") })?
                .clone();
            let sp = hoist_args(args, em, ctx, cp)?;
            em.op(0x2A); // aload_0
            em.push(1);
            emit_call_args(args, &desc, &sp, em, ctx, cp)?;
            let n = args.len() as u16 + 1;
            // 接口里的 default 方法调另一个方法（`area()` = this.area()）：
            // 自己就是接口时必须是 invokeinterface，否则 JVM 报
            // "Found interface Shape, but class was expected"
            if ctx.iface_types.contains(&ctx.class_name) {
                let idx = cp.interface_method_ref(&ctx.class_name, name, &desc);
                em.op(0xB9);
                em.bytes.extend_from_slice(&idx.to_be_bytes());
                em.bytes.push(n as u8);
                em.bytes.push(0);
            } else {
                let idx = cp.method_ref(&ctx.class_name, name, &desc);
                em.op_u2(0xB6, idx); // invokevirtual
            }
            em.pop(desc_slots(&desc) + 1);
            if !desc.ends_with(")V") {
                em.push(if kind_wide(kind_from_ret(&desc)) { 2 } else { 1 });
            }
            Ok(())
        }
        // 字符串拼接：`"a" + b + "c"` —— 任一操作数是 String 就走这里（Java 语义）
        Expr::Binary { op: BinOp::Add, .. } if is_string_val(e, ctx) => emit_concat(e, em, ctx, cp),
        // `&&` / `||` 作为值：短路求值成 0/1
        Expr::Binary { op: lop @ (BinOp::And | BinOp::Or), lhs, rhs } => {
            compile_logical(*lop, lhs, rhs, em, ctx, cp)
        }
        Expr::Binary { op, lhs, rhs } => {
            let kl = num_kind(lhs, ctx);
            let kr = num_kind(rhs, ctx);
            // 比较作为**表达式**（`boolean f = a < b;`）：产出 0/1
            if let Some((jump_false, _)) = compile_cmp_operands(e, em, ctx, cp)? {
                em.bool_from_branch(jump_false)?;
                return Ok(());
            }
            // 移位：右操作数恒为 int，左操作数决定是 int 还是 long 版本
            if matches!(op, BinOp::Shl | BinOp::Shr | BinOp::Ushr) {
                let k = if kl == K_LONG { K_LONG } else { K_INT };
                let sr = if needs_scratch(rhs) { Some(spill(rhs, em, ctx, cp)?) } else { None };
                emit_num(lhs, em, ctx, cp)?;
                coerce(kl, k, em)?;
                emit_num_val(rhs, &sr, em, ctx, cp)?;
                coerce(kr, K_INT, em)?;
                let code = match (op, k) {
                    (BinOp::Shl, K_LONG) => 0x79u8, // lshl
                    (BinOp::Shr, K_LONG) => 0x7B,   // lshr
                    (BinOp::Ushr, K_LONG) => 0x7D,  // lushr
                    (BinOp::Shl, _) => 0x78,        // ishl
                    (BinOp::Shr, _) => 0x7A,        // ishr
                    _ => 0x7C,                      // iushr
                };
                em.op(code);
                em.pop(if kind_wide(k) { 2 } else { 1 });
                return Ok(());
            }
            let k = promote(kl, kr);
            // 右操作数会分叉（`x + (a<b ? …)` 之类）时先把它算进脏槽，别压在左值上面
            let sr = if needs_scratch(rhs) { Some(spill(rhs, em, ctx, cp)?) } else { None };
            emit_num(lhs, em, ctx, cp)?;
            coerce(kl, k, em)?;
            emit_num_val(rhs, &sr, em, ctx, cp)?;
            coerce(kr, k, em)?;
            let code = match (op, k) {
                (BinOp::Add, K_LONG) => 0x61u8,
                (BinOp::Sub, K_LONG) => 0x65,
                (BinOp::Mul, K_LONG) => 0x69,
                (BinOp::Div, K_LONG) => 0x6D,
                (BinOp::Rem, K_LONG) => 0x71,
                (BinOp::Add, K_DOUBLE) => 0x63,
                (BinOp::Sub, K_DOUBLE) => 0x67,
                (BinOp::Mul, K_DOUBLE) => 0x6B,
                (BinOp::Div, K_DOUBLE) => 0x6F,
                (BinOp::Rem, K_DOUBLE) => 0x73,
                (BinOp::Add, K_FLOAT) => 0x62,
                (BinOp::Sub, K_FLOAT) => 0x66,
                (BinOp::Mul, K_FLOAT) => 0x6A,
                (BinOp::Div, K_FLOAT) => 0x6E,
                (BinOp::Rem, K_FLOAT) => 0x72,
                (BinOp::Add, _) => 0x60u8,
                (BinOp::Sub, _) => 0x64,
                (BinOp::Mul, _) => 0x68,
                (BinOp::Div, _) => 0x6C,
                (BinOp::Rem, _) => 0x70,
                (BinOp::BitAnd, K_LONG) => 0x7F, // land
                (BinOp::BitOr, K_LONG) => 0x81,  // lor
                (BinOp::BitXor, K_LONG) => 0x83, // lxor
                (BinOp::BitAnd, _) => 0x7E,      // iand
                (BinOp::BitOr, _) => 0x80,       // ior
                (BinOp::BitXor, _) => 0x82,      // ixor
                (other, _) => return err(format!("暂不支持的二元运算: {other:?}")),
            };
            em.op(code);
            em.pop(if kind_wide(k) { 2 } else { 1 });
            Ok(())
        }
        Expr::Unary { op: UnOp::Neg, expr } => {
            let k = num_kind(expr, ctx);
            emit_num(expr, em, ctx, cp)?;
            match k {
                K_LONG => em.op(0x75),   // lneg
                K_DOUBLE => em.op(0x77), // dneg
                K_FLOAT => em.op(0x76),  // fneg
                _ => em.op(0x74),        // ineg
            }
            Ok(())
        }
        // ~x：int 走 iconst_m1 + ixor，long 走 ldc2_w -1 + lxor（JVM 没有按位取反指令）
        Expr::Unary { op: UnOp::BitNot, expr } => {
            let k = num_kind(expr, ctx);
            emit_num(expr, em, ctx, cp)?;
            if k == K_LONG {
                let idx = cp.long(-1);
                em.op_u2(0x14, idx);
                em.push(2);
                em.op(0x83); // lxor
                em.pop(3);
            } else {
                em.op(0x02); // iconst_m1
                em.push(1);
                em.op(0x82); // ixor
                em.pop(2);
            }
            Ok(())
        }
        // +x：无操作
        Expr::Unary { op: UnOp::Pos, expr } => emit_num(expr, em, ctx, cp).map(|_| ()),
        // !x：求 0/1 再翻过来（非零 → 0）
        Expr::Unary { op: UnOp::Not, expr } => {
            compile_expr(expr, em, ctx, cp)?;
            em.pop(1);
            em.bool_from_branch(0x9A)?; // ifne → 「假」= 原值非零
            Ok(())
        }
        // `i++` / `--i` 作为**值**用（`arr[i++]`、`return n--;`）：
        // 局部变量重读一次是安全的，所以「旧值 dup → 加一 → 存回」就行
        Expr::Unary { op: op @ (UnOp::PreInc | UnOp::PreDec | UnOp::PostInc | UnOp::PostDec), expr } => {
            compile_inc_dec(*op, expr, em, ctx, cp)
        }
        // `{1,2,3}` / `new int[]{1,2,3}`：AST 里不带元素类型，按字面量推断
        Expr::ArrayInit(items) => {
            let elem = array_init_elem(items, None, ctx);
            emit_array_init(&elem, items, em, ctx, cp)
        }
        // 赋值当**表达式**用：`if ((n = next()) >= 0)`、`x = y = z`。
        // 局部变量好办（值留在栈上即可）；字段/数组元素要保证「接收者只求值一次」，
        // 这里没做，所以那两种仍然报错 —— 报错比按错的顺序算要好。
        Expr::Assign { op: None, lhs, rhs } => {
            let Expr::Ident(name) = &**lhs else {
                return err(format!("赋值作为表达式时只支持局部变量：{lhs:?}"));
            };
            let Some(slot) = ctx.lookup(name) else {
                return err(format!("赋值作为表达式时目标不是局部变量：`{name}`"));
            };
            let kind = *ctx.local_kind.get(name).unwrap_or(&K_INT);
            compile_expr(rhs, em, ctx, cp)?;
            if let Some(t) = ctx.local_ty.get(name).cloned() {
                coerce_to(&t, rhs, ctx, em, cp)?;
            }
            em.op(if kind_wide(kind) { 0x5C } else { 0x59 }); // dup2 / dup
            em.push(if kind_wide(kind) { 2 } else { 1 });
            em.store_slot(slot, kind)?;
            Ok(())
        }
        // `c ? a : b`：分叉求值 → 各自写进同一个脏槽 → 汇合点取回。
        // 两个跳转点（假分支入口、汇合点）操作数栈都是空的。
        Expr::Cond { cond, then, els } => compile_cond(cond, then, els, em, ctx, cp),
        other => err(format!("暂不支持的表达式: {other:?}")),
    }
}

/// `a && b` / `a || b` 编成 0/1 的值：短路求值，两个跳转点都落在空栈上。
///
/// 形状（以 `&&` 为例）：
/// ```text
///   <left> ; istore s ; iload s ; ifeq L_short   // 左边假 → 直接出 0
///   <right>; istore s ; iload s ; ifeq L_short
///   iconst_1 ; istore s ; goto L_end
/// L_short: iconst_0 ; istore s
/// L_end  : iload s
/// ```
/// 每处跳转指令前面都刚 `iload` 完、`ifeq` 已把它弹掉，所以跳转点上栈是空的。
fn compile_inc_dec(
    op: UnOp,
    target: &Expr,
    em: &mut Emit,
    ctx: &Ctx,
    cp: &mut CpBuilder,
) -> Result<(), CompileError> {
    let post = matches!(op, UnOp::PostInc | UnOp::PostDec);
    let inc = matches!(op, UnOp::PreInc | UnOp::PostInc);
    // 表达式位置只支持局部变量：它重读安全。字段/数组元素的自增在**语句**位置会由
    // `compile_stmt` 降糖成赋值，那里接收者只求值一次；表达式位置（`a[i++]++`）没有
    // 真实需求，报错比悄悄按错的顺序算要好。
    let Expr::Ident(name) = target else {
        return err(format!("自增/自减作为表达式时只支持局部变量：{target:?}"));
    };
    let Some(slot) = ctx.lookup(name) else {
        return err(format!("自增/自减的目标不是局部变量：`{name}`"));
    };
    let kind = *ctx.local_kind.get(name).unwrap_or(&K_INT);
    let wide = kind_wide(kind);
    let (one, arith) = match kind {
        K_LONG => (0x0Au8, if inc { 0x61 } else { 0x65 }), // lconst_1 / ladd / lsub
        K_FLOAT => (0x0Cu8, if inc { 0x62 } else { 0x66 }), // fconst_1 / fadd / fsub
        K_DOUBLE => (0x0Fu8, if inc { 0x63 } else { 0x67 }), // dconst_1 / dadd / dsub
        K_INT => (0x04, if inc { 0x60 } else { 0x64 }),    // iconst_1 / iadd / isub
        _ => return err(format!("自增/自减的目标不是数值：`{name}`")),
    };
    em.load_slot(slot, kind)?;
    let dup = if wide { 0x5C } else { 0x59 }; // dup2 / dup
    let push_slots = if wide { 2 } else { 1 };
    if post {
        // post（`i++` 的值是旧值）：[old] → dup → [old][old] → +1 → [old][new] → store → [old]
        em.op(dup);
        em.push(push_slots);
        em.op(one);
        em.push(push_slots);
        em.op(arith);
        em.pop(push_slots);
        em.store_slot(slot, kind)?;
        Ok(())
    } else {
        // pre（`++i` 的值是新值）：[old] → +1 → [new] → dup → [new][new] → store → [new]
        em.op(one);
        em.push(push_slots);
        em.op(arith);
        em.pop(push_slots);
        em.op(dup);
        em.push(push_slots);
        em.store_slot(slot, kind)?;
        Ok(())
    }
}

fn compile_logical(
    op: BinOp,
    lhs: &Expr,
    rhs: &Expr,
    em: &mut Emit,
    ctx: &Ctx,
    cp: &mut CpBuilder,
) -> Result<(), CompileError> {
    if em.depth != 0 {
        return err("`&&`/`||` 前面不能有未消费的操作数（操作数栈必须为空）");
    }
    let slot = em.alloc_scratch()?;
    let saved = em.local_type_of(slot);
    let is_and = op == BinOp::And;
    // 短路条件：&& 遇假跳出，|| 遇真跳出
    let jump_short = if is_and { 0x99 } else { 0x9A };
    let l_short = em.new_label();
    let l_end = em.new_label();

    for (i, side) in [lhs, rhs].iter().enumerate() {
        compile_expr(side, em, ctx, cp)?;
        em.istore(slot)?;
        // 槽的验证类型必须**第一次存完之后**才登记：左侧表达式内部如果带跳转，
        // 那些帧里这个槽还没赋值过，抢先声明成 integer 会被判 VerifyError。
        if i == 0 {
            em.set_local_type(slot as usize, 0);
        }
        em.iload(slot)?;
        em.branch(jump_short, l_short);
        em.pop(1); // ifeq/ifne 把刚 load 出来的值弹掉了
    }
    // 两侧都没短路：&& → 1，|| → 0
    em.const_int(if is_and { 1 } else { 0 });
    let _ = em.istore(slot);
    em.goto(l_end);
    em.bind(l_short); // 两条短路路径都在跳之前存过 → 槽里一定是 int
    em.const_int(if is_and { 0 } else { 1 });
    let _ = em.istore(slot);
    em.bind(l_end);
    let _ = em.iload(slot);
    em.set_local_type(slot as usize, saved);
    em.free_scratch(slot);
    Ok(())
}

/// 编译一个比较表达式（`a < b`、`o == null`、`p != null`），
/// 返回 `(条件为假时跳的指令, 条件为真时跳的指令)`；操作数已被消费。
///
/// 不是比较表达式时返回 `None`（调用方按「求出 0/1 再 ifeq」兜底）。
pub(crate) fn compile_cmp_operands(
    e: &Expr,
    em: &mut Emit,
    ctx: &Ctx,
    cp: &mut CpBuilder,
) -> Result<Option<(u8, u8)>, CompileError> {
    let Expr::Binary { op, lhs, rhs } = e else { return Ok(None) };
    if !matches!(
        op,
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
    ) {
        return Ok(None);
    }
    let kl = expr_kind(lhs, ctx);
    let kr = expr_kind(rhs, ctx);
    // 一边是包装类、另一边是基本类型（`box == 5`）：按数值比较（Java 拆箱）
    let mixed_box = {
        let lw = expr_type(lhs, ctx).map(|t| is_wrapper_ty(&t)).unwrap_or(false);
        let rw = expr_type(rhs, ctx).map(|t| is_wrapper_ty(&t)).unwrap_or(false);
        (lw != rw) && ((kl == K_REF) != (kr == K_REF))
    };
    // 和 null 比较：ifnull / ifnonnull
    let l_null = matches!(**lhs, Expr::Null);
    let r_null = matches!(**rhs, Expr::Null);
    if l_null || r_null {
        if !(matches!(op, BinOp::Eq | BinOp::Ne)) {
            return err("null 只能做 == / != 比较");
        }
        let other = if l_null { rhs } else { lhs };
        compile_expr(other, em, ctx, cp)?;
        em.pop(1);
        return Ok(Some(if matches!(op, BinOp::Eq) {
            (0xC7, 0xC6) // 假 = 非空(ifnonnull)；真 = 空(ifnull)
        } else {
            (0xC6, 0xC7)
        }));
    }
    // 引用比较：if_acmpeq / if_acmpne（只有 == / != 有意义）
    if (kl == K_REF || kr == K_REF) && !mixed_box {
        if !(matches!(op, BinOp::Eq | BinOp::Ne)) {
            return err("引用类型只能比较 == / !=");
        }
        let sa = if needs_scratch(lhs) { Some(spill(lhs, em, ctx, cp)?) } else { None };
        let sr = if needs_scratch(rhs) { Some(spill(rhs, em, ctx, cp)?) } else { None };
        emit_val(lhs, &sa, em, ctx, cp)?;
        emit_val(rhs, &sr, em, ctx, cp)?;
        em.pop(2);
        return Ok(Some(if matches!(op, BinOp::Eq) {
            (0xA6, 0xA5) // 假 = if_acmpne；真 = if_acmpeq
        } else {
            (0xA5, 0xA6)
        }));
    }
    let kl = num_kind(lhs, ctx);
    let kr = num_kind(rhs, ctx);
    let k = promote(kl, kr);
    // 任何一侧自带跳转（`(a<b) == (c<d)`）都要先算进脏槽：否则跳转点上压着另一侧的值
    let sa = if needs_scratch(lhs) { Some(spill(lhs, em, ctx, cp)?) } else { None };
    let sr = if needs_scratch(rhs) { Some(spill(rhs, em, ctx, cp)?) } else { None };
    emit_num_val(lhs, &sa, em, ctx, cp)?;
    coerce(kl, k, em)?;
    emit_num_val(rhs, &sr, em, ctx, cp)?;
    coerce(kr, k, em)?;
    // long/double/float 要先 lcmp / dcmpl(dcmpg) / fcmpl(fcmpg) 压成 int，再用单值比较
    let widen = match k {
        K_LONG => Some(0x94u8), // lcmp
        // NaN 语义：< 和 <= 用 xxcmpg（NaN → +1，比较结果为假），其余用 xxcmpl
        K_DOUBLE => Some(if matches!(op, BinOp::Lt | BinOp::Le) { 0x98 } else { 0x97 }),
        K_FLOAT => Some(if matches!(op, BinOp::Lt | BinOp::Le) { 0x96 } else { 0x95 }),
        _ => None,
    };
    if let Some(c) = widen {
        em.op(c);
        // long/double 是两个宽值 4 槽 → 1 槽 int；float 是 2 槽 → 1 槽
        em.pop(if k == K_FLOAT { 1 } else { 3 });
        let ops = match op {
            BinOp::Lt => (0x9C, 0x9B), // ifge / iflt
            BinOp::Ge => (0x9B, 0x9C), // iflt / ifge
            BinOp::Gt => (0x9E, 0x9D), // ifle / ifgt
            BinOp::Le => (0x9D, 0x9E), // ifgt / ifle
            BinOp::Eq => (0x9A, 0x99), // ifne / ifeq
            _ => (0x99, 0x9A),         // ifeq / ifne
        };
        em.pop(1);
        return Ok(Some(ops));
    }
    let ops = match op {
        BinOp::Lt => (0xA2, 0xA1), // if_icmpge / if_icmplt
        BinOp::Ge => (0xA1, 0xA2), // if_icmplt / if_icmpge
        BinOp::Gt => (0xA4, 0xA3), // if_icmple / if_icmpgt
        BinOp::Le => (0xA3, 0xA4), // if_icmpgt / if_icmple
        BinOp::Eq => (0xA0, 0x9F), // if_icmpne / if_icmpeq
        _ => (0x9F, 0xA0),         // if_icmpeq / if_icmpne
    };
    em.pop(2);
    Ok(Some(ops))
}

/// 把 `a < b` 这类条件编成「条件为假时跳到 label」。
pub(crate) fn compile_cond_false(
    e: &Expr,
    em: &mut Emit,
    ctx: &Ctx,
    cp: &mut CpBuilder,
    label: usize,
) -> Result<(), CompileError> {
    if let Some((jump_false, _)) = compile_cmp_operands(e, em, ctx, cp)? {
        em.branch(jump_false, label);
        return Ok(());
    }
    // 兜底：任何产出 0/1 的表达式（instanceof、boolean 局部变量、boolean 方法返回值）
    // 都编成「求值 → ifeq 跳到 label」。跳转点上操作数栈是空的。
    // 条件是包装类（`Boolean ok; if (ok) …`）时先拆箱：直接 ifeq 一个引用会 VerifyError。
    let kind = num_kind(e, ctx);
    if kind_wide(kind) {
        return err("条件表达式不能是 long/double");
    }
    emit_num(e, em, ctx, cp)?;
    em.pop(1);
    em.branch(0x99, label); // ifeq
    Ok(())
}

/// 三元运算符的类型：两侧一致就用它；一侧是 null 字面量就用另一侧；
/// 一侧是 Object 就用 Object；都不是就按「前一个分支」的类型
/// （另一侧会补一条 checkcast，保证字节码能过验证）。
pub(crate) fn cond_type(then: &Expr, els: &Expr, ctx: &Ctx) -> Option<Type> {
    let tt = expr_type(then, ctx);
    let et = expr_type(els, ctx);
    let is_object = |t: &Type| matches!(t, Type::Named { path, .. } if path.last().map(|s| s.as_str()) == Some("Object"));
    match (tt, et) {
        (Some(a), Some(b)) => {
            if a == b {
                Some(a)
            } else if is_object(&a) {
                Some(a)
            } else if is_object(&b) {
                Some(b)
            } else if matches!(a, Type::Prim(_)) || matches!(b, Type::Prim(_)) {
                // 数值两侧类型不同：种类层面已经提升过，这里给个 int 只是为了占位
                Some(a)
            } else {
                Some(a)
            }
        }
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// 把栈顶的值强制成 `ty`（引用类型才发 checkcast；两侧声明类型不同时补一条）
fn cast_to(ty: &Type, from: Option<&Type>, em: &mut Emit, cp: &mut CpBuilder) -> Result<(), CompileError> {
    if let Some(f) = from {
        if f == ty {
            return Ok(());
        }
    }
    match ty {
        Type::Named { .. } | Type::Array(_) => {
            let cls = ver_type(cp, ty);
            em.op_u2(0xC0, cls); // checkcast
            Ok(())
        }
        _ => Ok(()),
    }
}

/// `cond ? then : els`
fn compile_cond(
    cond: &Expr,
    then: &Expr,
    els: &Expr,
    em: &mut Emit,
    ctx: &Ctx,
    cp: &mut CpBuilder,
) -> Result<(), CompileError> {
    let kt = expr_kind(then, ctx);
    let ke = expr_kind(els, ctx);
    // 结果种类：只要有一侧是引用就按引用处理；两侧都是 boolean 时保持 boolean
    // （否则 `println(flag ? true : false)` 会打成 1/0）
    let k = if kt == K_REF || ke == K_REF {
        K_REF
    } else if kt == K_BOOL && ke == K_BOOL {
        K_BOOL
    } else {
        promote(kt, ke)
    };
    let res_ty = if k == K_REF { cond_type(then, els, ctx) } else { None };
    let vt = match (&res_ty, k) {
        (Some(t), _) => ver_type(cp, t),
        (None, K_LONG) => 0xFFFE,
        (None, K_DOUBLE) => 0xFFFD,
        (None, K_FLOAT) => 0xFFFC,
        // 引用但推不出静态类型：按 Object 声明（和 `spill` 的兜底一致）
        (None, K_REF) => cp.class("java/lang/Object"),
        (None, _) => 0,
    };

    // 脏槽：和 `bool_from_branch` 用同一个（SCRATCH_SLOT）——它的帧声明规则
    // 已经被现有样例验证过，不用再造一套池槽的按路径声明。
    let slot = SCRATCH_SLOT;
    em.reserve_scratch_slot(kind_wide(k));
    let l_false = em.new_label();
    let l_end = em.new_label();

    compile_cond_false(cond, em, ctx, cp, l_false)?;
    compile_expr(then, em, ctx, cp)?;
    coerce(kt, k, em)?;
    if let Some(t) = res_ty.as_ref().filter(|_| k == K_REF) {
        cast_to(t, expr_type(then, ctx).as_ref(), em, cp)?;
    }
    em.store_slot(slot, k)?;
    // 注意：**不**改全局的 `local_types` —— 分支里还可能绑别的帧（嵌套三元/比较），
    // 全局值会被它们快照进去，造成「这条路径上没写的槽被声明成已写」。
    // 这个槽的验证类型只在下面两处按路径显式声明。
    em.goto(l_end);

    // 假分支：这个槽在这里还没写过（Top 接受往后的任何赋值）
    em.bind_t(l_false, Some(u16::MAX));
    compile_expr(els, em, ctx, cp)?;
    coerce(ke, k, em)?;
    if let Some(t) = res_ty.as_ref().filter(|_| k == K_REF) {
        cast_to(t, expr_type(els, ctx).as_ref(), em, cp)?;
    }
    em.store_slot(slot, k)?;

    // 汇合点：两条路径都存过同一个类型
    em.bind_t(l_end, Some(vt));
    em.load_slot(slot, k)?;
    Ok(())
}

// ---------- 字符串拼接 ----------

/// 拼接编译成 `java/lang/StringBuilder` 的调用链（javac 的老做法）。
///
/// 为什么是**真的** StringBuilder 而不是自造运行时：三路一致性要求
/// `javac 编的类` / 解释器 / JIT 输出一致，而 `java` 那一路上跑的就是**我们生成
/// 的字节码** —— 自造的类名在 HotSpot 里根本解析不到。所以这里只发标准调用，
/// 把 `StringBuilder` 做成侧面内建的是解释器与 JIT（`vm::rt` / `jit::rt`）。
///
/// 形状（每步操作数栈都收回到 0，跳转点永远是空栈）：
///
/// ```text
///   new java/lang/StringBuilder; dup; invokespecial <init>()V; astore s
///   aload s; <第 1 段>; invokevirtual append(X)Ljava/lang/StringBuilder; pop
///   …
///   aload s; invokevirtual toString()Ljava/lang/String;
/// ```
fn concat_parts<'a>(e: &'a Expr, ctx: &Ctx, out: &mut Vec<&'a Expr>) {
    // 左结合展开：只有**本身是字符串**的那一侧才继续拆（`1 + 2 + "a"` 的左边是数字，不能拆）
    if let Expr::Binary { op: BinOp::Add, lhs, rhs } = e {
        if is_string_val(lhs, ctx) {
            concat_parts(lhs, ctx, out);
        } else {
            out.push(lhs);
        }
        if is_string_val(rhs, ctx) {
            concat_parts(rhs, ctx, out);
        } else {
            out.push(rhs);
        }
    } else {
        out.push(e);
    }
}

/// `append` 的重载：按操作数种类挑描述符（boolean 必须 `(Z)`，否则会打成 1/0）
fn append_desc(e: &Expr, kind: u8, ctx: &Ctx) -> &'static str {
    match kind {
        K_LONG => "(J)Ljava/lang/StringBuilder;",
        K_DOUBLE => "(D)Ljava/lang/StringBuilder;",
        K_FLOAT => "(F)Ljava/lang/StringBuilder;", // 浮点拼接的十进制形态按 f32 规则
        K_BOOL => "(Z)Ljava/lang/StringBuilder;",
        K_REF => {
            if is_string_val(e, ctx) {
                "(Ljava/lang/String;)Ljava/lang/StringBuilder;"
            } else {
                "(Ljava/lang/Object;)Ljava/lang/StringBuilder;"
            }
        }
        _ => "(I)Ljava/lang/StringBuilder;",
    }
}

fn emit_concat(e: &Expr, em: &mut Emit, ctx: &Ctx, cp: &mut CpBuilder) -> Result<(), CompileError> {
    let mut parts = Vec::new();
    concat_parts(e, ctx, &mut parts);

    let sb_cls = cp.class("java/lang/StringBuilder");
    let slot = em.alloc_scratch()?;
    let saved = em.local_type_of(slot);

    em.op_u2(0xBB, sb_cls); // new
    em.push(1);
    em.op(0x59); // dup
    em.push(1);
    let init = cp.method_ref("java/lang/StringBuilder", "<init>", "()V");
    em.op_u2(0xB7, init); // invokespecial
    em.pop(1);
    em.store_slot(slot, K_REF)?;
    // 这个槽从这一刻起就是 StringBuilder（分支里的帧照它声明才不会被判 top）
    em.set_local_type(slot as usize, sb_cls);

    for p in parts {
        // 会分叉的操作数先算进脏槽：`aload` 已经把 builder 压上栈了，跳转点就不能有别的值
        let sp = if needs_scratch(p) { Some(spill(p, em, ctx, cp)?) } else { None };
        em.load_slot(slot, K_REF)?;
        emit_val(p, &sp, em, ctx, cp)?;
        let desc = append_desc(p, expr_kind(p, ctx), ctx);
        let idx = cp.method_ref("java/lang/StringBuilder", "append", desc);
        em.op_u2(0xB6, idx); // invokevirtual
        em.pop(desc_slots(desc) + 1); // 接收者 + 实参
        em.push(1); // append 返回 this
        em.op(0x57); // pop
        em.pop(1);
    }
    em.load_slot(slot, K_REF)?;
    let to_s = cp.method_ref("java/lang/StringBuilder", "toString", "()Ljava/lang/String;");
    em.op_u2(0xB6, to_s); // invokevirtual
    em.pop(1);
    em.push(1);
    em.set_local_type(slot as usize, saved);
    em.free_scratch(slot);
    Ok(())
}
