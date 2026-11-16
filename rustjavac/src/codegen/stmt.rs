//! 语句编译：控制流、try/catch/finally、switch、break/continue 分支。

use crate::ast::*;

use super::constpool::CpBuilder;
use super::ctx::Ctx;
use super::emit::{Emit, SCRATCH_SLOT};
use super::error::*;
use super::expr::*;
use super::method::*;
use super::types::*;


pub(crate) fn compile_stmt(s: &Stmt, em: &mut Emit, ctx: &mut Ctx, cp: &mut CpBuilder) -> Result<bool, CompileError> {
    match s {
        Stmt::Block(items) => {
            // 块内声明的局部变量出了块就不可见：验证类型要退回入口快照，
            // 否则后续合并帧会声明一个「别处是 top」的槽（VerifyError）
            let scope = em.local_types.clone();
            for it in items {
                if compile_stmt(it, em, ctx, cp)? {
                    return Ok(true);
                }
            }
            em.local_types = scope;
            Ok(false)
        }
        Stmt::Return(None) => compile_return(&None, em, ctx, cp),
        Stmt::Return(Some(e)) => compile_return(&Some(e.clone()), em, ctx, cp),
        Stmt::Expr(e) if is_println_call(e) => {
            compile_expr(e, em, ctx, cp)?;
            Ok(false)
        }
        Stmt::Expr(e) if !matches!(e, Expr::Assign { .. }) => {
            let kind = expr_kind(e, ctx);
            compile_expr(e, em, ctx, cp)?;
            if is_void_call(e, ctx) {
                return Ok(false); // void 方法调用没有返回值可弹
            }
            if kind_wide(kind) {
                em.op(0x58); // pop2
                em.pop(2);
            } else {
                em.op(0x57); // pop
                em.pop(1);
            }
            Ok(false)
        }
        Stmt::LocalVar { ty, name, init } => {
            let slot = ctx.next_slot;
            if slot >= SCRATCH_SLOT {
                return err(format!(
                    "局部变量太多：槽位已用到 {slot}，最高可用 {}（{} 号槽留给比较结果）",
                    SCRATCH_SLOT - 1,
                    SCRATCH_SLOT
                ));
            }
            let kind = kind_of(ty);
            let wide_local = kind_wide(kind);
            ctx.next_slot += if wide_local { 2 } else { 1 };
            if em.max_locals < ctx.next_slot as u16 {
                em.max_locals = ctx.next_slot as u16;
            }
            ctx.locals.insert(name.clone(), slot);
            ctx.local_kind.insert(name.clone(), kind);
            ctx.local_ty.insert(name.clone(), ty.clone());
            if let Some(e) = init {
                compile_expr(e, em, ctx, cp)?;
            }
            // 槽的验证类型必须在**初始化表达式编译完之后**才登记：
            // 初始化式里如果带跳转（如 `boolean f = x == null;`），那时候这个槽还没赋值，
            // 帧里抢先写 ITEM_Integer 会被 HotSpot 判成 VerifyError（top 不可赋给 integer）。
            let vt = ver_type(&mut *cp, ty);
            em.set_local_type(slot as usize, vt);
            if wide_local {
                // 第 2 槽没有自己的类型，写「空」占位
                em.set_local_type(slot as usize + 1, u16::MAX);
            }
            if let Some(e) = init {
                let _ = e;
                em.store_slot(slot, kind)?;
            }
            Ok(false)
        }
        Stmt::Expr(Expr::Assign { op: None, lhs, rhs }) if matches!(&**lhs, Expr::ArrayAccess { .. }) => {
            let (arr, idx) = match &**lhs {
                Expr::ArrayAccess { array, index } => (array.clone(), index.clone()),
                _ => unreachable!(),
            };
            let k = elem_kind(&arr, ctx);
            let sa = if needs_scratch(&arr) { Some(spill(&arr, em, ctx, cp)?) } else { None };
            let si = if needs_scratch(&idx) { Some(spill(&idx, em, ctx, cp)?) } else { None };
            let sr = if needs_scratch(rhs) { Some(spill(rhs, em, ctx, cp)?) } else { None };
            emit_val(&arr, &sa, em, ctx, cp)?;
            emit_val(&idx, &si, em, ctx, cp)?;
            emit_val(rhs, &sr, em, ctx, cp)?;
            em.op(array_ops(&arr, ctx).1);
            em.pop(if kind_wide(k) { 4 } else { 3 });
            Ok(false)
        }
        Stmt::Expr(Expr::Assign { op: None, lhs, rhs }) if matches!(&**lhs, Expr::Field(..)) => {
            // 静态字段：`StaticCounter.count = ...`
            if let Some((owner, desc)) = static_field_access(lhs, ctx) {
                let fname = match &**lhs {
                    Expr::Field(_, n) => n.clone(),
                    _ => unreachable!(),
                };
                compile_expr(rhs, em, ctx, cp)?;
                let f = cp.field_ref(&owner, &fname, &desc);
                em.op_u2(0xB3, f); // putstatic
                em.pop(if kind_wide(kind_from_desc(&desc)) { 2 } else { 1 });
                return Ok(false);
            }
            let (recv, fname) = match &**lhs {
                Expr::Field(r, n) => (r.clone(), n.clone()),
                _ => unreachable!(),
            };
            let is_super = matches!(*recv, Expr::Super);
            // rhs 会分叉时先算进脏槽：接收者压上栈之后，跳转点就不空了
            let sr = if needs_scratch(rhs) { Some(spill(rhs, em, ctx, cp)?) } else { None };
            if is_super {
                em.op(0x2A); // aload_0
                em.push(1);
            } else {
                compile_expr(&recv, em, ctx, cp)?;
            }
            emit_val(rhs, &sr, em, ctx, cp)?;
            let (mut owner, desc) = field_access(&recv, &fname, ctx);
            if is_super {
                owner = ctx.super_name.clone();
            }
            let f = cp.field_ref(&owner, &fname, &desc);
            em.op_u2(0xB5, f); // putfield
            em.pop(if kind_wide(kind_from_desc(&desc)) { 3 } else { 2 });
            Ok(false)
        }
        Stmt::Expr(Expr::Assign { op: None, lhs, rhs }) => {
            let name = match &**lhs {
                Expr::Ident(n) => n.clone(),
                _ => return err("只支持给局部变量赋值"),
            };
            let Some(slot) = ctx.lookup(&name) else {
                // 本类的静态字段（`count = count + 1;`）
                if let Some((owner, desc)) = ctx.static_fields.get(&name).cloned() {
                    compile_expr(rhs, em, ctx, cp)?;
                    let f = cp.field_ref(&owner, &name, &desc);
                    em.op_u2(0xB3, f); // putstatic
                    em.pop(if kind_wide(kind_from_desc(&desc)) { 2 } else { 1 });
                    return Ok(false);
                }
                // 不是局部变量 → 隐式 this 字段（`x = v;` 这种写法）
                let (owner, desc) = field_access(&Expr::This, &name, ctx);
                let Some(_) = ctx.fields.get(&name) else {
                    return err(format!("未定义的变量 `{name}`"));
                };
                em.op(0x2A); // aload_0
                em.push(1);
                compile_expr(rhs, em, ctx, cp)?;
                let f = cp.field_ref(&owner, &name, &desc);
                em.op_u2(0xB5, f); // putfield
                em.pop(if kind_wide(kind_from_desc(&desc)) { 3 } else { 2 });
                return Ok(false);
            };
            let kind = *ctx.local_kind.get(&name).unwrap_or(&K_INT);
            compile_expr(rhs, em, ctx, cp)?;
            em.store_slot(slot, kind)?;
            Ok(false)
        }
        Stmt::Switch { selector, cases } => {
            // 枚举的选择子是引用：比较前先取 ordinal()（javac 也是这么编的，
            // switch 的表只认 int）。case 标签同样是枚举常量，两边一起取。
            let enum_switch = expr_kind(selector, ctx) == K_REF;
            // `case RED:` 里的 RED 得挂到枚举类上（`Color.RED`），否则按本类的静态字段找。
            // 标签不做种类推断：RED 是另一个类的静态字段，expr_kind 推不出来。
            let enum_cls = expr_type(selector, ctx).as_ref().and_then(type_internal);
            let pin = |e: &Expr, ctx: &Ctx, is_label: bool| -> Expr {
                if !enum_switch {
                    return e.clone();
                }
                if is_label {
                    let base = match (e, &enum_cls) {
                        (Expr::Ident(n), Some(cls)) if ctx.lookup(n).is_none() => {
                            Expr::Field(Box::new(Expr::Ident(cls.clone())), n.clone())
                        }
                        _ => e.clone(),
                    };
                    return Expr::Call {
                        recv: Some(Box::new(base)),
                        name: "ordinal".into(),
                        args: Vec::new(),
                        type_args: Vec::new(),
                    };
                }
                if expr_kind(e, ctx) != K_REF {
                    return e.clone();
                }
                Expr::Call {
                    recv: Some(Box::new(e.clone())),
                    name: "ordinal".into(),
                    args: Vec::new(),
                    type_args: Vec::new(),
                }
            };
            // 用一个临时槽保存选择子
            let sel_slot = ctx.next_slot;
            ctx.next_slot += 1;
            if em.max_locals < ctx.next_slot as u16 {
                em.max_locals = ctx.next_slot as u16;
            }
            compile_expr(&pin(selector, ctx, false), em, ctx, cp)?;
            em.istore(sel_slot)?;
            em.set_local_type(sel_slot as usize, 0); // int

            // 为每个 case 建标签；线性比较链 + goto
            let mut body_labels = Vec::new();
            for _ in cases {
                body_labels.push(em.new_label());
            }
            let end_label = em.new_label();
            // default 体在哪里？没有 default 就跳到结尾
            let default_target = cases
                .iter()
                .position(|c| c.labels.iter().any(|l| l.is_none()))
                .map(|i| body_labels[i])
                .unwrap_or(end_label);

            for (i, c) in cases.iter().enumerate() {
                for label in &c.labels {
                    match label {
                        Some(v) => {
                            em.iload(sel_slot)?;
                            compile_expr(&pin(v, ctx, true), em, ctx, cp)?;
                            let next = em.new_label();
                            // 不相等就继续比下一个
                            em.branch(0xa0, next); // if_icmpne
                            em.pop(2);
                            em.goto(body_labels[i]);
                            em.bind(next);
                        }
                        None => {
                            // default 不参与比较链：链条走完自然落到它
                        }
                    }
                }
            }
            em.goto(default_target);

            // 依次发射各 case 体（自然贯穿）
            let scope = em.local_types.clone();
            ctx.break_labels.push(end_label);
            for (i, c) in cases.iter().enumerate() {
                em.local_types = scope.clone();
                em.bind(body_labels[i]);
                for s in &c.body {
                    compile_stmt(s, em, ctx, cp)?;
                }
            }
            // end_label 是 break 的落点（也可能没有任何 break 跳到它，照样要绑定）
            em.local_types = scope.clone();
            em.bind(end_label);
            ctx.break_labels.pop();
            // 不声明「已终结」：switch 之后可能还有语句（case 里 break 出来后要接着执行）
            Ok(false)
        }
        Stmt::Break(None) => {
            let Some(l) = ctx.break_labels.last().copied() else {
                return err("`break` 不在循环或 switch 里");
            };
            em.goto(l);
            Ok(true)
        }
        Stmt::Continue(None) => {
            let Some(l) = ctx.cont_labels.last().copied() else {
                return err("`continue` 不在循环里");
            };
            em.goto(l);
            Ok(true)
        }
        Stmt::Break(Some(l)) | Stmt::Continue(Some(l)) => {
            err(format!("暂不支持带标签的 `{l}`（break/continue 后跟标签）"))
        }
        Stmt::For { init, cond, update, body } => {
            let scope = em.local_types.clone();
            for s in init {
                compile_stmt(s, em, ctx, cp)?;
            }
            let start = em.new_label();
            let end = em.new_label();
            // continue 跳到更新表达式之前
            let cont = em.new_label();
            em.bind(start);
            if let Some(c) = cond {
                compile_cond_false(c, em, ctx, cp, end)?;
            }
            ctx.break_labels.push(end);
            ctx.cont_labels.push(cont);
            compile_stmt(body, em, ctx, cp)?;
            ctx.break_labels.pop();
            ctx.cont_labels.pop();
            em.bind(cont);
            for u in update {
                // 更新表达式可能是赋值，交给语句编译器处理
                compile_stmt(&Stmt::Expr(u.clone()), em, ctx, cp)?;
            }
            // 循环里声明的变量出了循环就不可见：汇合帧必须 chop 掉它们，
            // 否则从循环外跳进来的路径会判 "top is not assignable to integer"
            em.local_types = scope;
            em.goto(start);
            em.bind(end);
            Ok(false)
        }
        Stmt::While { cond, body } => {
            let scope = em.local_types.clone();
            let start = em.new_label();
            let end = em.new_label();
            em.bind(start);
            compile_cond_false(cond, em, ctx, cp, end)?;
            ctx.break_labels.push(end);
            ctx.cont_labels.push(start);
            compile_stmt(body, em, ctx, cp)?;
            ctx.break_labels.pop();
            ctx.cont_labels.pop();
            em.local_types = scope;
            em.goto(start);
            em.bind(end);
            Ok(false)
        }
        Stmt::If { cond, then, els } => {
            let l_else = em.new_label();
            let l_end = em.new_label();
            compile_cond_false(cond, em, ctx, cp, l_else)?;
            // 两个分支各自声明的局部变量都不能出现在合并帧里
            let scope = em.local_types.clone();
            compile_stmt(then, em, ctx, cp)?;
            match els {
                Some(e) => {
                    em.local_types = scope.clone();
                    em.goto(l_end);
                    em.bind(l_else);
                    compile_stmt(e, em, ctx, cp)?;
                    em.local_types = scope;
                    em.bind(l_end);
                }
                None => {
                    em.local_types = scope;
                    em.bind(l_else);
                }
            }
            Ok(false)
        }
        Stmt::Throw(e) => {
            compile_expr(e, em, ctx, cp)?;
            em.op(0xBF); // athrow
            em.pop(1);
            Ok(true) // 之后不可达
        }
        Stmt::Try { body, catches, finally } => {
            // try 入口的局部变量快照：handler / 汇合帧只声明到这一层，多出来的槽按
            // Top 处理 —— 栈图里 Top 接受任何类型，反过来「声明得比实际多」才会 VerifyError。
            let entry = em.local_types.clone();
            let start = em.new_label();
            let body_end = em.new_label();
            let end = em.new_label();
            let fin_lbl = em.new_label(); // 正常路径与 catch 路径共用的一份 finally
            let fin_all = em.new_label(); // catch-all：接住任何异常 → 跑一份 finally → 再抛
            let fin = finally.as_deref();
            let normal_target = if fin.is_some() { fin_lbl } else { end };

            if let Some(f) = fin {
                ctx.finally_stack.push(f.clone());
            }
            em.bind(start);
            let body_terminated = compile_stmt(body, em, ctx, cp)?;
            // finally 一直挂在 ctx 上，直到**所有 catch 体**也编译完 —— catch 体里的
            // return 同样要先把 finally 跑一遍。
            // 保护区的结束位置。这条 goto 已经落在保护区外（goto 本身不会抛）。
            em.place(body_end);
            if !body_terminated {
                em.goto(normal_target);
            }

            let mut any_catch_falls = false;
            let mut fin_terminated = true; // 没有 finally 时由下面的分支单独处理
            for c in catches {
                let handler = em.new_label();
                let catch_ty = ver_type(cp, &c.ty); // 常量池 Class 索引
                em.bind_handler(handler, entry.clone(), catch_ty);
                em.push(1); // 栈上有一个异常对象（bind_handler 只写栈图，不记账）
                let slot = bump_slot(ctx, em, false)?;
                em.set_local_type(slot as usize, catch_ty);
                em.store_slot(slot, K_REF)?; // astore
                let saved = (
                    ctx.locals.insert(c.name.clone(), slot),
                    ctx.local_kind.insert(c.name.clone(), K_REF),
                    ctx.local_ty.insert(c.name.clone(), c.ty.clone()),
                );
                let catch_terminated = compile_stmt(&c.body, em, ctx, cp)?;
                any_catch_falls |= !catch_terminated;
                // 恢复作用域：catch 变量出了 catch 块就不可见
                match saved.0 {
                    Some(v) => ctx.locals.insert(c.name.clone(), v),
                    None => ctx.locals.remove(&c.name),
                };
                match saved.1 {
                    Some(v) => ctx.local_kind.insert(c.name.clone(), v),
                    None => ctx.local_kind.remove(&c.name),
                };
                match saved.2 {
                    Some(v) => ctx.local_ty.insert(c.name.clone(), v),
                    None => ctx.local_ty.remove(&c.name),
                };
                // catch 体的保护区到此结束（finally 的 catch-all 也要盖住 catch 体）
                let h_end = em.new_label();
                em.place(h_end);
                if !catch_terminated {
                    em.goto(normal_target);
                }
                em.exc(start, body_end, handler, catch_ty);
                if fin.is_some() {
                    em.exc(handler, h_end, fin_all, 0);
                }
            }
            // catch-all 必须排在所有具体 catch 之后（异常表按顺序匹配）
            if fin.is_some() {
                em.exc(start, body_end, fin_all, 0);
            }

            if fin.is_some() {
                ctx.finally_stack.pop(); // finally 自己的副本里不能再展开自己
            }
            // 有没有路径能落到 end？try 体和所有 catch 都终结时，这份共用副本
            // 根本没人跳进来（代码仍在，但只能补一条不可达的返回）。
            let normal_reaches = !body_terminated || any_catch_falls;
            if let Some(f) = fin {
                // finally 里 catch 变量已经出了作用域：把验证类型退回入口快照，
                // 否则 finally 体内部的帧会声明「某路径上其实没初始化」的槽。
                em.local_types = entry.clone();
                em.bind_handler(fin_lbl, entry.clone(), u16::MAX);
                fin_terminated = compile_stmt(f, em, ctx, cp)?;
                if !fin_terminated {
                    if normal_reaches {
                        em.goto(end);
                    } else {
                        // 没人会跳进这份副本：补一条不可达的返回，别让控制流掉出方法末尾
                        emit_zero_return(em, ctx.ret_ty.clone());
                    }
                }
                // 第二份：异常路径。跑完 finally 再把原异常抛出去。
                let throwable = cp.class("java/lang/Throwable");
                em.bind_handler(fin_all, entry.clone(), throwable);
                em.push(1);
                let xslot = bump_slot(ctx, em, false)?;
                em.set_local_type(xslot as usize, throwable);
                em.store_slot(xslot, K_REF)?; // astore
                let fin2_terminated = compile_stmt(f, em, ctx, cp)?;
                if !fin2_terminated {
                    em.load_slot(xslot, K_REF)?;
                    em.op(0xBF); // athrow
                    em.pop(1);
                }
            }
            // 有没有路径能落到 end？全都有终结（return/throw）时这里就没后续代码了：
            // 不能绑定 end 的帧（偏移会等于 code_length，StackMapTable 报 bad offset），
            // 也必须报「已终结」，否则外层会补一条不可达的默认返回。
            let reaches_end = if fin.is_some() { normal_reaches && !fin_terminated } else { normal_reaches };
            // 汇合点：正常路径、每条 catch 都到这里。
            // 帧只声明 try 入口那份快照（多出来的槽按 Top：Top 接受任何类型）。
            if reaches_end {
                em.bind_handler(end, entry.clone(), u16::MAX);
            }
            // catch 变量与 try 体内声明的槽出了语句就不可见：把验证类型退回入口快照，
            // 否则下一条 try 的 handler 帧会声明一个「实际未初始化」的槽（VerifyError）。
            em.local_types = entry;
            Ok(!reaches_end)
        }
        other => err(format!("暂不支持的语句: {other:?}")),
    }
}

