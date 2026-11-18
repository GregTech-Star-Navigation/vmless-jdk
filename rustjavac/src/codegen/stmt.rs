//! 语句编译：控制流、try/catch/finally、switch、break/continue 分支。

use crate::ast::*;

use super::constpool::CpBuilder;
use super::ctx::{Ctx, JumpTarget};
use super::emit::{Emit, SCRATCH_SLOT};
use super::error::*;
use super::expr::*;
use super::method::*;
use super::types::*;

/// `switch` 的发射形态：比较链 / 跳转表 / 查表
enum SwitchKind {
    Table { low: i32, high: i32 },
    Lookup { values: Vec<i32> },
}

/// 分支数达到这个量才值得用跳转表（小 switch 用比较链更省字节、也不用对齐 padding）
const SWITCH_TABLE_MIN_CASES: usize = 6;

/// 取出 `case N:` 里的 int 字面量（其它形态的标签 —— 常量名、枚举 ordinal —— 返回 None）
fn case_lit(e: &Expr) -> Option<i32> {
    match e {
        Expr::IntLit(s) => {
            let t = s.trim_end_matches(['l', 'L']);
            t.parse::<i32>().ok()
        }
        Expr::Unary { op: UnOp::Neg, expr } => match &**expr {
            Expr::IntLit(s) => s.trim_end_matches(['l', 'L']).parse::<i32>().ok().map(|v| -v),
            _ => None,
        },
        _ => None,
    }
}

/// 所有标签都是 int 字面量、且分支数够时，选跳转表还是查表。
///
/// 判据：case 值越密集越适合 tableswitch（表大小 = high-low+1）；
/// 稀疏到「表里一半以上是空槽」就换 lookupswitch（按 key 二分）。
fn pick_switch_kind(cases: &[SwitchCase]) -> Option<SwitchKind> {
    let mut values: Vec<i32> = Vec::new();
    for c in cases {
        for l in &c.labels {
            let Some(e) = l else { continue }; // default 不参与
            values.push(case_lit(e)?); // 有任何一个标签不是字面量 → 退回比较链
        }
    }
    values.sort_unstable();
    values.dedup();
    if values.len() < SWITCH_TABLE_MIN_CASES {
        return None;
    }
    let low = *values.first()?;
    let high = *values.last()?;
    let span = high as i64 - low as i64 + 1;
    // 空槽不超过一半就用表（表项 4 字节，查表是 O(1)）
    if span <= (values.len() as i64) * 2 {
        Some(SwitchKind::Table { low, high })
    } else {
        Some(SwitchKind::Lookup { values })
    }
}

/// 某个 case 值属于哪个 case 体（`case 1: case 2:` 共用体时取同一个）
fn case_of_value(cases: &[SwitchCase], v: i32) -> Option<usize> {
    cases.iter().position(|c| {
        c.labels.iter().any(|l| match l {
            Some(e) => case_lit(e) == Some(v),
            None => false,
        })
    })
}

