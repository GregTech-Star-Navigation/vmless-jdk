//! 单个方法装配：返回指令、临时槽分配、返回值临时槽。

use crate::ast::*;

use super::constpool::CpBuilder;
use super::ctx::Ctx;
use super::emit::{Emit, SCRATCH_SLOT};
use super::error::*;
use super::expr::compile_expr;
use super::stmt::compile_stmt;
use super::types::*;


/// 发一条 xreturn（按值的种类）
pub(crate) fn emit_xreturn(em: &mut Emit, kind: u8) {
    match kind {
        K_LONG => {
            em.op(0xAD); // lreturn
            em.pop(2);
        }
        K_DOUBLE => {
            em.op(0xAF); // dreturn
            em.pop(2);
        }
        K_FLOAT => {
            em.op(0xAE); // freturn（占 1 槽，但验证类型是 float）
            em.pop(1);
        }
        K_REF => {
            em.op(0xB0); // areturn
            em.pop(1);
        }
        _ => {
            em.op(0xAC); // ireturn
            em.pop(1);
        }
    }
}

/// 补一条「不可达」的返回：控制流不允许掉出方法末尾，而不可达块也得有终结指令。
pub(crate) fn emit_zero_return(em: &mut Emit, ret_ty: Option<Type>) {
    match ret_ty {
        None | Some(Type::Prim("void")) => em.op(0xB1),
        Some(t) => {
            let kind = kind_of(&t);
            match kind {
                K_LONG => em.op(0x09),   // lconst_0
                K_DOUBLE => em.op(0x0E), // dconst_0
                K_REF => em.op(0x01),    // aconst_null
                _ => em.op(0x03),        // iconst_0
            }
            emit_xreturn(em, kind);
        }
    }
}

/// 分配一个临时槽（只推进槽位分配，**不**登记验证类型 —— 登记要在初始化表达式编译完之后）。
pub(crate) fn bump_slot(ctx: &mut Ctx, em: &mut Emit, wide: bool) -> Result<u8, CompileError> {
    let slot = ctx.next_slot;
    if slot >= SCRATCH_SLOT {
        return err(format!(
            "局部变量太多：槽位已用到 {slot}，最高可用 {}（{} 号槽留给比较结果）",
            SCRATCH_SLOT - 1,
            SCRATCH_SLOT
        ));
    }
    ctx.next_slot += if wide { 2 } else { 1 };
    if em.max_locals < ctx.next_slot as u16 {
        em.max_locals = ctx.next_slot as u16;
    }
    Ok(slot)
}

/// `return e;` 该用哪种 `xreturn`。
///
/// 不能只用 `expr_kind(e)`：自动装箱/拆箱之后栈上的种类变了 ——
/// `int f() { return box; }` 要 `ireturn`（先 intValue），
/// `Integer f() { return 5; }` 要 `areturn`（先 valueOf）。
pub(crate) fn ret_kind(x: &Expr, ctx: &Ctx) -> u8 {
    let rk = ctx.ret_ty.as_ref().map(kind_of);
    if let Some(k) = rk {
        if k != K_REF {
            return k;
        }
        // 目标引用、值是基本类型 → 装箱，栈上是引用
        if matches!(expr_type(x, ctx), Some(t) if kind_of(&t) != K_REF) {
            return K_REF;
        }
    }
    expr_kind(x, ctx)
}

/// 编 `return e;`：升宽/checkcast/装箱/拆箱都做完，返回 xreturn 用的种类。
fn ret_kind_for(
    x: &Expr,
    ctx: &mut Ctx,
    em: &mut Emit,
    cp: &mut CpBuilder,
) -> Result<u8, CompileError> {
    let kind = ret_kind(x, ctx);
    if kind != K_REF && ctx.ret_ty.as_ref().map(|t| kind_of(t) != K_REF).unwrap_or(false) {
        // 数值返回：先拆箱（若是包装类）再按目标类型拓宽/收窄
        let kk = emit_num(x, em, ctx, cp)?;
        coerce(kk, kind, em)?;
        return Ok(kind);
    }
    compile_expr(x, em, ctx, cp)?;
    // 泛型擦除后 `return box.get();` 的栈上是 Object，方法签名要 String：补 checkcast
    if let Some(t) = ctx.ret_ty.clone() {
        coerce_to(&t, x, ctx, em, cp)?;
    }
    Ok(kind)
}

