//! `LambdaMetafactory` 的 JIT 侧落地：编译期「织类 + 蹦床」。
//!
//! # 为什么不发「对 VM indy 解析入口的调用」
//!
//! 那是最省事的路，但在这套运行时里**走不通**：JIT 与解释器各有自己的堆
//! （引用是各自堆内的偏移，见 `vm/bridge.rs` 开头那段），而 lambda 调用点解析出来的
//! 是一个**对象**（功能接口实例），那个偏移跨不过边界。所以这里按 HotSpot 的思路
//! 在**编译期**就把调用点展开掉：
//!
//! ```text
//! invokedynamic #N   // (captured...)Ljava/util/function/IntUnaryOperator;
//!   ↓ 编译期
//!   new <owner>$$lambda$<站点号>      // 合成类，字段 = 捕获变量（顺序一致）
//!   putfield 0..n                     // 捕获变量进字段（引用字段走写屏障）
//!   ↓ 运行期调用 applyAsInt(I)I
//!   vcall(tag, "applyAsInt(I)I") → 蹦床函数（本文件生成）
//!   ↓ 蹦床
//!   读捕获字段 + SAM 实参 → 调真正的实现方法（invokestatic / invokevirtual …）
//! ```
//!
//! 与解释器那套（`vm/indy.rs` 跑引导方法、缓存 `CallSite`、再走 MethodHandle 解释）
//! 相比，这里**没有**运行期解析：`MethodHandle` 在编译期就已经是一对
//! `(类, 方法名, 描述符)`，`CallSite` 就是一条虚方法表条目。所以 lambda 的每次调用
//! 只多一次间接跳转（与普通虚调用同价），不会「整方法回退解释器」。
//!
//! 调用点身份（`class + 常量池下标`）由 `jit::indy` 统一分配，**跨编译单元共享**：
//! 同一个调用点不管被编译几次，合成类与蹦床都只有一个。
//!
//! # 覆盖范围（不满足就**方法级回退**，绝不编出错的东西）
//!
//! * 实现目标是 `invokestatic` / `invokevirtual` / `invokeinterface` / `invokespecial`；
//! * 参数与返回值做**同类**适配（引用→引用、基本类型同宽或宽度提升）；
//! * **不支持**构造器引用（`REF_newInvokeSpecial`）与需要装箱/拆箱的适配
//!   （如 `BinaryOperator<Integer>` 指向 `Integer::sum(II)I`）。

use cranelift_codegen::ir::{types, AbiParam, InstBuilder, MemFlags, Value};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{Linkage, Module};

use crate::jit::call::{param_descs, param_slots, ret_type};
use crate::jit::indy::LambdaSite;
use crate::jit::layout::{emit_ref_addr, field_off};
use crate::jit::rt;
use crate::jit::JitError;

use super::Jit;

