//! 表达式编译。

use crate::ast::*;

use super::constpool::CpBuilder;
use super::ctx::Ctx;
use super::emit::Emit;
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
            compile_expr(array, em, ctx, cp)?;
            compile_expr(index, em, ctx, cp)?;
            em.op(match k {
                K_LONG => 0x2F,   // laload
                K_DOUBLE => 0x31, // daload
                K_REF => 0x32,    // aaload
                _ => 0x2E,        // iaload
            });
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
            em.op_u2(0xBB, cls); // new
            em.push(1);
            em.op(0x59); // dup
            em.push(1);
            for a in args {
                compile_expr(a, em, ctx, cp)?;
            }
            let init = cp.method_ref(&owner, "<init>", &ctor_desc);
            em.op_u2(0xB7, init); // invokespecial
            let n = args.len() as u16 + 1;
            em.pop(n);
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
                        em.op_u2(0xB2, out);
                        em.push(1);
                        if args.len() != 1 {
                            return err("println 目前只支持一个参数");
                        }
                        compile_expr(&args[0], em, ctx, cp)?;
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
            em.op(0x2A); // aload_0
            em.push(1);
            for a in args {
                compile_expr(a, em, ctx, cp)?;
            }
            let idx = cp.method_ref(&ctx.class_name, "<init>", &desc);
            em.op_u2(0xB7, idx); // invokespecial
            em.pop(args.len() as u16 + 1);
            Ok(())
        }
        // super(...) → invokespecial 父类 <init>（父类构造器）
        Expr::Call { recv: Some(r), name, args, .. } if name == "super" && matches!(**r, Expr::Super) => {
            if !ctx.in_ctor {
                return err("super(...) 只能出现在构造器里");
            }
            let desc = format!("({})V", args_desc(args, ctx));
            em.op(0x2A); // aload_0
            em.push(1);
            for a in args {
                compile_expr(a, em, ctx, cp)?;
            }
            let idx = cp.method_ref(&ctx.super_name, "<init>", &desc);
            em.op_u2(0xB7, idx); // invokespecial
            em.pop(args.len() as u16 + 1);
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
            let n = args.len() as u16 + 1;
            em.pop(n);
            if !desc.ends_with(")V") {
                em.push(if kind_wide(kind_from_ret(&desc)) { 2 } else { 1 });
            }
            Ok(())
        }
        // 调用对象方法 a.get()
        Expr::Call { recv: Some(r), name, args, .. } => {
            // 静态方法：`ClassName.m(...)` → invokestatic（owner 就是那个类）
            if let Some((owner, desc)) = static_call(r, name, ctx) {
                for a in args {
                    compile_expr(a, em, ctx, cp)?;
                }
                let idx = cp.method_ref(&owner, name, &desc);
                em.op_u2(0xB8, idx); // invokestatic
                em.pop(args.len() as u16);
                if !desc.ends_with(")V") {
                    em.push(if kind_wide(kind_from_ret(&desc)) { 2 } else { 1 });
                }
                return Ok(());
            }
            let desc = call_desc(Some(r), name, ctx)
                .ok_or_else(|| CompileError { msg: format!("未找到实例方法 `{name}`") })?;
            compile_expr(r, em, ctx, cp)?;
            for a in args {
                compile_expr(a, em, ctx, cp)?;
            }
            // 引用类用接收者的**静态类型**：验证器要求接收者可赋给引用类。
            // 分派本身按运行时类走（虚方法覆写），引用类只决定从哪条链开始解析。
            let owner = expr_type(r, ctx).as_ref().and_then(type_internal).unwrap_or_else(|| ctx.class_name.clone());
            let n = args.len() as u16 + 1;
            if ctx.iface_types.contains(&owner) {
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
            em.pop(n);
            // 返回值占的槽数：long/double 占 2 槽
            if !desc.ends_with(")V") {
                em.push(if kind_wide(kind_from_ret(&desc)) { 2 } else { 1 });
            }
            Ok(())
        }
        // 调用本类（或父类）的方法：静态走 invokestatic，实例走 this 的 invokevirtual
        Expr::Call { name, args, .. } => {
            if let Some(desc) = ctx.static_methods.get(name.as_str()).cloned() {
                for a in args {
                    compile_expr(a, em, ctx, cp)?;
                }
                let idx = cp.method_ref("<SELF>", name, &desc);
                em.op_u2(0xB8, idx);
                let argc = args.len() as u16;
                em.pop(argc);
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
            em.op(0x2A); // aload_0
            em.push(1);
            for a in args {
                compile_expr(a, em, ctx, cp)?;
            }
            let idx = cp.method_ref(&ctx.class_name, name, &desc);
            em.op_u2(0xB6, idx); // invokevirtual
            let n = args.len() as u16 + 1;
            em.pop(n);
            if !desc.ends_with(")V") {
                em.push(if kind_wide(kind_from_ret(&desc)) { 2 } else { 1 });
            }
            Ok(())
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
                compile_expr(lhs, em, ctx, cp)?;
                coerce(kl, k, em)?;
                compile_expr(rhs, em, ctx, cp)?;
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
            compile_expr(lhs, em, ctx, cp)?;
            coerce(kl, k, em)?;
            compile_expr(rhs, em, ctx, cp)?;
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
        other => err(format!("暂不支持的表达式: {other:?}")),
    }
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
        compile_expr(lhs, em, ctx, cp)?;
        compile_expr(rhs, em, ctx, cp)?;
        em.pop(2);
        return Ok(Some(if matches!(op, BinOp::Eq) {
            (0xA6, 0xA5) // 假 = if_acmpne；真 = if_acmpeq
        } else {
            (0xA5, 0xA6)
        }));
    }
    let k = promote(kl, kr);
    compile_expr(lhs, em, ctx, cp)?;
    coerce(kl, k, em)?;
    compile_expr(rhs, em, ctx, cp)?;
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
