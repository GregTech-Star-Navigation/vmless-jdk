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
        // `!x`：先求 x 再 `ifne` 翻成 0/1 —— **它自己就带跳转**，必须单独算。
        // 少了这条，`"deleted=" + !Files.exists(f)` 会在 builder 已经压栈的位置
        // 撞上跳转点，报「比较结果作为表达式时，前面不能有未消费的操作数」。
        Expr::Unary { op: UnOp::Not, .. } => true,
        Expr::Unary { expr, .. } => needs_scratch(expr),
        Expr::Cast { expr, .. } => needs_scratch(expr),
        // `x instanceof T` 同样落成「跳转 + 压 0/1」
        Expr::InstanceOf { .. } => true,
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
        // switch 表达式内部有跳转表
        Expr::SwitchExpr(_) => true,
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
    // 会分叉的元素（`new boolean[]{a < b, a > b}`）必须**在数组引用还没压栈时**算进
    // 脏槽：`dup; iconst i; <元素>` 期间操作数栈非空，而跳转点要求栈为空。
    // 顺序仍然是从左到右，和 Java 的求值顺序一致（建数组本身没有副作用）。
    let sps: Vec<Option<Spilled>> = items
        .iter()
        .map(|it| {
            if needs_scratch(it) {
                spill(it, em, ctx, cp).map(Some)
            } else {
                Ok(None)
            }
        })
        .collect::<Result<Vec<_>, CompileError>>()?;
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
        emit_val(item, &sps[i], em, ctx, cp)?;
        coerce_to(elem, item, ctx, em, cp)?;
        em.op(store_op);
        em.pop(if wide { 4 } else { 3 });
    }
    for s in &sps {
        if let Some(x) = s {
            em.free_scratch(x.slot);
        }
    }
    Ok(())
}

/// 在**已知声明类型**的位置编一个初始化表达式：`int[] a = {1,2,3};` 的元素类型
/// 只能从声明处拿到（`ArrayInit` 本身不带类型），其余表达式照常编。
/// 在设置好 `lambda_target` 的作用域里执行 `f`（进去设、出来恢复）。
pub(crate) fn with_target<R>(ctx: &Ctx, t: Option<super::ctx::LambdaTarget>, f: impl FnOnce() -> R) -> R {
    let saved = ctx.lambda_target.replace(t);
    let r = f();
    ctx.lambda_target.replace(saved);
    r
}

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
    let t = lambda_target_of_ty(ty, ctx);
    with_target(ctx, t, || compile_expr(e, em, ctx, cp))
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
        // 二元运算只要两侧都纯就是纯：`tk[i / 2] += x` 的下标是 `KC / 2`，
        // 不放行会把「数组元素复合赋值」整体判死（java.base 里到处都是）
        Expr::Binary { lhs, rhs, .. } => is_pure_readable(lhs) && is_pure_readable(rhs),
        Expr::Cast { expr, .. } => is_pure_readable(expr),
        Expr::Unary { expr, .. } => is_pure_readable(expr),
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
pub(crate) fn arg_looks_like_array_pub(a: &Expr, param_desc: &str, ctx: &Ctx) -> bool { arg_looks_like_array(a, param_desc, ctx) }