impl Jit {
    /// 把一个 lambda 调用点落到运行时：登记合成类、生成蹦床、登记虚方法表条目。
    /// 返回合成类的 `class_id`（分配 lambda 对象用）。
    pub(super) fn spin_lambda(&mut self, l: &LambdaSite) -> Result<u32, JitError> {
        if l.impl_kind == 8 {
            return Err(JitError(
                "JIT 不支持构造器引用（REF_newInvokeSpecial）".into(),
            ));
        }
        if !(5..=9).contains(&l.impl_kind) {
            return Err(JitError(format!(
                "JIT 不支持这种 MethodHandle 种类 {}",
                l.impl_kind
            )));
        }
        let tag = rt::class_tag(&l.class_name);
        let class_id = rt::register_class(tag, &l.class_name, &l.captured);
        // 实现目标在**编译期**编好（编不了 → 整个方法回退，这是正确的一档）
        let (impl_fid, impl_ml, _) = self.compile_into(&l.impl_class, &l.impl_name, &l.impl_desc)?;

        // ---------- 参数映射 ----------
        // 组合实参 = 捕获字段 ++ SAM 实参；实例方法的组合实参第 0 个是**接收者**。
        let sam_params = param_descs(&l.sam_desc);
        let impl_params = param_descs(&l.impl_desc);
        let is_instance = matches!(l.impl_kind, 5 | 7 | 9);
        let inst_params = param_descs(&l.inst_desc);
        let ncap = l.captured.len();
        if inst_params.len() != sam_params.len() {
            return Err(JitError(format!(
                "instantiatedMethodType 与 SAM 参数个数不一致：{} {} vs {}",
                l.inst_desc,
                l.sam_desc,
                inst_params.len()
            )));
        }
        let mut src_desc: Vec<String> = l.captured.clone();
        src_desc.extend(inst_params.iter().cloned());
        let mut dst_desc: Vec<String> = Vec::new();
        if is_instance {
            dst_desc.push(format!("L{};", l.impl_class));
        }
        dst_desc.extend(impl_params.iter().cloned());
        if src_desc.len() != dst_desc.len() {
            return Err(JitError(format!(
                "lambda 实参个数对不上：站点 {} 个（{}），实现 {}{} 要 {} 个",
                src_desc.len(),
                src_desc.join(""),
                l.impl_name,
                l.impl_desc,
                dst_desc.len()
            )));
        }
        for (s, d) in src_desc.iter().zip(dst_desc.iter()) {
            if adapt_kind(s, d).is_none() {
                return Err(JitError(format!(
                    "JIT 不支持 lambda 参数适配 {s} → {d}（装箱/拆箱未实现）：{}{}",
                    l.impl_name, l.impl_desc
                )));
            }
        }
        let sam_ret = crate::ret_of(&l.sam_desc).unwrap_or(0);
        let impl_ret = crate::ret_of(&l.impl_desc).unwrap_or(0);
        if sam_ret != 0 && impl_ret != 0 && adapt_kind(&(sam_ret as char).to_string(), &(impl_ret as char).to_string()).is_none() {
            return Err(JitError(format!(
                "JIT 不支持 lambda 返回值适配 {} → {}（装箱/拆箱未实现）",
                impl_ret as char, sam_ret as char
            )));
        }

        // ---------- 蹦床： locals[0] = this(lambda 对象)，其后是 SAM 实参 ----------
        let ret_ty = ret_type(&l.sam_desc);
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(ret_ty));
        let tramp = format!("{}.sam", l.class_name);
        let fid = self
            .module
            .declare_function(&tramp, Linkage::Local, &sig)
            .map_err(|e| JitError(e.to_string()))?;

        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        let mut fbctx = FunctionBuilderContext::new();
        let mut fb = FunctionBuilder::new(&mut ctx.func, &mut fbctx);
        let entry = fb.create_block();
        fb.switch_to_block(entry);
        fb.seal_block(entry);
        fb.append_block_params_for_function_params(entry);
        // `locals` 是**实参数组的栈地址**；`locals[0]` 才是 lambda 对象的句柄（堆内偏移）。
        // 两者差一层：字段访问要「基址 + 句柄 + 字段偏移」，SAM 实参访问只要
        // 「数组地址 + 槽位偏移」。
        let locals = fb.block_params(entry)[0];
        let this_h = fb.ins().load(types::I64, MemFlags::new(), locals, 0);
        let this_addr = emit_ref_addr(&mut fb, this_h);

