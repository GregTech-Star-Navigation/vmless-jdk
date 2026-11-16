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
    let Expr::Call { recv, name, .. } = e else { return false };
    // super(...) / this(...) / super.m() 都是 invokespecial，没有返回值可弹
    if name == "super" || name == "this" {
        return true;
    }
    matches!(call_desc(recv.as_deref(), name, ctx), Some(d) if d.ends_with(")V"))
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

pub(crate) fn compile_expr(e: &Expr, em: &mut Emit, ctx: &Ctx, cp: &mut CpBuilder) -> Result<(), CompileError> {
    match e {
        Expr::IntLit(s) => {
            if s.ends_with('L') || s.ends_with('l') {
                let v: i64 = s[..s.len() - 1].parse().map_err(|_| CompileError { msg: format!("long 字面量 `{s}` 解析失败") })?;
                if v == 0 || v == 1 {
                    em.op(0x09 + v as u8); // lconst_0/1
                } else {
                    let idx = cp.long(v);
                    em.op_u2(0x14, idx); // ldc2_w
                }
                em.push(2);
                return Ok(());
            }
            let v: i32 = s.parse().map_err(|_| CompileError { msg: format!("整数字面量 `{s}` 解析失败") })?;
            if (-32768..=32767).contains(&v) {
                em.const_int(v);
            } else {
                // 超出 sipush 范围，走常量池 ldc_w
                let idx = cp.int(v);
                em.op_u2(0x13, idx);
                em.push(1);
            }
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
            let ctor_desc = format!("({})V", args_desc(args, ctx));
            let cls = cp.class(&owner);
            let sp = hoist_args(args, em, ctx, cp)?;
            em.op_u2(0xBB, cls); // new
            em.push(1);
            em.op(0x59); // dup
            em.push(1);
            for (a, s) in args.iter().zip(sp.iter()) {
                emit_val(a, s, em, ctx, cp)?;
            }
            let init = cp.method_ref(&owner, "<init>", &ctor_desc);
            em.op_u2(0xB7, init); // invokespecial
            em.pop(desc_slots(&ctor_desc) + 1);
            Ok(())
        }
        Expr::Cast { ty, expr } => {
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
                        // double → (D)V，引用 → (Ljava/lang/String;)V
                        let (desc, width) = match expr_kind(&args[0], ctx) {
                            K_LONG => ("(J)V", 2u16),
                            K_DOUBLE => ("(D)V", 2),
                            K_REF => (ref_println_desc(&args[0], ctx), 1),
                            K_BOOL => ("(Z)V", 1),
                            _ => ("(I)V", 1),
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
            let desc = format!("({})V", args_desc(args, ctx));
            let sp = hoist_args(args, em, ctx, cp)?;
            em.op(0x2A); // aload_0
            em.push(1);
            for (a, s) in args.iter().zip(sp.iter()) {
                emit_val(a, s, em, ctx, cp)?;
            }
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
            let desc = format!("({})V", args_desc(args, ctx));
            let sp = hoist_args(args, em, ctx, cp)?;
            em.op(0x2A); // aload_0
            em.push(1);
            for (a, s) in args.iter().zip(sp.iter()) {
                emit_val(a, s, em, ctx, cp)?;
            }
            let idx = cp.method_ref(&ctx.super_name, "<init>", &desc);
            em.op_u2(0xB7, idx); // invokespecial
            em.pop(desc_slots(&desc) + 1);
            Ok(())
        }
        // super.m() → invokespecial 父类实现（owner 必须是父类，否则解析到自己）
        Expr::Call { recv: Some(r), name, args, .. } if matches!(**r, Expr::Super) => {
            let desc = call_desc(Some(r), name, ctx)
                .ok_or_else(|| CompileError { msg: format!("父类里未找到方法 `{name}`") })?;
            em.op(0x2A); // aload_0
            em.push(1);
            for a in args {
                compile_expr(a, em, ctx, cp)?;
            }
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
            if let Some((owner, desc)) = static_call(r, name, ctx) {
                let sp = hoist_args(args, em, ctx, cp)?;
                for (a, s) in args.iter().zip(sp.iter()) {
                    emit_val(a, s, em, ctx, cp)?;
                }
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
            let desc = call_desc(Some(r), name, ctx)
                .ok_or_else(|| CompileError { msg: format!("未找到实例方法 `{name}`") })?;
            let sp = hoist_args(args, em, ctx, cp)?;
            compile_expr(r, em, ctx, cp)?;
            for (a, s) in args.iter().zip(sp.iter()) {
                emit_val(a, s, em, ctx, cp)?;
            }
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
                for (a, s) in args.iter().zip(sp.iter()) {
                    emit_val(a, s, em, ctx, cp)?;
                }
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
            for (a, s) in args.iter().zip(sp.iter()) {
                emit_val(a, s, em, ctx, cp)?;
            }
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
            let kl = expr_kind(lhs, ctx);
            let kr = expr_kind(rhs, ctx);
            // 比较作为**表达式**（`boolean f = a < b;`）：产出 0/1
            if let Some((jump_false, _)) = compile_cmp_operands(e, em, ctx, cp)? {
                em.bool_from_branch(jump_false)?;
                return Ok(());
            }
            // 移位：右操作数恒为 int，左操作数决定是 int 还是 long 版本
            if matches!(op, BinOp::Shl | BinOp::Shr | BinOp::Ushr) {
                let k = if kl == K_LONG { K_LONG } else { K_INT };
                let sr = if needs_scratch(rhs) { Some(spill(rhs, em, ctx, cp)?) } else { None };
                compile_expr(lhs, em, ctx, cp)?;
                coerce(kl, k, em)?;
                emit_val(rhs, &sr, em, ctx, cp)?;
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
            compile_expr(lhs, em, ctx, cp)?;
            coerce(kl, k, em)?;
            emit_val(rhs, &sr, em, ctx, cp)?;
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
            let k = expr_kind(expr, ctx);
            compile_expr(expr, em, ctx, cp)?;
            match k {
                K_LONG => em.op(0x75),   // lneg
                K_DOUBLE => em.op(0x77), // dneg
                _ => em.op(0x74),        // ineg
            }
            Ok(())
        }
        // ~x：int 走 iconst_m1 + ixor，long 走 ldc2_w -1 + lxor（JVM 没有按位取反指令）
        Expr::Unary { op: UnOp::BitNot, expr } => {
            let k = expr_kind(expr, ctx);
            compile_expr(expr, em, ctx, cp)?;
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
        Expr::Unary { op: UnOp::Pos, expr } => compile_expr(expr, em, ctx, cp),
        // !x：求 0/1 再翻过来（非零 → 0）
        Expr::Unary { op: UnOp::Not, expr } => {
            compile_expr(expr, em, ctx, cp)?;
            em.pop(1);
            em.bool_from_branch(0x9A)?; // ifne → 「假」= 原值非零
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
    if kl == K_REF || kr == K_REF {
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
    let k = promote(kl, kr);
    // 任何一侧自带跳转（`(a<b) == (c<d)`）都要先算进脏槽：否则跳转点上压着另一侧的值
    let sa = if needs_scratch(lhs) { Some(spill(lhs, em, ctx, cp)?) } else { None };
    let sr = if needs_scratch(rhs) { Some(spill(rhs, em, ctx, cp)?) } else { None };
    emit_val(lhs, &sa, em, ctx, cp)?;
    coerce(kl, k, em)?;
    emit_val(rhs, &sr, em, ctx, cp)?;
    coerce(kr, k, em)?;
    // long/double 要先 lcmp / dcmpl(dcmpg) 压成 int，再用单值比较
    let widen = match k {
        K_LONG => Some(0x94u8), // lcmp
        // NaN 语义：< 和 <= 用 dcmpg，其余用 dcmpl
        K_DOUBLE => Some(if matches!(op, BinOp::Lt | BinOp::Le) { 0x98 } else { 0x97 }),
        _ => None,
    };
    if let Some(c) = widen {
        em.op(c);
        em.pop(3); // 两个宽值 4 槽 → 1 槽 int
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
    let kind = expr_kind(e, ctx);
    if kind_wide(kind) {
        return err("条件表达式不能是 long/double");
    }
    compile_expr(e, em, ctx, cp)?;
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