fn arg_looks_like_array(a: &Expr, param_desc: &str, ctx: &Ctx) -> bool {
    match expr_type(a, ctx) {
        Some(t) => {
            let d = descriptor(&t);
            // 两边都是数组 → 传的就是整个数组，别再拿它当单个元素打包。
            //
            // 只比描述符会让 `Arrays.asList(stringArray)` 编成「一个 String[] 的
            // 单元素列表」（`[Ljava/lang/String;` != `[Ljava/lang/Object;`），
            // 于是 `new TreeSet<>(…).size()` 里塞的是数组，
            // 运行时 `ClassCastException: [Ljava.lang.String; → Comparable`。
            // 数组是协变的，元素类型能赋值就够，不需要描述符相等。
            d == param_desc
                || d == "Ljava/lang/Object;"
                || (d.starts_with('[') && param_desc.starts_with('['))
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
        // 实参位置的 lambda：目标类型来自被调方法的**形参类型**
        let t = params.get(i).and_then(|pd| lambda_target_of_desc(pd, ctx));
        with_target(ctx, t, || emit_val(a, &sp[i], em, ctx, cp))?;
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
    let raw = ctx.type_ctors.get(owner)?;
    // 跨包构造器只能选 public：`new TreeSet<>(someSet)` 曾经挑中包私有的
    // `TreeSet(NavigableMap)`，编出来是合法 class 而真 JVM 抛 IllegalAccessError。
    let list = crate::codegen::types::visible_overloads(owner, "<init>", raw, ctx);
    let list = &list;
    // 「精确命中」这条快路只有在**每个实参的静态类型都推得出来**时才可信：
    // `args_desc` 对推不出类型的引用实参只能瞎猜 `Ljava/lang/String;`，于是
    // `new Box(() -> {})` 会「精确命中」`Box(String)`，lambda 永远拿不到目标类型。
    let all_known = args.iter().all(|a| expr_type(a, ctx).is_some() || !is_lambda_like(a));
    let exact = format!("({})V", args_desc(args, ctx));
    if all_known && list.iter().any(|d| d == &exact) {
        return Some(exact);
    }
    let mut best: Option<(i32, String)> = None;
    for d in list.iter() {
        let ps = desc_params(d);
        let n = ps.len();
        let mut score = -1i32;
        if n == args.len() {
            match arg_score(&ps, args, ctx) {
                Some(s) => score = s,
                None => continue,
            }
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

/// 一组形参对一组实参打匹配分：`None` = 这个候选形状都不对（淘汰）。
///
/// 三点，缺一不可：
/// - **形状**：形参是引用、实参却是个数值（或反过来）→ 直接淘汰。少了它，
///   `new Thread(() -> {})` 会在 `Thread(String)` 上拿 0 分并被选中（重载按声明
///   顺序，撞上哪个全看运气），lambda 于是「推断不出目标类型」。
/// - **lambda 加分**：`Expr::Lambda` / 方法引用只能交给功能性接口形参，命中加
///   `LAMBDA_HIT`；交给非功能性接口形参 **淘汰**（`new Thread(() -> {})` 不可能是
///   `Thread(String)`）。这是语言规则，不是启发式。
/// - **精确命中**：形参描述符与实参静态类型完全一致 +1。`append('y')` 的 `(C)` 与
///   `(I)` 形状相同，只按形状会挑中 `append(int)` 打成 121。
pub(crate) fn arg_score(ps: &[String], args: &[Expr], ctx: &Ctx) -> Option<i32> {
    /// lambda / 方法引用命中功能性接口形参的加分。取 8 是为了压过「实参个数精确
    /// 命中」那几分的总和（实参再多也不会到 8），让 lambda 的位置始终优先。
    const LAMBDA_HIT: i32 = 8;
    if ps.len() != args.len() {
        return None;
    }
    let mut score = 0i32;
    for (i, p) in ps.iter().enumerate() {
        let want_ref = p.starts_with('L') || p.starts_with('[');
        let ad = expr_type(&args[i], ctx).map(|t| descriptor(&t));
        let got_ref = match ad.as_deref() {
            Some(d) => d.starts_with('L') || d.starts_with('['),
            None => expr_kind(&args[i], ctx) == K_REF,
        };
        // 形状**不一致不等于不适用**：`Object` 形参接 `int` 实参要自动装箱
        // （`new Holder<>(10)` 的构造器是 `(Ljava/lang/Object;)V`），反过来是拆箱。
        // 一律只降分、不淘汰 —— 淘汰会让 `pick_ctor` 一个候选都选不出来，
        // 于是退回「拿实参类型现拼描述符」，拼出根本不存在的 `<init>(I)V`，
        // 真 JVM 一跑就是 NoSuchMethodError。
        if want_ref == got_ref {
            score += 1;
        }
        if is_lambda_like(&args[i]) {
            if !desc_is_fun_iface(p, ctx) {
                return None;
            }
            // 参数个数也要对得上：`Comparator.compare(a,b)` 是 2 参，`Emp::name`
            // 只会消费 1 个（未绑定实例方法），所以 `thenComparing(Comparator)` 不是
            // 它的目标 —— 不排掉就会挑中先声明的那个 `thenComparing(Comparator)`，
            // 然后把 String 当 int 返回（REF → INT 转换失败）。
            if let (Some(hint), Some(sam)) = (lambda_arity(&args[i], ctx), sam_iface_desc(p, ctx)) {
                if desc_params(&sam).len() != hint {
                    return None;
                }
            }
            score += LAMBDA_HIT;
        }
        if ad.as_deref() == Some(p.as_str()) {
            score += 2;
        } else if want_ref && got_ref {
            // 形状对上了但类型不同：能赋值给这个形参的候选优先（+1），
            // 不能的也不淘汰（擦除/泛型会让类型推不出来，硬淘汰会一个都选不出）。
            if let (Some(at), true) = (ad.as_deref(), true) {
                let a_owner = at.trim_start_matches('L').trim_end_matches(';');
                let b_owner = p.trim_start_matches('L').trim_end_matches(';');
                if !at.starts_with('[') && crate::codegen::types::is_assignable(a_owner, b_owner, ctx) {
                    score += 1;
                }
            }
        }
    }
    Some(score)
}

/// 实参是不是 lambda / 方法引用 —— 这种实参**只能**交给功能性接口形参
///
/// `X.class` 也被 parser 记成 `MethodRef { name: "class" }`，但它的值是一个
/// `java.lang.Class`，不是方法引用，所以排除掉。
pub(crate) fn is_lambda_like(e: &Expr) -> bool {
    match e {
        Expr::Lambda { .. } | Expr::LambdaStmt { .. } => true,
        Expr::MethodRef { name, .. } => name != "class",
        _ => false,
    }
}

/// 形参描述符是个**已知的功能性接口**（能接 lambda）
fn desc_is_fun_iface(d: &str, ctx: &Ctx) -> bool {
    d.starts_with('L') && ctx.iface_sam.contains_key(d.trim_start_matches('L').trim_end_matches(';'))
}

/// 功能性接口形参的 SAM 描述符
fn sam_iface_desc<'a>(d: &str, ctx: &'a Ctx) -> Option<&'a String> {
    let name = d.trim_start_matches('L').trim_end_matches(';');
    ctx.iface_sam.get(name).map(|(_, sam)| sam)
}

/// lambda / 方法引用**会消费几个实参** —— 也就是它要求的 SAM 参数个数。
///
/// 推不出来就返回 `None`（不施加约束），推得出来就必须精确相等：这是 Java 的
/// 函数类型兼容规则，不是启发式。少了它，一个重载里只要有两个功能性接口形参
/// （`thenComparing(Comparator)` / `thenComparing(Function)`）就完全看声明顺序，
/// 撞错的那个会在 lambda 体里报出莫名其妙的转换错误。
fn lambda_arity(e: &Expr, ctx: &Ctx) -> Option<usize> {
    match e {
        Expr::Lambda { params, .. } | Expr::LambdaStmt { params, .. } => Some(params.len()),
        Expr::MethodRef { recv, name } if name != "class" => {
            // 绑定形态（`o::m` / `this::m`）：只消费方法自己的形参
            let bound_owner: Option<String> = match &**recv {
                Expr::This | Expr::Super => Some(ctx.class_name.clone()),
                Expr::Ident(n) => ctx
                    .local_ty
                    .get(n)
                    .and_then(|t| crate::codegen::types::type_name_candidates(t, ctx).into_iter().next())
                    .or_else(|| ctx.fields.get(n).and_then(|i| ctx.field_tys.get(*i)).and_then(|t| {
                        crate::codegen::types::type_name_candidates(t, ctx).into_iter().next()
                    })),
                // 其它表达式（`get()::m` / `arr[0]::m`）当作绑定，但拿不到类型
                _ => None,
            };
            if let Some(owner) = bound_owner {
                return method_arity(&owner, name, ctx);
            }
            // 类名形态：静态方法引用（0 个接收者）或未绑定实例方法引用（+1 个接收者）
            for owner in crate::codegen::types::class_owner_forms(recv, ctx) {
                if let Some(n) = static_arity(&owner, name, ctx) {
                    return Some(n);
                }
                if let Some(n) = method_arity(&owner, name, ctx) {
                    return Some(n + 1);
                }
            }
            None
        }
        _ => None,
    }
}

/// `owner` 上同名重载的形参个数（重载个数不一致就当作推不出来）
fn arity_of(cands: Option<&Vec<String>>) -> Option<usize> {
    let cands = cands?;
    let mut it = cands.iter().map(|d| desc_params(d).len());
    let first = it.next()?;
    if it.all(|n| n == first) {
        Some(first)
    } else {
        None
    }
}

fn method_arity(owner: &str, name: &str, ctx: &Ctx) -> Option<usize> {
    arity_of(ctx.type_methods.get(owner).and_then(|m| m.get(name)))
}

fn static_arity(owner: &str, name: &str, ctx: &Ctx) -> Option<usize> {
    arity_of(ctx.type_static_methods.get(owner).and_then(|m| m.get(name)))
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
            // VarHandle / MethodHandle 的**签名多态**调用：`(int) vh.getVolatile(o, i)`
            // 的方法描述符由**调用点**决定（实参静态类型 + 这里的 cast 目标类型），
            // 不是类文件里声明的那一个。按声明发 `(…)Ljava/lang/Object;` 再拆箱在
            // 运行时必抛 WrongMethodTypeException，所以必须在这里按多态描述符发。
            if to != K_REF {
                if let Some(pc) = poly_call_desc(expr, ty, ctx) {
                    return emit_poly_call(expr, &pc, em, ctx, cp);
                }
            }
            compile_expr(expr, em, ctx, cp)?;
            if to == K_REF {
                // 引用转换不是数值转换：要发 checkcast（失败抛 ClassCastException）
                //
                // 种类推不出来时（方法签名拿不到，`expr_kind` 会默认成 int）**按引用走**：
                // 把「不认识」当成「不是引用」会凭空报错，而 `(T) foo()` 这种写法在
                // 泛型擦除后的代码里遍地都是
                if from != K_REF && expr_type(expr, ctx).is_some() {
                    return err("只能把引用值转成引用类型");
                }
                let cls = ver_type(cp, ty);
                em.op_u2(0xC0, cls); // checkcast
            } else {
                coerce(from, to, em)?;
            }
            Ok(())
        }
        Expr::InstanceOf { expr, ty, .. } => {
            // 表达式位置上的 `x instanceof T t`：**绑定在这里用不上**（模式变量只在
            // 「测试为真」那条路径上可见，那种作用域必须由语句层展开 —— 见
            // `stmt.rs` 的 `pattern_cond` / `split_pattern_and`）。所以这里只发测试，
            // 不声明变量：真的有人在这个位置用 t，会在下一处报「未定义的变量」
            // （明确失败），而不是悄悄算错。
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
        // 文本块（`"""…"""`）：parser 已经把缩进/转义处理成最终内容，字节码里就是普通 String
        Expr::StrLit(s) | Expr::TextBlock(s) => {
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
            // `import static p.C.*;` / `import static p.C.F;` 之后的裸常量名
            // （`SEMI_BLKSIZE`、`UTF_8` 这类）—— 符号表里有，这里不查就会报「未定义的变量」
            if let Some((owner, desc)) = static_import_field(name, ctx) {
                let f = cp.field_ref(&owner, name, &desc);
                em.op_u2(0xB2, f); // getstatic
                em.push(if kind_wide(kind_from_desc(&desc)) { 2 } else { 1 });
                return Ok(());
            }
            err(format!("未定义的变量 `{name}`"))
        }
        // System.out.println(x) / System.err.println(x)。
        // **只接管 System.out / System.err 这一种接收者**：`s.println(x)`（s 是
        // PrintStream 的局部变量或字段）交给下面的通用调用路径 —— 那里有 PrintStream
        // 的完整重载表，能按实参类型挑对 `println(Object)` / `println(char[])`。
        Expr::Call { recv: Some(r), name, args, .. }
            if name == "println" && is_system_stream(r) =>
        {
            if let Expr::Field(boxed, field) = &**r {
                if field == "out" || field == "err" {
                    if let Expr::Ident(_) = &**boxed {
                        let out = cp.field_ref("java/lang/System", field, "Ljava/io/PrintStream;");
                        // `println()` —— 只换行，是最常用的一行；按 `println()V` 发
                        if args.is_empty() {
                            let println = cp.method_ref("java/io/PrintStream", "println", "()V");
                            em.op_u2(0xB2, out);
                            em.push(1);
                            em.op_u2(0xB6, println);
                            em.pop(1);
                            return Ok(());
                        }
                        if args.len() != 1 {
                            return err(format!("println 支持 0/1 个参数，实际 {}", args.len()));
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
                .ok_or_else(|| CompileError { msg: format!("未找到实例方法 `{name}`（接收者 {} `{}` / {} 参）", expr_type(r, ctx).as_ref().and_then(crate::codegen::types::type_internal).unwrap_or_else(|| "?".into()), recv_brief(r), args.len()) })?;
            let sp = hoist_args(args, em, ctx, cp)?;
            compile_expr(r, em, ctx, cp)?;
            emit_call_args(args, &desc, &sp, em, ctx, cp)?;
            // 引用类用接收者的**静态类型**：验证器要求接收者可赋给引用类。
            // 分派本身按运行时类走（虚方法覆写），引用类只决定从哪条链开始解析。
            let owner = expr_type(r, ctx)
                .as_ref()
                .and_then(|t| resolve_type_owner(t, ctx))
                .or_else(|| expr_type(r, ctx).as_ref().and_then(type_internal))
                .unwrap_or_else(|| ctx.class_name.clone());
            // invokeinterface 的 count 是**描述符里的参数槽数**（long/double 各 2 槽）
            // + 1 个接收者，不是源码里的实参个数 —— 写成实参个数会得到
            // `VerifyError: Inconsistent args count operand in invokeinterface`
            let n = desc_slots(&desc) as u16 + 1;
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
                // 泛型方法的**静态**返回类型和擦除后不一样时，补一条 checkcast。
                //
                // 验证器只认描述符里的类型（`List.get` 发出去的是 `Object`），而后面
                // 的 invokevirtual / 拆箱都按**静态**类型发（`l.get(0).isEmpty()` →
                // `invokevirtual String.isEmpty`）。不补这一条，栈上的 Object 赋给
                // String 形参就是 `VerifyError: Bad type on operand stack`。
                //
                // 只在擦除后是 `Object` 时才有必要：别的返回类型本来就对得上，
                // 而 `generic_ret` 会递归求接收者的类型，无谓地查会拖慢批量编译。
                if desc.rsplit(')').next() == Some("Ljava/lang/Object;") {
                    if let Some(t) = crate::codegen::types::generic_ret(r, name, &desc, ctx) {
                        let erased = type_from_desc("Ljava/lang/Object;");
                        if kind_of(&t) == K_REF
                            && !matches!(t, Type::Var)
                            && descriptor(&t) != descriptor(&erased)
                        {
                            let cls = ver_type(cp, &t);
                            em.op_u2(0xC0, cls); // checkcast
                        }
                    }
                }
            }
            Ok(())
        }
        // 调用本类（或父类）的方法：静态走 invokestatic，实例走 this 的 invokevirtual
        Expr::Call { name, args, .. } => {
            if let Some(desc) = ctx.static_methods.get(name.as_str()).and_then(|c| pick_desc(c, args, ctx)) {
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
            // 从**父类/外层类**继承来的静态方法（`createCapacityException(n)`）：
            // invokestatic 的 owner 必须是声明它的那个类，不能是 <SELF>
            if let Some((owner, desc)) = ctx
                .inherited_statics
                .get(name.as_str())
                .and_then(|(o, ds)| pick_desc(ds, args, ctx).map(|d| (o.clone(), d)))
            {
                let sp = hoist_args(args, em, ctx, cp)?;
                emit_call_args(args, &desc, &sp, em, ctx, cp)?;
                let idx = if ctx.iface_types.contains(&owner) {
                    cp.interface_method_ref(&owner, name, &desc)
                } else {
                    cp.method_ref(&owner, name, &desc)
                };
                em.op_u2(0xB8, idx);
                em.pop(desc_slots(&desc));
                if !desc.ends_with(")V") {
                    em.push(if kind_wide(kind_from_ret(&desc)) { 2 } else { 1 });
                }
                return Ok(());
            }
            // `import static p.C.m;` 之后的裸名字调用 → invokestatic 那个声明类
            if let Some((owner, desc)) = static_import_call(name, args, ctx) {
                let sp = hoist_args(args, em, ctx, cp)?;
                emit_call_args(args, &desc, &sp, em, ctx, cp)?;
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
            let desc = ctx
                .instance_methods
                .get(name.as_str())
                .and_then(|c| pick_desc(c, args, ctx))
                .ok_or_else(|| CompileError { msg: format!("未找到方法 `{name}`") })?;
            let sp = hoist_args(args, em, ctx, cp)?;
            em.op(0x2A); // aload_0
            em.push(1);
            emit_call_args(args, &desc, &sp, em, ctx, cp)?;
            // invokeinterface 的 count 是**描述符里的参数槽数**（long/double 各 2 槽）
            // + 1 个接收者，不是源码里的实参个数 —— 写成实参个数会得到
            // `VerifyError: Inconsistent args count operand in invokeinterface`
            let n = desc_slots(&desc) as u16 + 1;
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
                // 移位指令的栈效果：int 版弹 2 压 1（净 -1）；long 版弹 **3**
                // （long 占 2 槽 + int 移位量 1 槽）压 2（净 -1）。
                // 原来对 long 只按 `pop(2)` 记账，少减一槽，`max_stack` 于是偏小 1，
                // 真 JVM 报 `VerifyError: Operand stack overflow`（`i2l` 那一步就超了）。
                if kind_wide(k) {
                    em.pop(3);
                    em.push(2);
                } else {
                    em.pop(1);
                }
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
                // `this.f = v` / `a[i] = v` 当值用：接收者/下标都只求值一次
                if let Expr::Field(recv, f) = &**lhs {
                    if matches!(**recv, Expr::This) {
                        return assign_expr_this_field(f, rhs, em, ctx, cp);
                    }
                }
                if matches!(&**lhs, Expr::ArrayAccess { .. }) {
                    return assign_expr_array(lhs, rhs, em, ctx, cp);
                }
                return err(format!("赋值作为表达式时只支持局部变量：{lhs:?}"));
            };
            let Some(slot) = ctx.lookup(name) else {
                // `f = v` 里的 f 是隐式 this 的字段 / 或者本类的静态字段
                return assign_expr_this_field(name, rhs, em, ctx, cp);
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
        // `switch (...) { … }` 表达式：整块降成
        //   { <ty> $swN = <零值>; switch (...) { case … -> … } }  ，取值再 load 回来
        // 箭头体 / `yield e;` 都变成「写 $swN，再 break」。switch 的代码生成完全复用
        // 语句形态那套（tableswitch / lookupswitch / 比较链都在那里）。
        Expr::SwitchExpr(_) => return err("switch 表达式只支持语句/赋值/返回位置"),
        // class 字面量：`String.class` / `int.class` → `ldc` 一个 CONSTANT_Class
        Expr::ClassLit(ty) => emit_class_literal(ty, em, ctx, cp),
        // parser 把 `X.class` 记成 `MethodRef { recv: X, name: "class" }`
        Expr::MethodRef { recv, name } if name == "class" => match expr_as_type(recv) {
            Some(ty) => emit_class_literal(&ty, em, ctx, cp),
            None => err(format!("`X.class` 的左边不是类型名: {recv:?}")),
        },
        // 表达式体 lambda：`x -> e`
        Expr::Lambda { params, body } => {
            let Some(tgt) = ctx.lambda_target.borrow().clone() else {
                return err("推断不出 lambda 的目标类型（功能性接口）");
            };
            let Some((_, sam_desc)) = ctx.iface_sam.get(&tgt.iface).cloned() else {
                return err(format!("`{}` 不是功能性接口", tgt.iface.replace('/', ".")));
            };
            let named = sam_params_named(params, &sam_desc, &tgt.args);
            // 表达式 lambda 的体怎么收尾，取决于 **SAM 的返回类型**：
            // `Consumer<String> c = s -> list.add(s);` 的 `add` 返回 boolean，
            // 直接 `Stmt::Return(Some(…))` 会按「返回 int」发 `ireturn`，
            // 而合成方法的描述符是 `()V` —— 真 JVM 与我们的 VM 都报
            // `VerifyError: Operand stack underflow … ireturn`。
            // void 的 SAM 要把它当**语句**编（值算完丢掉），这才是 Java 的语义。
            let sam_ret_void = sam_desc.rsplit(')').next().map(|r| r == "V").unwrap_or(false);
            let stmt = if sam_ret_void {
                Stmt::Expr((**body).clone())
            } else {
                Stmt::Return(Some((**body).clone()))
            };
            let inst = body_uses_this(&stmt, ctx);
            emit_lambda(stmt, named, inst, em, ctx, cp)
        }
        // 块体 lambda：`(a, b) -> { … }`
        Expr::LambdaStmt { params, body } => {
            let Some(tgt) = ctx.lambda_target.borrow().clone() else {
                return err("推断不出 lambda 的目标类型（功能性接口）");
            };
            let Some((_, sam_desc)) = ctx.iface_sam.get(&tgt.iface).cloned() else {
                return err(format!("`{}` 不是功能性接口", tgt.iface.replace('/', ".")));
            };
            let named = sam_params_named(params, &sam_desc, &tgt.args);
            let inst = body_uses_this(body, ctx);
            emit_lambda((**body).clone(), named, inst, em, ctx, cp)
        }
        // 方法引用：`Foo::m`（静态 / 未绑定实例）、`o::m`（绑定实例）、`this::m`
        Expr::MethodRef { recv, name } if name != "class" => {
            let Some(tgt) = ctx.lambda_target.borrow().clone() else {
                return err("推断不出方法引用的目标类型（功能性接口）");
            };
            let Some((_, sam_desc)) = ctx.iface_sam.get(&tgt.iface).cloned() else {
                return err(format!("`{}` 不是功能性接口", tgt.iface.replace('/', ".")));
            };
            let pnames: Vec<String> =
                (0..desc_params(&sam_desc).len()).map(|i| format!("$p{i}")).collect();
            let params = sam_params_named(&pnames, &sam_desc, &tgt.args);
            let args: Vec<Expr> =
                pnames.iter().map(|n| Expr::Ident(n.clone())).collect();
            let recv_local = match &**recv {
                Expr::Ident(n) if ctx.locals.get(n).is_some() => Some(n.clone()),
                _ => None,
            };
            let (body, inst, used) = if let Some(n) = recv_local {
                // 绑定到局部变量：接收者本身就是一个捕获
                (
                    Stmt::Return(Some(Expr::Call {
                        recv: Some(Box::new(Expr::Ident(n))),
                        name: name.clone(),
                        args,
                        type_args: Vec::new(),
                    })),
                    false,
                    params.clone(),
                )
            } else if matches!(&**recv, Expr::This | Expr::Super) {
                (
                    Stmt::Return(Some(Expr::Call {
                        recv: Some(recv.clone()),
                        name: name.clone(),
                        args,
                        type_args: Vec::new(),
                    })),
                    true,
                    params.clone(),
                )
            } else if !class_owner_forms(recv, ctx).is_empty() {
                // 构造器引用：parser 把 `Foo::new` / `File[]::new` 都记成 `%array-new`
                if name == "new" || name == "%array-new" {
                    let Some(ty) = expr_as_type(recv) else {
                        return err("构造器引用的类名解析失败");
                    };
                    let body = if params.len() == 1 {
                        // `File[]::new`：SAM 那一个参数是数组长度
                        Stmt::Return(Some(Expr::ArrayNew {
                            elem: ty,
                            dims: vec![Expr::Ident(pnames[0].clone())],
                        }))
                    } else {
                        Stmt::Return(Some(Expr::New { ty, args }))
                    };
                    (body, false, params.clone())
                } else {
                // 类名形态：静态方法引用，或未绑定的实例方法引用（SAM 第一个参数是接收者）。
                // `String::length` 这种擦除后第一个参数是 Object，接收者要补 checkcast。
                let static_owner = class_owner_forms(recv, ctx).into_iter().find(|o| {
                    ctx.type_static_methods.get(o).map(|m| m.contains_key(name.as_str())).unwrap_or(false)
                });
                if static_owner.is_some() {
                    (
                        Stmt::Return(Some(Expr::Call {
                            recv: Some(recv.clone()),
                            name: name.clone(),
                            args,
                            type_args: Vec::new(),
                        })),
                        false,
                        params.clone(),
                    )
                } else {
                    let Some(recv_ty0) = expr_as_type(recv) else {
                        return err("方法引用的类名解析失败");
                    };
                    // 类名要翻成符号表里的**内部名**：`Emp::salary` 写在 `R1` 里时真名是
                    // `R1$Emp`，按裸名发 checkcast 会得到一个不存在的类
                    // （运行时 `NoClassDefFoundError: Emp`）。
                    let recv_ty = crate::codegen::types::resolve_type_owner(&recv_ty0, ctx)
                        .map(|o| Type::Named {
                            path: o.split('/').map(|s| s.to_string()).collect(),
                            args: Vec::new(),
                        })
                        .unwrap_or(recv_ty0);
                    if params.is_empty() {
                        return err("未绑定实例方法引用要求 SAM 至少有一个参数");
                    }
                    let first = pnames[0].clone();
                    (
                        Stmt::Return(Some(Expr::Call {
                            recv: Some(Box::new(Expr::Cast {
                                ty: recv_ty,
                                expr: Box::new(Expr::Ident(first)),
                            })),
                            name: name.clone(),
                            args: args[1..].to_vec(),
                            type_args: Vec::new(),
                        })),
                        false,
                        params.clone(),
                    )
                }
                }
            } else {
                // `this.field::m` 这类：接收者表达式本身就在实例上下文里
                (
                    Stmt::Return(Some(Expr::Call {
                        recv: Some(recv.clone()),
                        name: name.clone(),
                        args,
                        type_args: Vec::new(),
                    })),
                    true,
                    params.clone(),
                )
            };
            emit_lambda(body, used, inst, em, ctx, cp)
        }
        other => err(format!("暂不支持的表达式: {other:?}")),
    }
}

/// `System.out` / `System.err` 这种接收者（`getstatic java/lang/System.out`）
fn is_system_stream(r: &Expr) -> bool {
    match r {
        Expr::Field(boxed, f) => {
            (f == "out" || f == "err") && matches!(&**boxed, Expr::Ident(_))
        }
        _ => false,
    }
}

/// 把「类名当表达式」的 `A.B.C` 还原成类型路径
fn expr_as_type(e: &Expr) -> Option<Type> {
    match e {
        Expr::Ident(n) => Some(Type::Named { path: vec![n.clone()], args: vec![] }),
        Expr::Field(inner, seg) => {
            let mut t = expr_as_type(inner)?;
            if let Type::Named { path, .. } = &mut t {
                path.push(seg.clone());
            }
            Some(t)
        }
        _ => None,
    }
}

/// `T.class`：引用类型 `ldc CONSTANT_Class`；基本类型没有对应的 class 常量，
/// 按 javac 的做法取包装类的 `TYPE` 字段（`int.class == Integer.TYPE`）。
fn emit_class_literal(
    ty: &Type,
    em: &mut Emit,
    ctx: &Ctx,
    cp: &mut CpBuilder,
) -> Result<(), CompileError> {
    if let Type::Prim(p) = ty {
        let pname: &str = p;
        let wrapper = match pname {
            "int" => "java/lang/Integer",
            "long" => "java/lang/Long",
            "short" => "java/lang/Short",
            "byte" => "java/lang/Byte",
            "char" => "java/lang/Character",
            "boolean" => "java/lang/Boolean",
            "float" => "java/lang/Float",
            "double" => "java/lang/Double",
            "void" => "java/lang/Void",
            other => return err(format!("不支持 `{other}.class`")),
        };
        let f = cp.field_ref(wrapper, "TYPE", "Ljava/lang/Class;");
        em.op_u2(0xB2, f); // getstatic
        em.push(1);
        return Ok(());
    }
    let Some(name) = class_operand_name(ty, ctx) else {
        return err(format!("不支持这个类型的 class 字面量: {ty:?}"));
    };
    let idx = cp.class(&name);
    em.op_u2(0x13, idx); // ldc_w
    em.push(1);
    Ok(())
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
        // `this.f++` 与隐式 this 的 `f++` 是同一条路
        if let Expr::Field(recv, fname) = target {
            if matches!(**recv, Expr::This) {
                return inc_dec_this_field(op, fname, em, ctx, cp);
            }
        }
        return err(format!("自增/自减作为表达式时只支持局部变量：{target:?}"));
    };
    if ctx.lookup(name).is_none() {
        // `f++` 里的 f 可能是**字段**（隐式 this），不是局部变量
        return inc_dec_this_field(op, name, em, ctx, cp);
    }
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
        // char 和 int 在字节码层面同一个种类，但 append 的重载语义不同：
        // 打成 `append(int)` 会输出码点（`first=a` 变成 `first=97`）。
        _ => {
            if matches!(expr_type(e, ctx), Some(Type::Prim("char"))) {
                "(C)Ljava/lang/StringBuilder;"
            } else {
                "(I)Ljava/lang/StringBuilder;"
            }
        }
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

/// `f++` / `++f` / `this.f++` 作为**值**用（隐式 this 的字段）。
///
/// 字段读两次是安全的（`aload_0` 没有副作用），所以不用临时槽：
/// ```text
///   post: aload_0 ; aload_0 ; getfield f ; dup_x1     ; [old][this][old]
///         iconst_1 ; iadd ; putfield f                ; [old]
///   pre : aload_0 ; aload_0 ; getfield f ; iconst_1 ; iadd ; dup_x1
///         putfield f                                  ; [new]
/// ```
/// long/double 用 `dup2_x1`（宽值占两槽，插入点要下移一格）。
fn inc_dec_this_field(
    op: UnOp,
    name: &str,
    em: &mut Emit,
    ctx: &Ctx,
    cp: &mut CpBuilder,
) -> Result<(), CompileError> {
    let Some(&idx) = ctx.fields.get(name) else {
        return err(format!("自增/自减的目标不是局部变量也不是字段：`{name}`"));
    };
    let desc = ctx.field_descs[idx].clone();
    let kind = kind_from_desc(&desc);
    let wide = kind_wide(kind);
    let post = matches!(op, UnOp::PostInc | UnOp::PostDec);
    let inc = matches!(op, UnOp::PreInc | UnOp::PostInc);
    let (one, arith) = match kind {
        K_LONG => (0x0Au8, if inc { 0x61 } else { 0x65 }),
        K_FLOAT => (0x0Cu8, if inc { 0x62 } else { 0x66 }),
        K_DOUBLE => (0x0Fu8, if inc { 0x63 } else { 0x67 }),
        K_INT => (0x04, if inc { 0x60 } else { 0x64 }),
        _ => return err(format!("自增/自减的目标不是数值：`{name}`")),
    };
    let fref = cp.field_ref(&ctx.class_name, name, &desc);
    let w = if wide { 2 } else { 1 };
    // [this]
    em.op(0x2A);
    em.push(1);
    // [this] → [this][this] → getfield → [this][old]
    em.op(0x2A);
    em.push(1);
    em.op_u2(0xB4, fref);
    em.pop(1);
    em.push(w);
    let bump = |em: &mut Emit| -> Result<(), CompileError> {
        em.op(one);
        em.push(w);
        em.op(arith);
        em.pop(w);
        Ok(())
    };
    if post {
        // [this][old] → [old][this][old]
        em.op(if wide { 0x5D } else { 0x5A }); // dup2_x1 / dup_x1
        em.push(w);
        bump(em)?;
        em.op_u2(0xB5, fref); // putfield
        em.pop(w + 1);
    } else {
        bump(em)?;
        // [this][new] → [new][this][new]
        em.op(if wide { 0x5D } else { 0x5A });
        em.push(w);
        em.op_u2(0xB5, fref);
        em.pop(w + 1);
    }
    Ok(())
}

/// `f = v` / `this.f = v` 当**值**用（隐式 this 的字段或本类静态字段）。
///
/// 值要留在栈上：`aload_0; <v>; dup_x1; putfield` 之后栈上还剩 v。
/// 静态字段更简单：`<v>; dup; putstatic`。
fn assign_expr_this_field(
    name: &str,
    rhs: &Expr,
    em: &mut Emit,
    ctx: &Ctx,
    cp: &mut CpBuilder,
) -> Result<(), CompileError> {
    let kind;
    let w;
    if let Some(&idx) = ctx.fields.get(name) {
        let desc = ctx.field_descs[idx].clone();
        kind = kind_from_desc(&desc);
        w = if kind_wide(kind) { 2 } else { 1 };
        let fref = cp.field_ref(&ctx.class_name, name, &desc);
        em.op(0x2A); // aload_0
        em.push(1);
        compile_expr(rhs, em, ctx, cp)?;
        coerce_to(&type_from_desc(&desc), rhs, ctx, em, cp)?;
        em.op(if w == 2 { 0x5D } else { 0x5A }); // dup2_x1 / dup_x1
        em.push(w);
        em.op_u2(0xB5, fref); // putfield
        em.pop(w + 1);
        return Ok(());
    }
    let Some((owner, desc)) = ctx.static_fields.get(name).cloned() else {
        return err(format!("赋值作为表达式时目标既不是局部变量也不是字段：`{name}`"));
    };
    let kind = kind_from_desc(&desc);
    let w = if kind_wide(kind) { 2 } else { 1 };
    compile_expr(rhs, em, ctx, cp)?;
    coerce_to(&type_from_desc(&desc), rhs, ctx, em, cp)?;
    em.op(if w == 2 { 0x5C } else { 0x59 }); // dup2 / dup
    em.push(w);
    let fref = cp.field_ref(&owner, name, &desc);
    em.op_u2(0xB3, fref); // putstatic
    em.pop(w);
    Ok(())
}

/// `a[i] = v` 当**值**用：`<a><i><v>; dup_x2|dup2_x2; <x>astore` —— 之后栈上还剩 v。
fn assign_expr_array(
    lhs: &Expr,
    rhs: &Expr,
    em: &mut Emit,
    ctx: &Ctx,
    cp: &mut CpBuilder,
) -> Result<(), CompileError> {
    let Expr::ArrayAccess { array, index } = lhs else {
        return err("内部错误：assign_expr_array 收到了非数组下标");
    };
    let (_, store) = array_ops(array, ctx);
    let elem = match expr_type(array, ctx) {
        Some(Type::Array(inner)) => *inner,
        _ => Type::Prim("int"),
    };
    compile_expr(array, em, ctx, cp)?;
    em.push(1);
    compile_expr(index, em, ctx, cp)?;
    em.push(1);
    compile_expr(rhs, em, ctx, cp)?;
    coerce_to(&elem, rhs, ctx, em, cp)?;
    let w = if kind_wide(kind_of(&elem)) { 2u16 } else { 1u16 };
    // [arr][idx][v] → [v][arr][idx][v]
    em.op(if w == 2 { 0x5E } else { 0x5B }); // dup2_x2 / dup_x2
    em.push(w);
    em.op(store);
    em.pop(2 + w);
    Ok(())
}

/// VarHandle 的签名多态访问模式（JVMS 2.9.3 / java.lang.invoke.VarHandle）
const VARHANDLE_MODES: &[&str] = &[
    "get", "set", "getVolatile", "setVolatile", "getOpaque", "setOpaque", "getAcquire", "setRelease",
    "compareAndSet", "compareAndExchange", "compareAndExchangeAcquire", "compareAndExchangeRelease",
    "weakCompareAndSetPlain", "weakCompareAndSet", "weakCompareAndSetAcquire",
    "weakCompareAndSetRelease", "weakCompareAndSetVolatile", "getAndSet", "getAndSetAcquire",
    "getAndSetRelease", "getAndAdd", "getAndAddAcquire", "getAndAddRelease", "getAndBitwiseOr",
    "getAndBitwiseOrAcquire", "getAndBitwiseOrRelease", "getAndBitwiseAnd",
    "getAndBitwiseAndAcquire", "getAndBitwiseAndRelease", "getAndBitwiseXor",
    "getAndBitwiseXorAcquire", "getAndBitwiseXorRelease",
];

/// 一次签名多态调用：谁的方法、是不是静态、调用点描述符
struct PolyCall {
    owner: String,
    is_static: bool,
    desc: String,
}

/// 判断 `expr` 是不是签名多态调用；是就给出**调用点**描述符。
///
/// `(int) vh.getVolatile(o, i)` 里 `getVolatile` 在类文件里的声明是
/// `([Ljava/lang/Object;)Ljava/lang/Object;`，但真正要发的指令是
/// `invokevirtual VarHandle.getVolatile:(Ljava/lang/Object;I)I` —— 描述符由
/// 「实参静态类型 + 期望返回类型」拼出来，期望返回类型就来自这层 cast。
///
/// 静态的四个 `linkTo*` 同理（`(int) MethodHandle.linkToStatic(handle, arg0, mn)`）。
fn poly_call_desc(expr: &Expr, want: &Type, ctx: &Ctx) -> Option<PolyCall> {
    let Expr::Call { recv: Some(r), name, args, .. } = expr else { return None };
    let desc = format!("({}){}", args_desc(args, ctx), descriptor(want));
    // 静态签名多态：MethodHandle.linkToStatic / linkToVirtual / linkToSpecial / linkToInterface
    if matches!(name.as_str(), "linkToStatic" | "linkToVirtual" | "linkToSpecial" | "linkToInterface") {
        if class_owner_forms(r, ctx).iter().any(|o| o == "java/lang/invoke/MethodHandle") {
            return Some(PolyCall {
                owner: "java/lang/invoke/MethodHandle".to_string(),
                is_static: true,
                desc,
            });
        }
        return None;
    }
    let owner = expr_type(r, ctx).as_ref().and_then(|t| resolve_type_owner(t, ctx))?;
    let is_vh = owner.ends_with("VarHandle");
    let is_mh = owner == "java/lang/invoke/MethodHandle";
    let poly = if is_vh {
        VARHANDLE_MODES.contains(&name.as_str())
    } else if is_mh {
        matches!(name.as_str(), "invoke" | "invokeExact" | "invokeBasic")
    } else {
        false
    };
    if !poly {
        return None;
    }
    Some(PolyCall { owner, is_static: false, desc })
}

/// 按多态描述符发调用（接收者 + 实参 + invokevirtual / invokestatic）
fn emit_poly_call(
    expr: &Expr,
    pc: &PolyCall,
    em: &mut Emit,
    ctx: &Ctx,
    cp: &mut CpBuilder,
) -> Result<(), CompileError> {
    let Expr::Call { recv: Some(_), name, args, .. } = expr else {
        return err("内部错误：emit_poly_call 收到了非方法调用");
    };
    let sp = hoist_args(args, em, ctx, cp)?;
    if !pc.is_static {
        let Expr::Call { recv: Some(r), .. } = expr else { unreachable!() };
        compile_expr(r, em, ctx, cp)?;
    }
    emit_call_args(args, &pc.desc, &sp, em, ctx, cp)?;
    let idx = cp.method_ref(&pc.owner, name, &pc.desc);
    if pc.is_static {
        em.op_u2(0xB8, idx); // invokestatic
        em.pop(desc_slots(&pc.desc));
    } else {
        em.op_u2(0xB6, idx); // invokevirtual
        em.pop(desc_slots(&pc.desc) + 1);
    }
    em.push(if kind_wide(kind_from_ret(&pc.desc)) { 2 } else { 1 });
    Ok(())
}


// ============================================================ lambda / 方法引用
//
// 做法与 javac 一致：把体编成一个**合成方法**，调用点发 `invokedynamic`
// （引导方法 `LambdaMetafactory.metafactory`）。合成方法统一用**实例方法**，
// 这样 lambda 体里的 `this` / 实例字段访问原样可用 —— 调用点用
// `REF_invokeVirtual` 把 `this` 当第一个「捕获实参」压上去即可。
//
// 捕获：lambda 体里出现、且在**外层方法**里是局部变量的那些名字。
// 它们变成合成方法的前若干个形参，调用点按同样的顺序压栈。

/// 收集表达式里出现的**标识符名**（`Expr::Ident` 与 `Field` 的段名）。
///
/// 只用来算「lambda 捕获了哪些外层局部变量」：一个名字既要出现在体里、又要
/// 在外层是局部变量，才算捕获。多算会被 outer-locals 过滤掉，少算会漏传实参。
fn collect_idents_e(e: &Expr, out: &mut Vec<String>) {
    let push = |out: &mut Vec<String>, n: &str| {
        if !out.iter().any(|x| x == n) {
            out.push(n.to_string());
        }
    };
    match e {
        Expr::Ident(n) => push(out, n),
        Expr::Field(r, f) => {
            collect_idents_e(r, out);
            push(out, f);
        }
        Expr::Call { recv, args, .. } => {
            if let Some(r) = recv {
                collect_idents_e(r, out);
            }
            for a in args {
                collect_idents_e(a, out);
            }
        }
        Expr::New { args, .. } => {
            for a in args {
                collect_idents_e(a, out);
            }
        }
        Expr::ArrayNew { dims, .. } => {
            for a in dims {
                collect_idents_e(a, out);
            }
        }
        Expr::ArrayInit(xs) => {
            for a in xs {
                collect_idents_e(a, out);
            }
        }
        Expr::ArrayAccess { array, index } => {
            collect_idents_e(array, out);
            collect_idents_e(index, out);
        }
        Expr::Binary { lhs, rhs, .. } => {
            collect_idents_e(lhs, out);
            collect_idents_e(rhs, out);
        }
        Expr::Unary { expr, .. } => collect_idents_e(expr, out),
        Expr::Assign { lhs, rhs, .. } => {
            collect_idents_e(lhs, out);
            collect_idents_e(rhs, out);
        }
        Expr::Cond { cond, then, els } => {
            collect_idents_e(cond, out);
            collect_idents_e(then, out);
            collect_idents_e(els, out);
        }
        Expr::Cast { expr, .. } => collect_idents_e(expr, out),
        Expr::InstanceOf { expr, .. } => collect_idents_e(expr, out),
        Expr::Lambda { params, body } => {
            collect_idents_e(body, out);
            out.retain(|n| !params.contains(n));
        }
        Expr::LambdaStmt { params, body } => {
            collect_idents_s(body, out);
            out.retain(|n| !params.contains(n));
        }
        Expr::MethodRef { recv, .. } => collect_idents_e(recv, out),
        Expr::SwitchExpr(s) => collect_idents_s(s, out),
        _ => {}
    }
}

fn collect_idents_s(s: &Stmt, out: &mut Vec<String>) {
    match s {
        Stmt::Block(xs) => {
            for x in xs {
                collect_idents_s(x, out);
            }
        }
        Stmt::LocalVar { init, .. } => {
            if let Some(i) = init {
                collect_idents_e(i, out);
            }
        }
        Stmt::Expr(e) | Stmt::Throw(e) | Stmt::Yield(e) => collect_idents_e(e, out),
        Stmt::Return(Some(e)) => collect_idents_e(e, out),
        Stmt::Return(None) | Stmt::Break(_) | Stmt::Continue(_) | Stmt::Empty => {}
        Stmt::If { cond, then, els } => {
            collect_idents_e(cond, out);
            collect_idents_s(then, out);
            if let Some(e) = els {
                collect_idents_s(e, out);
            }
        }
        Stmt::While { cond, body } => {
            collect_idents_e(cond, out);
            collect_idents_s(body, out);
        }
        Stmt::DoWhile { body, cond } => {
            collect_idents_s(body, out);
            collect_idents_e(cond, out);
        }
        Stmt::For { init, cond, update, body } => {
            for s in init {
                collect_idents_s(s, out);
            }
            if let Some(c) = cond {
                collect_idents_e(c, out);
            }
            for u in update {
                collect_idents_e(u, out);
            }
            collect_idents_s(body, out);
        }
        Stmt::ForEach { iter, body, .. } => {
            collect_idents_e(iter, out);
            collect_idents_s(body, out);
        }
        Stmt::Labeled { body, .. } => collect_idents_s(body, out),
        Stmt::Switch { selector, cases } => {
            collect_idents_e(selector, out);
            for c in cases {
                for l in c.labels.iter().flatten() {
                    collect_idents_e(l, out);
                }
                for b in &c.body {
                    collect_idents_s(b, out);
                }
            }
        }
        Stmt::Sync { lock, body } => {
            collect_idents_e(lock, out);
            collect_idents_s(body, out);
        }
        Stmt::Try { body, catches, finally } => {
            collect_idents_s(body, out);
            for c in catches {
                collect_idents_s(&c.body, out);
            }
            if let Some(f) = finally {
                collect_idents_s(f, out);
            }
        }
    }
}

/// 声明类型 → 它作为功能性接口的内部名（符号表里真实存在的那个键）
pub(crate) fn lambda_target_of_ty(ty: &Type, ctx: &Ctx) -> Option<super::ctx::LambdaTarget> {
    let iface = type_name_candidates(ty, ctx).into_iter().find(|c| ctx.iface_sam.contains_key(c))?;
    let args = match ty {
        Type::Named { args, .. } => args.clone(),
        _ => Vec::new(),
    };
    Some(super::ctx::LambdaTarget { iface, args })
}

/// 描述符里的一个参数 → 目标接口（实参位置的目标类型）
pub(crate) fn lambda_target_of_desc(d: &str, ctx: &Ctx) -> Option<super::ctx::LambdaTarget> {
    if !d.starts_with('L') {
        return None;
    }
    let name = d.trim_start_matches('L').trim_end_matches(';');
    if ctx.iface_sam.contains_key(name) {
        Some(super::ctx::LambdaTarget { iface: name.to_string(), args: Vec::new() })
    } else {
        None
    }
}

/// 描述符的参数列表 → 类型表
fn desc_param_types(desc: &str) -> Vec<Type> {
    desc_params(desc).iter().map(|d| type_from_desc(d)).collect()
}

/// 把 lambda / 方法引用编成「合成方法 + invokedynamic」。
///
/// `params` 是 lambda 的形参（方法引用传空表）；`as_instance` 说合成方法要不要
/// `this`（体里用到了 `this` / 实例成员时就应当是实例方法）。
fn emit_lambda(
    body: Stmt,
    params: Vec<(String, Type)>,
    as_instance: bool,
    em: &mut Emit,
    ctx: &Ctx,
    cp: &mut CpBuilder,
) -> Result<(), CompileError> {
    let Some(tgt) = ctx.lambda_target.borrow().clone() else {
        return err("推断不出 lambda 的目标类型（功能性接口）");
    };
    let iface = tgt.iface.clone();
    let Some((sam_name, sam_desc)) = ctx.iface_sam.get(&iface).cloned() else {
        return err(format!("`{}` 不是功能性接口，实现不了 lambda", iface.replace('/', ".")));
    };
    let sam_params = instantiate_sam(&desc_param_types(&sam_desc), &tgt.args);

    // 捕获：体里出现、且在外层是局部变量的名字（按槽序，保证确定性）
    let mut names: Vec<String> = Vec::new();
    collect_idents_s(&body, &mut names);
    let mut caps: Vec<(String, u8, Type)> = Vec::new();
    for n in &names {
        if params.iter().any(|(p, _)| p == n) || n == "<this-class>" {
            continue;
        }
        if let Some(slot) = ctx.locals.get(n) {
            let ty = ctx.local_ty.get(n).cloned().unwrap_or(Type::Named {
                path: vec!["java/lang/Object".to_string()],
                args: Vec::new(),
            });
            caps.push((n.clone(), *slot, ty));
        }
    }
    caps.sort_by_key(|(_, s, _)| *s);

    let seq = {
        let mut g = ctx.lambda_seq.borrow_mut();
        let v = *g;
        *g += 1;
        v
    };
    let impl_name = format!("lambda${}${}", sanitize_method_name(&ctx.method_name), seq);
    let mut impl_desc = String::from("(");
    for (_, _, t) in &caps {
        impl_desc.push_str(&descriptor(t));
    }
    for t in &sam_params {
        impl_desc.push_str(&descriptor(t));
    }
    impl_desc.push(')');
    impl_desc.push_str(sam_desc.rsplit(')').next().unwrap_or("V"));

    ctx.synth.borrow_mut().push(super::ctx::SynthMethod {
        name: impl_name.clone(),
        desc: impl_desc.clone(),
        captures: caps.iter().map(|(n, _, t)| (n.clone(), t.clone())).collect(),
        params,
        body,
        is_instance: as_instance,
    });

    // 调用点：先压 `this`（REF_invokeVirtual 的接收者），再压捕获
    let mut indy_desc = String::from("(");
    if as_instance {
        indy_desc.push('L');
        indy_desc.push_str(&ctx.class_name);
        indy_desc.push(';');
        em.op(0x2A); // aload_0
        em.push(1);
    }
    let mut slots = if as_instance { 1u16 } else { 0 };
    for (_, slot, t) in &caps {
        let kind = kind_of(t);
        em.load_slot(*slot, kind)?;
        let w = if kind_wide(kind) { 2 } else { 1 };
        em.push(w);
        slots += w;
        indy_desc.push_str(&descriptor(t));
    }
    indy_desc.push(')');
    indy_desc.push('L');
    indy_desc.push_str(&iface);
    indy_desc.push(';');

    // 引导方法：LambdaMetafactory.metafactory(Lookup, String, MethodType, MethodType,
    //                                        MethodHandle, MethodType)
    let kind = if as_instance { 5u8 } else { 6u8 }; // REF_invokeVirtual / REF_invokeStatic
    let impl_handle = cp.method_handle(kind, &ctx.class_name, &impl_name, &impl_desc);
    let sam_mt = cp.method_type(&sam_desc);
    // instantiatedMethodType：代入类型实参之后的签名（必须是 samMethodType 的特化）
    let inst_desc = {
        let mut d = String::from("(");
        for t in &sam_params {
            d.push_str(&descriptor(t));
        }
        d.push(')');
        d.push_str(sam_desc.rsplit(')').next().unwrap_or("V"));
        d
    };
    let inst_mt_ty = cp.method_type(&inst_desc);
    let bsm_ref = cp.method_handle(
        6, // REF_invokeStatic
        "java/lang/invoke/LambdaMetafactory",
        "metafactory",
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/CallSite;",
    );
    let bsm = cp.bootstrap(bsm_ref, vec![sam_mt, impl_handle, inst_mt_ty]);
    let idx = cp.invoke_dynamic(&sam_name, &indy_desc, bsm);
    em.op(0xBA); // invokedynamic
    em.bytes.extend_from_slice(&idx.to_be_bytes());
    em.bytes.push(0);
    em.bytes.push(0);
    em.pop(slots);
    em.push(1);
    Ok(())
}

/// SAM 形参名 + 擦除类型（`params: Vec<String>` 来自 parser，类型从 SAM 描述符取）
fn sam_params_named(names: &[String], desc: &str, targs: &[Type]) -> Vec<(String, Type)> {
    let tys = instantiate_sam(&desc_param_types(desc), targs);
    names
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), tys.get(i).cloned().unwrap_or(Type::Prim("int"))))
        .collect()
}

/// 把 SAM 描述符里**擦除成 `Object` 的形参**换成目标类型给的类型实参。
///
/// `Consumer<String>` 的 SAM 擦成 `accept(Object)`：不代回去，lambda 体里
/// `s.isEmpty()` 就按 `Object` 接收者解析（报「未找到实例方法」）。
/// 返回值**故意不代**：那需要真正的类型推断（还要处理装箱/拆箱），代错了会变成
/// VerifyError；返回 `Object` 在字节码层面永远合法。
fn instantiate_sam(params: &[Type], targs: &[Type]) -> Vec<Type> {
    let mut out = params.to_vec();
    if targs.is_empty() {
        return out;
    }
    let obj = |t: &Type| match t {
        Type::Named { path, args } => args.is_empty() && path.len() == 1 && path[0] == "java/lang/Object",
        _ => false,
    };
    let idx: Vec<usize> = params.iter().enumerate().filter(|(_, t)| obj(t)).map(|(i, _)| i).collect();
    if idx.is_empty() {
        return out;
    }
    if targs.len() == 1 {
        // `Comparator<T>`：两个 Object 形参共用一个类型实参
        for i in &idx {
            out[*i] = targs[0].clone();
        }
    } else {
        // `Function<A,R>` / `BiFunction<A,B,R>`：末尾多出来的那个是返回值，只代形参
        for (k, i) in idx.iter().enumerate() {
            if let Some(a) = targs.get(k) {
                out[*i] = a.clone();
            }
        }
    }
    out
}

/// 体里用没用到 `this`（含隐式：实例字段 / 实例方法 / 裸 `this`）。
///
/// 判据保守：只要体里出现了不是局部变量、但在本类可见的名字，就认为要 `this`。
/// 多认得一张实例方法也不必 —— 合成方法用实例形态永远合法（除了静态方法里没有
/// `this`，那种情况调用点会自然报错）。
fn body_uses_this(body: &Stmt, ctx: &Ctx) -> bool {
    if !ctx.has_this {
        return false;
    }
    let mut names: Vec<String> = Vec::new();
    collect_idents_s(body, &mut names);
    names.iter().any(|n| {
        // 只看**实例**成员：静态字段/静态方法不需要 this
        ctx.locals.get(n).is_none()
            && (ctx.fields.contains_key(n) || ctx.instance_methods.contains_key(n))
    })
}

/// 合成方法名里不能出现 `<` `>`（JVM 规范 4.2.2 限制方法名），构造器叫 `<init>`：
/// 直接拼进 `lambda$<init>$0` 会得到 `ClassFormatError: Illegal method name`。
fn sanitize_method_name(n: &str) -> String {
    match n {
        "<init>" => "new".to_string(),
        "<clinit>" => "clinit".to_string(),
        _ => n.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$').collect(),
    }
}