        // 目标方法自己的 locals 数组（槽位布局与普通调用点一致，见 `translate` 的 invokestatic）
        let slot = fb.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
            cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
            (impl_ml.max(1) * 8) as u32,
            3,
        ));
        let sam_slots = param_slots(&l.sam_desc);
        let impl_pslots = param_slots(&l.impl_desc);
        for (i, (s, d)) in src_desc.iter().zip(dst_desc.iter()).enumerate() {
            // 取值：前 ncap 个从 `this` 的字段读，其余从 SAM 实参的槽位读
            let raw = if i < ncap {
                let addr = fb.ins().iadd_imm(this_addr, field_off(i) as i64);
                fb.ins().load(types::I64, MemFlags::new(), addr, 0)
            } else {
                let k = i - ncap;
                let off = ((1 + sam_slots.get(k).copied().unwrap_or(k)) * 8) as i64;
                let addr = fb.ins().iadd_imm(locals, off);
                fb.ins().load(types::I64, MemFlags::new(), addr, 0)
            };
            // 摆到目标方法的槽位上：实例方法的接收者占槽 0，其余按描述符排
            let (ty, dst_off) = if is_instance {
                if i == 0 {
                    (types::I64, 0usize)
                } else {
                    (
                        param_ty(d),
                        (impl_pslots.get(i - 1).copied().unwrap_or(i - 1) + 1) * 8,
                    )
                }
            } else {
                (param_ty(d), impl_pslots.get(i).copied().unwrap_or(i) * 8)
            };
            let v = fit(&mut fb, raw, s, d, ty);
            let addr = fb.ins().stack_addr(types::I64, slot, dst_off as i32);
            fb.ins().store(MemFlags::new(), v, addr, 0);
        }
        let ptr = fb.ins().stack_addr(types::I64, slot, 0);
        let fref = self.module.declare_func_in_func(impl_fid, fb.func);
        let call = fb.ins().call(fref, &[ptr]);
        if sam_ret == 0 || sam_ret == b'V' as u8 || impl_ret == 0 || impl_ret == b'V' as u8 {
            let z = zero(&mut fb, ret_ty);
            fb.ins().return_(&[z]);
        } else {
            let r = fb.inst_results(call)[0];
            let r = fit(
                &mut fb,
                r,
                &(impl_ret as char).to_string(),
                &(sam_ret as char).to_string(),
                ret_ty,
            );
            fb.ins().return_(&[r]);
        }
        fb.seal_all_blocks();
        fb.finalize();
        self.module
            .define_function(fid, &mut ctx)
            .map_err(|e| JitError(format!("{e:?}")))?;
        self.module.clear_context(&mut ctx);

        // 登记成虚方法：`applyAsInt(I)I` 之类打到 lambda 对象上就进蹦床
        let sig_id = rt::intern_sig(&l.sam_name, &l.sam_desc);
        let ret_kind = match ret_ty {
            types::I64 => rt::RET_I64,
            types::F64 => rt::RET_F64,
            types::F32 => rt::RET_F32,
            _ => rt::RET_I32,
        };
        if !self
            .pending_v
            .iter()
            .any(|(t, s, _, _)| *t == tag && *s == sig_id)
        {
            self.pending_v.push((tag, sig_id, fid, ret_kind));
        }
        Ok(class_id)
    }
}

/// 描述符 → Cranelift 类型（参数方向）
fn param_ty(desc: &str) -> cranelift_codegen::ir::Type {
    match desc.as_bytes().first().copied() {
        Some(b'J') => types::I64,
        Some(b'D') => types::F64,
        Some(b'F') => types::F32,
        Some(b'L') | Some(b'[') => types::I64,
        _ => types::I32,
    }
}

/// 两个描述符之间允许的适配类别
#[derive(Clone, Copy, PartialEq)]
enum Adapt {
    /// 引用 → 引用（句柄直接过；JIT 不做 checkcast 校验）
    Ref,
    /// 同一基本类型（宽度也一样）
    Same,
    /// 基本类型的宽度提升（`I`→`J` 之类）
    Wider,
}