/// `return e;`
///
/// 有外层 finally 时不能直接把值留在操作数栈上去跑 finally —— 本项目的栈图一律声明
/// 「栈为 0」，finally 体里只要有一个跳转点就会和「栈上压着值」冲突。所以按 javac 的
/// 做法：先把值存进临时槽，跑完 finally 副本再取回来返回。
pub(crate) fn compile_return(
    e: &Option<Expr>,
    em: &mut Emit,
    ctx: &mut Ctx,
    cp: &mut CpBuilder,
) -> Result<bool, CompileError> {
    if ctx.finally_stack.is_empty() {
        match e {
            None => em.op(0xB1),
            Some(x) => {
                // 方法声明是 void（`Runnable` 的 lambda 体就是这种）：表达式算完
                // **丢掉值**再发 `return`。
                //
                // 少了这条会按表达式的种类发 `ireturn`/`areturn`，而合成方法的描述符
                // 是 `()V` —— 真 JVM 与我们的 VM 都报
                // `VerifyError: Operand stack underflow … ireturn`。
                if ctx.ret_ty.as_ref().map(|t| matches!(t, Type::Prim("void"))).unwrap_or(true) {
                    let k = expr_kind(x, ctx);
                    compile_expr(x, em, ctx, cp)?;
                    if kind_wide(k) {
                        em.op(0x58); // pop2
                        em.pop(2);
                    } else {
                        em.op(0x57); // pop
                        em.pop(1);
                    }
                    em.op(0xB1); // return
                    return Ok(true);
                }
                let kind = ret_kind_for(x, ctx, em, cp)?;
                emit_xreturn(em, kind);
            }
        }
        return Ok(true);
    }
    let mut tmp: Option<(u8, u8, Vec<u16>)> = None; // (槽, 种类, 该槽原来的验证类型)
    if let Some(x) = e {
        let wide = kind_wide(ret_kind(x, ctx));
        let slot = bump_slot(ctx, em, wide)?;
        // 临时槽只在「这条 return」上存在，用完必须把验证类型退回去：
        // 否则后续帧会声明一个别的路径上根本没初始化过的槽（VerifyError）。
        let prev: Vec<u16> = (0..if wide { 2 } else { 1 })
            .map(|i| *em.local_types.get(slot as usize + i).unwrap_or(&u16::MAX))
            .collect();
        let kind = ret_kind_for(x, ctx, em, cp)?;
        let vt = match ctx.ret_ty.clone() {
            Some(t) => ver_type(cp, &t),
            None => 0,
        };
        em.set_local_type(slot as usize, vt);
        if wide {
            em.set_local_type(slot as usize + 1, u16::MAX);
        }
        em.store_slot(slot, kind)?;
        tmp = Some((slot, kind, prev));
    }
    // 内层 finally 先跑；编译第 i 份时把 pending 换成它外层的那些（否则 return
    // 会把自己那份 finally 再展开一次，无限递归）。
    let pending = ctx.finally_stack.clone();
    for i in (0..pending.len()).rev() {
        let mut outer: Vec<Stmt> = pending[..i].to_vec();
        std::mem::swap(&mut ctx.finally_stack, &mut outer);
        let terminated = compile_stmt(&pending[i], em, ctx, cp)?;
        std::mem::swap(&mut ctx.finally_stack, &mut outer);
        if terminated {
            return Ok(true); // finally 自己就终结了，这个 return 不可达
        }
    }
    match tmp {
        Some((slot, kind, prev)) => {
            em.load_slot(slot, kind)?;
            emit_xreturn(em, kind);
            // 这条路径到 return 就结束了：临时槽的验证类型退回原样，
            // 免得后续帧声明一个「别的路径上没初始化」的槽。
            for (i, v) in prev.iter().enumerate() {
                em.set_local_type(slot as usize + i, *v);
            }
        }
        None => em.op(0xB1),
    }
    Ok(true)
}