/// 发射各 case 体 + end 标签（跳转表与比较链共用的收尾）
fn finish_switch(
    cases: &[SwitchCase],
    body_labels: &[usize],
    end_label: usize,
    em: &mut Emit,
    ctx: &mut Ctx,
    cp: &mut CpBuilder,
) -> Result<bool, CompileError> {
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
        // 语句位置的 `i++` / `--n` / `s += x` 的值没人要 —— 直接改写成等价的赋值语句，
        // 赋值语句那三条 arm（数组元素 / 字段 / 局部变量）已经把「接收者只求值一次」做对了。
        // 前提是 lvalue 的接收者/下标可重算（`a[i++]++` 这种要算两次，会被 `is_pure` 挡住）。
        Stmt::Expr(Expr::Unary { op, expr })
            if matches!(op, UnOp::PreInc | UnOp::PreDec | UnOp::PostInc | UnOp::PostDec) =>
        {
            let asg = inc_dec_to_assign(*op, expr)?;
            compile_stmt(&Stmt::Expr(asg), em, ctx, cp)
        }
        Stmt::Expr(Expr::Assign { op: Some(op), lhs, rhs }) => {
            let bin = compound_to_assign(*op, lhs, rhs)?;
            compile_stmt(&Stmt::Expr(bin), em, ctx, cp)
        }
        // `;` / `assert ...`（开关关掉时）/ 块内局部类声明 —— 都降级成空语句
        Stmt::Empty => Ok(false),
        Stmt::Return(None) => compile_return(&None, em, ctx, cp),
        // `return P && Q && R;`（链上有 instanceof 模式测试）→
        // `if (P) { if (Q) { return R; } return false; } return false;`
        Stmt::Return(Some(e)) if is_pattern_and_chain(e) => {
            let mut parts: Vec<&Expr> = Vec::new();
            flatten_and(e, &mut parts);
            let chain = rewrite_and_chain_return(&parts);
            compile_stmt(&chain, em, ctx, cp)
        }
        // `return P ? a : b;`（P 里带模式绑定）→ `if (P) return a; return b;`
        Stmt::Return(Some(Expr::Cond { cond, then: t, els: e }))
            if pattern_cond(cond).is_some() || is_pattern_and_chain(cond) =>
        {
            let if_stmt = Stmt::If {
                cond: (**cond).clone(),
                then: Box::new(Stmt::Return(Some((**t).clone()))),
                els: Some(Box::new(Stmt::Return(Some((**e).clone())))),
            };
            compile_stmt(&if_stmt, em, ctx, cp)
        }
        // `return switch (...) { … };`：表达式层没有 `&mut Ctx` 分配不了槽位，
        // 先把 switch 的值落到临时局部，再按普通 `return 名字` 走
        Stmt::Return(Some(Expr::SwitchExpr(sw))) => {
            let (decl, id) = hoist_switch_expr(sw, ctx);
            compile_stmt(
                &Stmt::Block(vec![decl, Stmt::Return(Some(id))]),
                em,
                ctx,
                cp,
            )
        }
        Stmt::Return(Some(e)) => {
            // return 位置的 lambda：目标类型是方法的返回类型
            let t = ctx.ret_ty.as_ref().and_then(|t| lambda_target_of_ty(t, ctx));
            let saved = ctx.lambda_target.replace(t);
            let r = compile_return(&Some(e.clone()), em, ctx, cp);
            ctx.lambda_target.replace(saved);
            r
        }
        // `f = switch (...) { … };` —— 同上
        Stmt::Expr(Expr::Assign { op: None, lhs, rhs })
            if matches!(**rhs, Expr::SwitchExpr(_)) =>
        {
            let Expr::SwitchExpr(sw) = &**rhs else { unreachable!() };
            let (decl, id) = hoist_switch_expr(sw, ctx);
            let assign = Expr::Assign { op: None, lhs: lhs.clone(), rhs: Box::new(id) };
            compile_stmt(
                &Stmt::Block(vec![decl, Stmt::Expr(assign)]),
                em,
                ctx,
                cp,
            )
        }
        Stmt::Expr(e) if is_println_call(e) => {
            compile_expr_mut(e, em, ctx, cp)?;
            Ok(false)
        }
        Stmt::Expr(e) if !matches!(e, Expr::Assign { .. }) => {
            let kind = expr_kind(e, ctx);
            compile_expr_mut(e, em, ctx, cp)?;
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
        // `T v = P ? a : b;`（P 里带模式绑定）→ 先声明，再在两条分支里各自赋值
        Stmt::LocalVar { ty, name, init } if init.as_ref().is_some_and(is_pattern_cond_expr) => {
            let Some(Expr::Cond { cond, then: t, els: e }) = init.as_ref() else { unreachable!() };
            let decl = Stmt::LocalVar { ty: ty.clone(), name: name.clone(), init: None };
            let path = |v: &Expr| {
                Stmt::Expr(Expr::Assign {
                    op: None,
                    lhs: Box::new(Expr::Ident(name.clone())),
                    rhs: Box::new(v.clone()),
                })
            };
            let if_stmt = Stmt::If {
                cond: (**cond).clone(),
                then: Box::new(path(t)),
                els: Some(Box::new(path(e))),
            };
            compile_stmt(&Stmt::Block(vec![decl, if_stmt]), em, ctx, cp)
        }
        // `v = P ? a : b;` —— 同上（赋值形态）
        Stmt::Expr(Expr::Assign { op: None, lhs, rhs }) if is_pattern_cond_expr(rhs) => {
            let Expr::Cond { cond, then: t, els: e } = &**rhs else { unreachable!() };
            let path = |v: &Expr| {
                Stmt::Expr(Expr::Assign { op: None, lhs: lhs.clone(), rhs: Box::new(v.clone()) })
            };
            let if_stmt = Stmt::If {
                cond: (**cond).clone(),
                then: Box::new(path(t)),
                els: Some(Box::new(path(e))),
            };
            compile_stmt(&if_stmt, em, ctx, cp)
        }
        Stmt::LocalVar { ty, name, init } => {
            // `var x = init;`（JLS 10.9）：局部变量的类型由初值推导。不推导的话这个
            // 局部**没有静态类型** —— 后面 `x.m()` / `x.f` 全报「未找到实例方法」，
            // 而且 `kind_of(Type::Var)` 是 K_INT，引用值被塞进整数槽（aload 变 iload）。
            let inferred = if matches!(*ty, Type::Var) {
                init.as_ref().and_then(|i| expr_type(i, ctx))
            } else {
                None
            };
            let ty = inferred.as_ref().unwrap_or(ty);
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
            // `T[] a = {…}`：元素类型来自声明（裸 `{…}` 自己没有类型信息）
            let init = match (ty, init) {
                (Type::Array(_), Some(Expr::ArrayInit(items))) => Some(Expr::Cast {
                    ty: ty.clone(),
                    expr: Box::new(Expr::ArrayInit(items.clone())),
                }),
                _ => init.clone(),
            };
            if let Some(e) = &init {
                // 目标类型：`Runnable r = () -> { … };` —— lambda 没有自己的类型，
                // 只能从声明的左侧推
                let saved = ctx.lambda_target.replace(crate::codegen::expr::lambda_target_of_ty(ty, ctx));
                let r = compile_expr_mut(e, em, ctx, cp);
                ctx.lambda_target.replace(saved);
                r?;
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
                compile_expr_mut(rhs, em, ctx, cp)?;
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
                compile_expr_mut(&recv, em, ctx, cp)?;
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
                    compile_expr_mut(rhs, em, ctx, cp)?;
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
                compile_expr_mut(rhs, em, ctx, cp)?;
                coerce_to(&type_from_desc(&desc), rhs, ctx, em, cp)?;
                let f = cp.field_ref(&owner, &name, &desc);
                em.op_u2(0xB5, f); // putfield
                em.pop(if kind_wide(kind_from_desc(&desc)) { 3 } else { 2 });
                return Ok(false);
            };
            let kind = *ctx.local_kind.get(&name).unwrap_or(&K_INT);
            compile_expr_mut(rhs, em, ctx, cp)?;
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
            compile_expr_mut(&pin(selector, ctx, false), em, ctx, cp)?;
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

            // 分支多且标签都是 int 字面量时发 tableswitch / lookupswitch：
            // 比较链是 O(case 数)，跳转表是一次查表。小 switch 仍然走比较链
            // （表本身的字节数 + 对齐 padding 反而是浪费）。
            if let Some(kind) = pick_switch_kind(cases) {
                em.iload(sel_slot)?;
                match kind {
                    SwitchKind::Table { low, high } => {
                        let mut targets = Vec::new();
                        for v in low..=high {
                            let idx = case_of_value(cases, v).map(|i| body_labels[i]).unwrap_or(default_target);
                            targets.push(idx);
                        }
                        em.tableswitch(low, high, &targets, default_target);
                    }
                    SwitchKind::Lookup { values } => {
                        let mut pairs = Vec::new();
                        for v in values {
                            let t = case_of_value(cases, v).map(|i| body_labels[i]).unwrap_or(default_target);
                            pairs.push((v, t));
                        }
                        em.lookupswitch(&pairs, default_target);
                    }
                }
                return finish_switch(cases, &body_labels, end_label, &mut *em, ctx, cp);
            }

            for (i, c) in cases.iter().enumerate() {
                for label in &c.labels {
                    match label {
                        Some(v) => {
                            em.iload(sel_slot)?;
                            compile_expr_mut(&pin(v, ctx, true), em, ctx, cp)?;
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
            finish_switch(cases, &body_labels, end_label, em, ctx, cp)
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
            // `if (x instanceof T t)` / `if (!(x instanceof T t))`：模式变量只在
            // 「测试为真」的那条路径上存在，必须在**语句层**做分支（表达式层没有
            // 控制流可用，见 `compile_expr` 的 InstanceOf 分支）
            if let Some(p) = pattern_cond(cond) {
                return compile_if_pattern(p, then, els.as_deref(), em, ctx, cp);
            }
            // `a && P && b`（P 是其中最左边那个 instanceof 模式测试）：模式变量的
            // 可见性只能在语句层展开，摊平成嵌套 if。`&&` 的语义正好等长：
            // 任何一段为假都进 else 分支。
            {
                let mut parts: Vec<&Expr> = Vec::new();
                flatten_and(cond, &mut parts);
                if parts.len() > 1 && parts.iter().any(|p| pattern_cond(p).is_some()) {
                    let nested = rewrite_and_chain(&parts, then, els.as_deref());
                    return compile_stmt(&nested, em, ctx, cp);
                }
            }
            // `if (先决条件 && x instanceof T t)`：模式在 `&&` 右边，同样只能先求值
            // 先决条件、再在「它成立」的分支里测模式
            // 等价于 `if (A) { if (P) {then} else {els} } else {els}`
            if let Expr::Binary { op: BinOp::And, lhs: a, rhs: p } = cond {
                if pattern_cond(p).is_some() && pattern_cond(a).is_none() {
                    let inner = Stmt::If {
                        cond: (**p).clone(),
                        then: then.clone(),
                        els: els.clone(),
                    };
                    let outer = Stmt::If {
                        cond: (**a).clone(),
                        then: Box::new(inner),
                        els: els.clone(),
                    };
                    return compile_stmt(&outer, em, ctx, cp);
                }
            }
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
            compile_expr_mut(e, em, ctx, cp)?;
            em.op(0xBF); // athrow
            em.pop(1);
            Ok(true) // 之后不可达
        }
        // `synchronized (lock) { body }` —— 照 javac 的形态发：
        //
        //   <lock>; astore L          ← 只求值一次，异常路径还要再用
        //   aload L; monitorenter
        //   body                       ← 保护区 [body_start, body_end)
        //   aload L; monitorexit
        //   goto end
        //   handler: astore E; aload L; monitorexit; aload E; athrow
        //   end:
        //
        // 关键是**异常路径上也要 monitorexit**：漏了就是「抛一次异常永久死锁」。
        // 另外 `aload` 之后不能插 null 检查 —— JVMS 规定 null 由 `monitorenter`
        // 自己抛 NPE，提前检查反而改变语义（异常类型/时机都不对）。
        Stmt::Sync { lock, body } => {
            let entry = em.local_types.clone();
            let lk = bump_slot(ctx, em, false)?;
            let ex = bump_slot(ctx, em, false)?;
            let obj_cls = ver_type(cp, &Type::Named { path: vec!["Object".into()], args: vec![] });
            // 锁对象先算出来存好（它自己可能有跳转，必须在栈空时算）
            compile_expr(lock, em, ctx, cp)?;
            em.set_local_type(lk as usize, obj_cls);
            em.store_slot(lk, K_REF)?; // astore
            // handler 的帧：锁槽**已声明**（handler 里要 `aload` 它），
            // 异常槽**还是 Top**（那一刻它还没被赋值 —— 声明成 Object 会让
            // 验证器算出的「当前帧 = top」和栈图打架，报
            // `Inconsistent stackmap frames`）。
            let handler_entry = em.local_types.clone();
            em.load_slot(lk, K_REF)?;
            em.op(0xC2); // monitorenter
            em.pop(1); // monitorenter 弹掉锁对象：不记账的话整个方法后面都多算一槽，
                       // 后续每一处 `&&`/`||` 都会被判「栈非空」（44 个文件是这么挂的）
            let body_start = em.new_label();
            let body_end = em.new_label();
            let handler = em.new_label();
            let end = em.new_label();
            em.place(body_start);
            let terminated = compile_stmt(body, em, ctx, cp)?;
            em.place(body_end);
            if !terminated {
                em.load_slot(lk, K_REF)?;
                em.op(0xC3); // monitorexit
                em.pop(1);
                em.goto(end);
            }
            // 异常路径：接住任何异常 → 解锁 → 原样抛出
            em.bind_handler(handler, handler_entry, cp.class("java/lang/Throwable"));
            em.push(1); // 栈上已经有一个异常对象
            em.set_local_type(ex as usize, cp.class("java/lang/Throwable"));
            em.store_slot(ex, K_REF)?; // astore
            em.load_slot(lk, K_REF)?;
            em.op(0xC3); // monitorexit
            em.pop(1);
            em.load_slot(ex, K_REF)?;
            em.op(0xBF); // athrow
            em.pop(1); // athrow 弹掉异常对象：不记账会让 end 之后的代码从 +1 槽开始算
            em.exc(body_start, body_end, handler, 0);
            // **先**把隐藏槽的声明退回入口快照，**再**绑定汇合标签：
            // 反过来的话（先 bind 再退回）end 那一帧会带着 handler 里刚声明的
            // 异常槽类型，而数据流上那个槽根本没赋值 —— 验证器报
            // `Inconsistent stackmap frames`。
            em.local_types = entry;
            em.bind(end);
            Ok(false)
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
    compile_expr_mut(selector, em, ctx, cp)?;
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
            compile_expr_mut(v, em, ctx, cp)?;
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

/// 这个表达式是不是「条件里带 instanceof 模式绑定」的三元式
fn is_pattern_cond_expr(e: &Expr) -> bool {
    matches!(e, Expr::Cond { cond, .. } if pattern_cond(cond).is_some() || is_pattern_and_chain(cond))
}

/// 把 `P && rest`（P 是 instanceof 模式测试）拆成 (P, rest)。
///
/// 模式变量只活在「测试为真」那条路径上，所以这种条件必须**先测再绑**，
/// 不能在表达式层按 `&&` 的短路语义直接顺序求值。
fn split_pattern_and(e: &Expr) -> Option<(&Expr, &Expr)> {
    match e {
        Expr::Binary { op: BinOp::And, lhs, rhs } if pattern_cond(lhs).is_some() => {
            Some((lhs.as_ref(), rhs.as_ref()))
        }
        _ => None,
    }
}

/// `A && B && C` 摊平成 `[A, B, C]`（左结合链）
fn flatten_and<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
    match e {
        Expr::Binary { op: BinOp::And, lhs, rhs } => {
            flatten_and(lhs, out);
            flatten_and(rhs, out);
        }
        _ => out.push(e),
    }
}

/// `cond1 && cond2 && …` 里**任意一段**带模式绑定时，把条件链展开成嵌套 if。
///
/// 模式变量的可见性由「测试为真」决定，所以只能在语句层一段一段测：
/// ```text
/// if (a && P && b) { T } else { E }
/// ≡ if (a) { if (P) { if (b) { T } else { E } } else { E } } else { E }
/// ```
fn rewrite_and_chain(parts: &[&Expr], then: &Stmt, els: Option<&Stmt>) -> Stmt {
    match parts.split_first() {
        None => then.clone(),
        Some((head, rest)) => {
            let inner = rewrite_and_chain(rest, then, els);
            Stmt::If {
                cond: (*head).clone(),
                then: Box::new(inner),
                els: els.map(|e| Box::new(e.clone())),
            }
        }
    }
}

/// `if (x instanceof T t) …` 条件里的 instanceof 模式绑定
enum PatCond<'a> {
    /// `if (x instanceof T t)`
    Pos(&'a Expr, &'a Type, &'a str),
    /// `if (!(x instanceof T t))`
    Neg(&'a Expr, &'a Type, &'a str),
}

fn pattern_cond(e: &Expr) -> Option<PatCond<'_>> {
    match e {
        Expr::InstanceOf { expr, ty, binding: Some(n) } => Some(PatCond::Pos(expr, ty, n)),
        Expr::Unary { op: UnOp::Not, expr } => match &**expr {
            Expr::InstanceOf { expr, ty, binding: Some(n) } => Some(PatCond::Neg(expr, ty, n)),
            _ => None,
        },
        _ => None,
    }
}

/// 发 `if (x instanceof T t) …`：值先存进脏槽（跳转点必须落在空栈上），
/// 再在「测试为真」的那条路径上 checkcast + astore 到模式变量。
///
/// ```text
///   <x> ; astore tmp ; aload tmp ; instanceof T
///   ifeq  L_else         // Neg 时是 ifne：是 T 就直接去 L_else
///   aload tmp ; checkcast T ; astore t      // Pos 的绑定
///   <then>                                   // Pos 时 t 可见
///   goto  L_end
/// L_else:
///   aload tmp ; checkcast T ; astore t      // Neg 的绑定
///   <els>                                    // Neg 时 t 可见
/// L_end:
/// ```
///
/// Neg 且**没有 else** 时（`if (!(x instanceof T t)) return;` 这种守卫写法），
/// `then` 一定终结，落进 L_else 就等于「测试为真」，所以 `t` 在 if 之后继续可见 ——
/// 这是 JLS 的 flow scoping 在 java.base 里最常见的用法。
fn compile_if_pattern(
    p: PatCond<'_>,
    then: &Stmt,
    els: Option<&Stmt>,
    em: &mut Emit,
    ctx: &mut Ctx,
    cp: &mut CpBuilder,
) -> Result<bool, CompileError> {
    let (expr, ty, name, negated) = match p {
        PatCond::Pos(e, t, n) => (e, t, n, false),
        PatCond::Neg(e, t, n) => (e, t, n, true),
    };
    let tmp = em.alloc_scratch()?;
    let saved_tmp_ty = em.local_type_of(tmp);
    compile_expr_mut(expr, em, ctx, cp)?;
    em.astore(tmp)?;
    let tmp_vt = match expr_type(expr, ctx) {
        Some(t) if !matches!(t, Type::Prim(_)) => ver_type(cp, &t),
        _ => cp.class("java/lang/Object"),
    };
    em.set_local_type(tmp as usize, tmp_vt);
    em.aload(tmp)?;
    let cls = ver_type(cp, ty);
    em.op_u2(0xC1, cls); // instanceof
    em.pop(1);
    em.push(1);

    let slot = ctx.next_slot;
    if slot >= SCRATCH_SLOT {
        return err(format!("局部变量太多：槽位已用到 {slot}（上限 {SCRATCH_SLOT}）"));
    }
    ctx.next_slot += 1;
    if em.max_locals < ctx.next_slot as u16 {
        em.max_locals = ctx.next_slot as u16;
    }
    let pat_vt = ver_type(cp, ty);
    let scope = em.local_types.clone();
    let l_else = em.new_label();
    let l_end = em.new_label();
    em.branch(if negated { 0x9A } else { 0x99 }, l_else); // ifne / ifeq
    em.pop(1);

    // 把模式变量登记进作用域并发射 checkcast + astore
    fn bind(
        name: &str,
        ty: &Type,
        slot: u8,
        pat_vt: u16,
        em: &mut Emit,
        ctx: &mut Ctx,
        cp: &mut CpBuilder,
        tmp: u8,
        cls: u16,
    ) -> Result<(), CompileError> {
        ctx.locals.insert(name.to_string(), slot);
        ctx.local_kind.insert(name.to_string(), K_REF);
        ctx.local_ty.insert(name.to_string(), ty.clone());
        em.aload(tmp)?;
        em.op_u2(0xC0, cls); // checkcast
        em.astore(slot)?;
        em.set_local_type(slot as usize, pat_vt);
        let _ = cp;
        Ok(())
    }

    // 下游要恢复的旧绑定
    let old_local = ctx.locals.get(name).copied();
    let old_kind = ctx.local_kind.get(name).copied();
    let old_ty = ctx.local_ty.get(name).cloned();

    let mut then_term = false;
    if negated {
        then_term = compile_stmt(then, em, ctx, cp)?; // t 在这里不绑定
    } else {
        bind(name, ty, slot, pat_vt, em, ctx, cp, tmp, cls)?;
        then_term = compile_stmt(then, em, ctx, cp)?;
    }
    em.local_types = scope.clone();
    if !then_term {
        em.goto(l_end);
    }
    em.bind(l_else);
    match els {
        Some(e) => {
            if negated {
                bind(name, ty, slot, pat_vt, em, ctx, cp, tmp, cls)?;
            } else {
                ctx.locals.remove(name);
                ctx.local_kind.remove(name);
                ctx.local_ty.remove(name);
            }
            compile_stmt(e, em, ctx, cp)?;
            em.local_types = scope;
            em.bind(l_end);
        }
        None => {
            if negated && then_term {
                // 落进 L_else 就等于测试为真：t 留到这里之后继续可见
                bind(name, ty, slot, pat_vt, em, ctx, cp, tmp, cls)?;
                em.bind(l_end);
            } else {
                em.local_types = scope;
                em.bind(l_end);
            }
        }
    }

    // 恢复旧绑定（Neg + 无 else + then 终结 是唯一保留的情形）
    let keep = negated && els.is_none() && then_term;
    if !keep {
        match old_local {
            Some(v) => ctx.locals.insert(name.to_string(), v),
            None => ctx.locals.remove(name),
        };
        match old_kind {
            Some(v) => ctx.local_kind.insert(name.to_string(), v),
            None => ctx.local_kind.remove(name),
        };
        match old_ty {
            Some(v) => ctx.local_ty.insert(name.to_string(), v),
            None => ctx.local_ty.remove(name),
        };
    }
    em.free_scratch(tmp);
    em.set_local_type(tmp as usize, saved_tmp_ty);
    Ok(false)
}

// ---------------------------------------------------------------------------
// switch 表达式：整块降成「临时局部 + 语句形态的 switch」
// ---------------------------------------------------------------------------

/// 同 `compile_expr`，但**允许分配局部变量槽**（switch 表达式要用临时局部收值）。
/// 语句层一律走这个函数；表达式内部（手里只有 `&Ctx`）仍走 `compile_expr`。
pub(crate) fn compile_expr_mut(
    e: &Expr,
    em: &mut Emit,
    ctx: &mut Ctx,
    cp: &mut CpBuilder,
) -> Result<(), CompileError> {
    match e {
        Expr::SwitchExpr(sw) => compile_switch_expr(sw, em, ctx, cp),
        _ => compile_expr(e, em, &*ctx, cp),
    }
}

/// `switch` 表达式的零值（临时变量的初值）。
///
/// Java 要求 switch 表达式穷尽，正常代码里这个初值读不到；给它一个确定的值是为了
/// **栈图成立** —— 声明了类型的槽位在没赋值的路径上也得有合法值。
fn default_value(ty: &Type) -> Expr {
    match ty {
        Type::Prim("long") => Expr::IntLit("0L".into()),
        Type::Prim("float") => Expr::FloatLit("0.0f".into()),
        Type::Prim("double") => Expr::FloatLit("0.0".into()),
        Type::Prim("boolean") => Expr::BoolLit(false),
        Type::Prim("char") => Expr::CharLit("a".into()),
        Type::Prim(_) => Expr::IntLit("0".into()),
        _ => Expr::Null,
    }
}

/// 一条 `yield e;` / 箭头体的裸表达式 → 「写临时变量 + 跳出 switch」
fn yield_store(tgt: &Expr, label: &str, e: &Expr) -> Stmt {
    Stmt::Block(vec![
        Stmt::Expr(Expr::Assign { op: None, lhs: Box::new(tgt.clone()), rhs: Box::new(e.clone()) }),
        Stmt::Break(Some(label.to_string())),
    ])
}

/// 把语句树里的 `yield e;` 改写成「写临时变量 + 跳出 switch」。
///
/// 递归进去，但**不**进 `Expr` —— 嵌套的 switch 表达式是 `Expr::SwitchExpr`，
/// 它有自己的临时变量和标签。
fn rewrite_yields(s: &Stmt, tgt: &Expr, label: &str) -> Stmt {
    match s {
        Stmt::Yield(e) => yield_store(tgt, label, e),
        Stmt::Block(v) => Stmt::Block(v.iter().map(|x| rewrite_yields(x, tgt, label)).collect()),
        Stmt::If { cond, then, els } => Stmt::If {
            cond: cond.clone(),
            then: Box::new(rewrite_yields(then, tgt, label)),
            els: els.as_ref().map(|e| Box::new(rewrite_yields(e, tgt, label))),
        },
        Stmt::While { cond, body } => {
            Stmt::While { cond: cond.clone(), body: Box::new(rewrite_yields(body, tgt, label)) }
        }
        Stmt::DoWhile { body, cond } => {
            Stmt::DoWhile { body: Box::new(rewrite_yields(body, tgt, label)), cond: cond.clone() }
        }
        Stmt::For { init, cond, update, body } => Stmt::For {
            init: init.iter().map(|x| rewrite_yields(x, tgt, label)).collect(),
            cond: cond.clone(),
            update: update.clone(),
            body: Box::new(rewrite_yields(body, tgt, label)),
        },
        Stmt::ForEach { ty, name, iter, body } => Stmt::ForEach {
            ty: ty.clone(),
            name: name.clone(),
            iter: iter.clone(),
            body: Box::new(rewrite_yields(body, tgt, label)),
        },
        Stmt::Labeled { label: l, body } => {
            Stmt::Labeled { label: l.clone(), body: Box::new(rewrite_yields(body, tgt, label)) }
        }
        Stmt::Switch { selector, cases } => Stmt::Switch {
            selector: selector.clone(),
            cases: cases
                .iter()
                .map(|c| SwitchCase {
                    labels: c.labels.clone(),
                    body: c.body.iter().map(|x| rewrite_yields(x, tgt, label)).collect(),
                    arrow: c.arrow,
                })
                .collect(),
        },
        Stmt::Try { body, catches, finally } => Stmt::Try {
            body: Box::new(rewrite_yields(body, tgt, label)),
            catches: catches
                .iter()
                .map(|c| Catch {
                    types: c.types.clone(),
                    name: c.name.clone(),
                    body: rewrite_yields(&c.body, tgt, label),
                })
                .collect(),
            finally: finally.as_ref().map(|f| Box::new(rewrite_yields(f, tgt, label))),
        },
        other => other.clone(),
    }
}

/// `switch (...) { … }` 表达式 → 临时局部变量 + 语句形态的 switch
fn compile_switch_expr(
    sw: &Stmt,
    em: &mut Emit,
    ctx: &mut Ctx,
    cp: &mut CpBuilder,
) -> Result<(), CompileError> {
    let Stmt::Switch { selector, cases } = sw else {
        // parser 拿这个变体当「块体 lambda」的占位
        return err("暂不支持的表达式：块体 lambda（`x -> { … }`）");
    };
    let ty = switch_expr_type(sw, ctx)
        .unwrap_or(Type::Named { path: vec!["Object".into()], args: vec![] });
    let seq = ctx.next_tmp();
    let name = format!("$sw{seq}");
    let label = format!("$swx{seq}");
    let tgt = Expr::Ident(name.clone());
    let new_cases: Vec<SwitchCase> = cases
        .iter()
        .map(|c| {
            // 箭头体里直接写值（`case A -> v;`）—— 语句位置上是 Stmt::Expr
            let value_body = c.arrow && c.body.len() == 1 && matches!(c.body[0], Stmt::Expr(_));
            let body = if value_body {
                match &c.body[0] {
                    Stmt::Expr(e) => vec![yield_store(&tgt, &label, e)],
                    _ => vec![],
                }
            } else {
                c.body.iter().map(|s| rewrite_yields(s, &tgt, &label)).collect()
            };
            SwitchCase { labels: c.labels.clone(), body, arrow: c.arrow }
        })
        .collect();
    // 用标签包住 switch：`yield` 的 break 只跳这一层（不误伤体里的循环）
    let block = Stmt::Block(vec![
        Stmt::LocalVar { ty: ty.clone(), name: name.clone(), init: Some(default_value(&ty)) },
        Stmt::Labeled {
            label: label.clone(),
            body: Box::new(Stmt::Switch { selector: selector.clone(), cases: new_cases }),
        },
    ]);
    compile_stmt(&block, em, ctx, cp)?;
    let Some(slot) = ctx.locals.get(&name).copied() else {
        return err("switch 表达式内部错误：临时变量没有分到槽位");
    };
    let kind = ctx.local_kind.get(&name).copied().unwrap_or(K_REF);
    em.load_slot(slot, kind)?;
    Ok(())
}

/// `switch (...) { … }` 的值落到一个新的临时局部：返回 (声明, 引用它的名字表达式)
fn hoist_switch_expr(sw: &Stmt, ctx: &mut Ctx) -> (Stmt, Expr) {
    let seq = ctx.next_tmp();
    let name = format!("$swv{seq}");
    let ty = switch_expr_type(sw, ctx)
        .unwrap_or(Type::Named { path: vec!["Object".into()], args: vec![] });
    let decl = Stmt::LocalVar {
        ty,
        name: name.clone(),
        init: Some(Expr::SwitchExpr(Box::new(sw.clone()))),
    };
    (decl, Expr::Ident(name))
}

/// 这个表达式是不是「`&&` 链且链上有 instanceof 模式测试」
fn is_pattern_and_chain(e: &Expr) -> bool {
    if !matches!(e, Expr::Binary { op: BinOp::And, .. }) {
        return false;
    }
    let mut parts: Vec<&Expr> = Vec::new();
    flatten_and(e, &mut parts);
    parts.iter().any(|p| pattern_cond(p).is_some())
}

/// `return A && B && C;` → 嵌套 if，最后一段是真正的返回值
fn rewrite_and_chain_return(parts: &[&Expr]) -> Stmt {
    match parts.split_first() {
        None => Stmt::Return(Some(Expr::BoolLit(true))),
        Some((last, [])) => Stmt::Return(Some((**last).clone())),
        Some((head, rest)) => Stmt::If {
            cond: (**head).clone(),
            then: Box::new(rewrite_and_chain_return(rest)),
            els: Some(Box::new(Stmt::Return(Some(Expr::BoolLit(false))))),
        },
    }
}