/// 判断 `src` 能否**不装箱**直接给 `dst` 用
fn adapt_kind(src: &str, dst: &str) -> Option<Adapt> {
    let (s, d) = (
        src.as_bytes().first().copied()?,
        dst.as_bytes().first().copied()?,
    );
    let is_ref = |c: u8| c == b'L' || c == b'[';
    if is_ref(s) && is_ref(d) {
        return Some(Adapt::Ref);
    }
    if is_ref(s) || is_ref(d) {
        return None; // 装箱/拆箱：不做
    }
    if s == d {
        return Some(Adapt::Same);
    }
    match (s, d) {
        (b'I', b'J') | (b'I', b'F') | (b'I', b'D') | (b'J', b'F') | (b'J', b'D') | (b'F', b'D') => {
            Some(Adapt::Wider)
        }
        _ => None,
    }
}

/// 从 8 字节槽里读出的位模式 → 目标 ABI 类型（按 `src` 的语义解释，再到 `dst`）
///
/// 注意入参可能是两种形状：`load.i64` 读出来的 I64（实参槽），或者目标方法直接
/// 返回的窄值（`inst_results` 对 `I` 返回的就是 I32）。所以这里先看**实际类型**，
/// 别对已经是 I32 的值再 reduce 一次（Cranelift 的 verifier 会直接拒）。
fn fit(
    fb: &mut FunctionBuilder,
    raw: Value,
    src: &str,
    dst: &str,
    want: cranelift_codegen::ir::Type,
) -> Value {
    let kind = adapt_kind(src, dst).unwrap_or(Adapt::Same);
    let s = src.as_bytes()[0] as char;
    let cur = match kind {
        Adapt::Ref => types::I64,
        _ => match s {
            'J' => types::I64,
            'D' => types::F64,
            'F' => types::F32,
            _ => types::I32,
        },
    };
    let have = fb.func.dfg.value_type(raw);
    let raw = if kind == Adapt::Ref || have == cur {
        raw
    } else {
        match (cur, have) {
            (types::F64, types::I64) => fb.ins().bitcast(types::F64, MemFlags::new(), raw),
            (types::F32, types::I64) => {
                let t = fb.ins().ireduce(types::I32, raw);
                fb.ins().bitcast(types::F32, MemFlags::new(), t)
            }
            (types::F32, types::I32) => fb.ins().bitcast(types::F32, MemFlags::new(), raw),
            (types::I32, types::I64) => fb.ins().ireduce(types::I32, raw),
            _ => raw,
        }
    };
    convert(fb, raw, cur, want, kind)
}

/// 类型转换（对应 `translate::convert_to` 的一个小子集）
fn convert(
    fb: &mut FunctionBuilder,
    v: Value,
    cur: cranelift_codegen::ir::Type,
    want: cranelift_codegen::ir::Type,
    kind: Adapt,
) -> Value {
    if cur == want || kind == Adapt::Ref {
        return v;
    }
    match (cur, want) {
        (types::I32, types::I64) => fb.ins().sextend(types::I64, v),
        (types::I32, types::F32) => fb.ins().fcvt_from_sint(types::F32, v),
        (types::I32, types::F64) => fb.ins().fcvt_from_sint(types::F64, v),
        (types::I64, types::F32) => fb.ins().fcvt_from_sint(types::F32, v),
        (types::I64, types::F64) => fb.ins().fcvt_from_sint(types::F64, v),
        (types::F32, types::F64) => fb.ins().fpromote(types::F64, v),
        (types::F64, types::F32) => fb.ins().fdemote(types::F32, v),
        (types::I64, types::I32) => fb.ins().ireduce(types::I32, v),
        (types::I32, types::I32) => v,
        _ => v,
    }
}

/// 返回类型的零值（SAM 是 `void` 时蹦床要还一个）
fn zero(fb: &mut FunctionBuilder, ty: cranelift_codegen::ir::Type) -> Value {
    match ty {
        types::F64 => fb.ins().f64const(0.0),
        types::F32 => fb.ins().f32const(0.0),
        types::I64 => fb.ins().iconst(types::I64, 0),
        _ => fb.ins().iconst(types::I32, 0),
    }
}
