//! 语句编译：控制流、try/catch/finally、switch、break/continue 分支。

use crate::ast::*;

use super::constpool::CpBuilder;
use super::ctx::{Ctx, JumpTarget};
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
                // 泛型擦除后 `String s = box.get();` 的右值是 Object：补 checkcast
                coerce_to(ty, e, ctx, em, cp)?;
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
            if let Some(Type::Array(inner)) = expr_type(&arr, ctx) {
                coerce_to(&inner, rhs, ctx, em, cp)?;
            }
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
                coerce_to(&type_from_desc(&desc), rhs, ctx, em, cp)?;
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
            coerce_to(&type_from_desc(&desc), rhs, ctx, em, cp)?;
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
                    coerce_to(&type_from_desc(&desc), rhs, ctx, em, cp)?;
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
                coerce_to(&type_from_desc(&desc), rhs, ctx, em, cp)?;
                let f = cp.field_ref(&owner, &name, &desc);
                em.op_u2(0xB5, f); // putfield
                em.pop(if kind_wide(kind_from_desc(&desc)) { 3 } else { 2 });
                return Ok(false);
            };
            let kind = *ctx.local_kind.get(&name).unwrap_or(&K_INT);
            compile_expr(rhs, em, ctx, cp)?;
            if let Some(t) = ctx.local_ty.get(&name).cloned() {
                coerce_to(&t, rhs, ctx, em, cp)?;
            }
            em.store_slot(slot, kind)?;
            Ok(false)
        }
        Stmt::Switch { selector, cases } => {
            // `switch` 的选择子是 String：javac 编成 hashCode+equals 两段，
            // 我们退化成 if-else 链（`String.equals` 已内建），语义相同
            if is_string_type(&expr_type(selector, ctx).unwrap_or(Type::Prim("int")))
                || matches!(selector, Expr::StrLit(_) | Expr::TextBlock(_))
            {
                return compile_string_switch(selector, cases, em, ctx, cp);
            }
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
            let lbl = ctx.pending_label.take();
            ctx.break_labels.push(JumpTarget { label: lbl, target: end_label });
            for (i, c) in cases.iter().enumerate() {
                em.local_types = scope.clone();
                em.bind(body_labels[i]);
                let mut term = false;
                for s in &c.body {
                    if term {
                        break; // 终结语句之后的代码不可达，编进去反而打乱栈图
                    }
                    term = compile_stmt(s, em, ctx, cp)?;
                }
                // 箭头语法（`case "a" -> ...`）**不贯穿**：体末尾自动 break
                if c.arrow && !term {
                    em.goto(end_label);
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
            let Some(t) = ctx.break_labels.last() else {
                return err("`break` 不在循环或 switch 里");
            };
            em.goto(t.target);
            Ok(true)
        }
        Stmt::Continue(None) => {
            let Some(t) = ctx.cont_labels.last() else {
                return err("`continue` 不在循环里");
            };
            em.goto(t.target);
            Ok(true)
        }
        // 带标签的 break/continue：从内向外找第一个同名落点（Java 的标签作用域规则）
        Stmt::Break(Some(l)) => {
            let Some(t) = find_target(&ctx.break_labels, l) else {
                return err(format!("`break {l}` 找不到同名标签"));
            };
            em.goto(t);
            Ok(true)
        }
        Stmt::Continue(Some(l)) => {
            let Some(t) = find_target(&ctx.cont_labels, l) else {
                return err(format!("`continue {l}` 找不到同名循环标签"));
            };
            em.goto(t);
            Ok(true)
        }
        // `outer: <语句>` —— 循环体的标签交给循环自己（break/continue 都要用），
        // 非循环语句只能被 `break label` 跳到语句末尾。
        Stmt::Labeled { label, body } => {
            if is_loop_stmt(body) {
                let prev = ctx.pending_label.replace(label.clone());
                let r = compile_stmt(body, em, ctx, cp)?;
                ctx.pending_label = prev;
                return Ok(r);
            }
            let end = em.new_label();
            let scope = em.local_types.clone();
            ctx.break_labels.push(JumpTarget { label: Some(label.clone()), target: end });
            let r = compile_stmt(body, em, ctx, cp)?;
            ctx.break_labels.pop();
            let _ = r;
            em.local_types = scope;
            em.bind(end);
            Ok(false)
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
            let lbl = ctx.pending_label.take();
            ctx.break_labels.push(JumpTarget { label: lbl.clone(), target: end });
            ctx.cont_labels.push(JumpTarget { label: lbl, target: cont });
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
        // `do { … } while (cond);` —— 先跑一遍体，条件为真再来一轮。
        // `continue` 跳到**条件判断**处（不是循环头），否则会跳过条件直接重跑。
        Stmt::DoWhile { body, cond } => {
            let scope = em.local_types.clone();
            let start = em.new_label();
            let cont = em.new_label();
            let end = em.new_label();
            em.bind(start);
            let lbl = ctx.pending_label.take();
            ctx.break_labels.push(JumpTarget { label: lbl.clone(), target: end });
            ctx.cont_labels.push(JumpTarget { label: lbl, target: cont });
            compile_stmt(body, em, ctx, cp)?;
            ctx.break_labels.pop();
            ctx.cont_labels.pop();
            // 循环体里声明的变量出了体就不可见：汇合帧要 chop 掉它们
            em.local_types = scope;
            em.bind(cont);
            compile_cond_false(cond, em, ctx, cp, end)?;
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
            let lbl = ctx.pending_label.take();
            ctx.break_labels.push(JumpTarget { label: lbl.clone(), target: end });
            ctx.cont_labels.push(JumpTarget { label: lbl, target: start });
            compile_stmt(body, em, ctx, cp)?;
            ctx.break_labels.pop();
            ctx.cont_labels.pop();
            em.local_types = scope;
            em.goto(start);
            em.bind(end);
            Ok(false)
        }
        // for-each 降糖成已有的 for / while：数组走「长度 + 下标」，
        // 其余按 `iterator()` / `hasNext()` / `next()` 协议（Iterable 的契约）。
        Stmt::ForEach { ty, name, iter, body } => compile_for_each(ty, name, iter, body, em, ctx, cp),
        Stmt::If { cond, then, els } => {
            let l_else = em.new_label();
            let l_end = em.new_label();
            compile_cond_false(cond, em, ctx, cp, l_else)?;
            // 两个分支各自声明的局部变量都不能出现在合并帧里
            let scope = em.local_types.clone();
            let then_term = compile_stmt(then, em, ctx, cp)?;
            match els {
                Some(e) => {
                    em.local_types = scope.clone();
                    // then 分支已经终结（return/throw）时不能再发 goto：那条指令是
                    // 不可达的，而不可达指令要么有栈图帧、要么只能出现在方法末尾，
                    // 否则 HotSpot 报 "Expecting a stack map frame at this location"
                    if !then_term {
                        em.goto(l_end);
                    }
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
                // 多重捕获（`catch (A | B e)`）：handler 只有一份，异常表里对每个类型
                // 各记一条（catch_type 不同）。帧里的栈类型必须是公共父类 ——
                // 声明成 A 而实际抛出 B 会被验证器判 VerifyError。
                let multi = c.types.len() > 1;
                let frame_ty = if multi {
                    cp.class("java/lang/Throwable")
                } else {
                    ver_type(cp, &c.types[0])
                };
                em.bind_handler(handler, entry.clone(), frame_ty);
                em.push(1); // 栈上有一个异常对象（bind_handler 只写栈图，不记账）
                let slot = bump_slot(ctx, em, false)?;
                em.set_local_type(slot as usize, frame_ty);
                em.store_slot(slot, K_REF)?; // astore
                // 多重捕获的变量按公共父类算（调用点解析不到具体类的方法，也不会猜错类型）
                let var_ty = if multi {
                    Type::Named { path: vec!["Throwable".into()], args: vec![] }
                } else {
                    c.types[0].clone()
                };
                let saved = (
                    ctx.locals.insert(c.name.clone(), slot),
                    ctx.local_kind.insert(c.name.clone(), K_REF),
                    ctx.local_ty.insert(c.name.clone(), var_ty),
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
                for t in &c.types {
                    let ct = ver_type(cp, t);
                    em.exc(start, body_end, handler, ct);
                }
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


/// `switch (String)`：if-else 链。
///
/// 选择子只求值一次（存进引用临时槽），每个 case 标签用 `equals` 逐个比。
/// `break` 仍然落到 end 标签（和 int switch 同一套 break_labels），
/// 各 case 体之间**没有**贯穿语义 —— 这也和 javac 的 String switch 一致。
fn compile_string_switch(
    selector: &Expr,
    cases: &[SwitchCase],
    em: &mut Emit,
    ctx: &mut Ctx,
    cp: &mut CpBuilder,
) -> Result<bool, CompileError> {
    let sel_slot = ctx.next_slot;
    ctx.next_slot += 1;
    if em.max_locals < ctx.next_slot as u16 {
        em.max_locals = ctx.next_slot as u16;
    }
    compile_expr(selector, em, ctx, cp)?;
    let vt = cp.class("java/lang/String");
    em.set_local_type(sel_slot as usize, vt);
    em.astore(sel_slot)?;

    let mut body_labels = Vec::new();
    for _ in cases {
        body_labels.push(em.new_label());
    }
    let end_label = em.new_label();
    let default_target = cases
        .iter()
        .position(|c| c.labels.iter().any(|l| l.is_none()))
        .map(|i| body_labels[i])
        .unwrap_or(end_label);
    let equals = cp.method_ref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    for (i, c) in cases.iter().enumerate() {
        for label in &c.labels {
            let Some(v) = label else { continue };
            em.aload(sel_slot)?;
            compile_expr(v, em, ctx, cp)?;
            em.op_u2(0xB6, equals); // invokevirtual String.equals
            em.pop(2);
            em.push(1);
            em.branch(0x9A, body_labels[i]); // ifne → 命中
            em.pop(1);
        }
    }
    em.goto(default_target);

    let scope = em.local_types.clone();
    let lbl = ctx.pending_label.take();
    ctx.break_labels.push(JumpTarget { label: lbl, target: end_label });
    for (i, c) in cases.iter().enumerate() {
        em.local_types = scope.clone();
        em.bind(body_labels[i]);
        let mut term = false;
        for s in &c.body {
            if term {
                break;
            }
            term = compile_stmt(s, em, ctx, cp)?;
        }
        if c.arrow && !term {
            em.goto(end_label);
        }
    }
    em.local_types = scope.clone();
    em.bind(end_label);
    ctx.break_labels.pop();
    Ok(false)
}

/// 带标签的跳转：从内向外找第一个同名落点
fn find_target(stack: &[JumpTarget], name: &str) -> Option<usize> {    stack
        .iter()
        .rev()
        .find(|t| t.label.as_deref() == Some(name))
        .map(|t| t.target)
}

/// `label:` 后面跟的是不是循环（只有循环的标签能当 `continue` 目标）
fn is_loop_stmt(s: &Stmt) -> bool {
    matches!(
        s,
        Stmt::For { .. } | Stmt::While { .. } | Stmt::DoWhile { .. } | Stmt::ForEach { .. } | Stmt::Labeled { .. }
    )
}

/// `for (T x : iter)` 降糖。
///
/// **数组**（`T[]`）→
/// ```text
/// T[] $a = iter;  for (int $i = 0; $i < $a.length; $i++) { T x = $a[$i]; body }
/// ```
/// 下标循环用 `for` 而不是 `while`，`continue` 才会落到 `$i++` 上。
///
/// **其它**（Iterable 契约）→
/// ```text
/// java.util.Iterator $it = iter.iterator();
/// while ($it.hasNext()) { T x = (T) $it.next(); body }
/// ```
/// 这两段用的都是已有的语句形态，数组读写/强制转换/跳转全走原来那套已验证的路径。
fn compile_for_each(
    ty: &Type,
    name: &str,
    iter: &Expr,
    body: &Stmt,
    em: &mut Emit,
    ctx: &mut Ctx,
    cp: &mut CpBuilder,
) -> Result<bool, CompileError> {
    let seq = ctx.next_tmp();
    // 被迭代的表达式只求值一次（`for (int v : getArr())` 不能每轮调一次）
    let arr_ty = match expr_type(iter, ctx) {
        Some(t @ Type::Array(_)) => Some(t),
        _ => None,
    };
    match arr_ty {
        Some(arr_ty) => {
            let a_name = format!("$fe{seq}a");
            let i_name = format!("$fe{seq}i");
            let arr = Expr::Ident(a_name.clone());
            let idx = Expr::Ident(i_name.clone());
            let init = vec![
                Stmt::LocalVar { ty: arr_ty, name: a_name.clone(), init: Some(iter.clone()) },
                Stmt::LocalVar {
                    ty: Type::Prim("int"),
                    name: i_name.clone(),
                    init: Some(Expr::IntLit("0".into())),
                },
            ];
            let cond = Expr::Binary {
                op: BinOp::Lt,
                lhs: Box::new(idx.clone()),
                rhs: Box::new(Expr::Field(Box::new(arr.clone()), "length".into())),
            };
            let step = Expr::Assign {
                op: None,
                lhs: Box::new(idx.clone()),
                rhs: Box::new(Expr::Binary {
                    op: BinOp::Add,
                    lhs: Box::new(idx.clone()),
                    rhs: Box::new(Expr::IntLit("1".into())),
                }),
            };
            let elem = Stmt::LocalVar {
                ty: ty.clone(),
                name: name.to_string(),
                init: Some(Expr::ArrayAccess { array: Box::new(arr), index: Box::new(idx) }),
            };
            let for_stmt = Stmt::For {
                init,
                cond: Some(cond),
                update: vec![step],
                body: Box::new(Stmt::Block(vec![elem, body.clone()])),
            };
            return compile_stmt(&for_stmt, em, ctx, cp);
        }
        None => {}
    }
    // 迭代器协议：`iterator()` 的返回类型从接收者的类元数据里查
    let it_name = format!("$fe{seq}it");
    let Some(iter_desc) = call_desc(Some(iter), "iterator", &[], ctx) else {
        return err(format!(
            "for-each 的被迭代表达式不是数组，也找不到 `iterator()` 方法（元素类型 `{ty:?}`）"
        ));
    };
    let ret = iter_desc.rsplit(')').next().unwrap_or("").to_string();
    let it_ty = type_from_desc(&ret);
    let iter_call = Expr::Call {
        recv: Some(Box::new(iter.clone())),
        name: "iterator".into(),
        args: Vec::new(),
        type_args: Vec::new(),
    };
    let next_call = Expr::Call {
        recv: Some(Box::new(Expr::Ident(it_name.clone()))),
        name: "next".into(),
        args: Vec::new(),
        type_args: Vec::new(),
    };
    // `next()` 的静态类型是 Object（raw Iterator）：元素类型不是 Object 时要插 checkcast
    let want_cast = kind_of(ty) == K_REF
        && !matches!(ty, Type::Named { path, .. } if path.last().map(|s| s.as_str()) == Some("Object"));
    let elem_init = if want_cast {
        Expr::Cast { ty: ty.clone(), expr: Box::new(next_call) }
    } else {
        next_call
    };
    let has_next = Expr::Call {
        recv: Some(Box::new(Expr::Ident(it_name.clone()))),
        name: "hasNext".into(),
        args: Vec::new(),
        type_args: Vec::new(),
    };
    let elem = Stmt::LocalVar { ty: ty.clone(), name: name.to_string(), init: Some(elem_init) };
    let block = Stmt::Block(vec![
        Stmt::LocalVar { ty: it_ty, name: it_name, init: Some(iter_call) },
        Stmt::While { cond: has_next, body: Box::new(Stmt::Block(vec![elem, body.clone()])) },
    ]);
    compile_stmt(&block, em, ctx, cp)
}
