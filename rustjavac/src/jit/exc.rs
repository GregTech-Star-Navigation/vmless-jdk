//! 异常哨兵与 handler 派发。

use cranelift_codegen::ir::{types, InstBuilder, MemFlags, Value};
use cranelift_frontend::FunctionBuilder;
use cranelift_module::Module;

use crate::ClassFile;

use super::call::{code_of, zero_of};
use super::insn::insn_len;
use super::layout::class_const_tag;
use super::{Jit, JitError};

/// 异常相关运行时函数的 FuncId 集合
pub(super) struct ExcHelpers {
    pub(super) pending: cranelift_module::FuncId,
    pub(super) clear: cranelift_module::FuncId,
    pub(super) get: cranelift_module::FuncId,
    pub(super) throw_: cranelift_module::FuncId,
    pub(super) set_msg: cranelift_module::FuncId,
    pub(super) catch_match: cranelift_module::FuncId,
}

impl Jit {
    /// 生成「异常派发」代码：当前块**一定会被终结**（跳到 handler 或原样返回哨兵）。
    ///
    /// 调用前必须已经 `vmless_exc_throw` 过（pending=1），异常对象在运行时的全局里。
    pub(super) fn emit_dispatch(
        &mut self,
        fb: &mut FunctionBuilder,
        cf: &ClassFile,
        exc_table: &[(u16, u16, u16, u16)],
        blocks: &std::collections::HashMap<usize, cranelift_codegen::ir::Block>,
        exc_slot: cranelift_codegen::ir::StackSlot,
        helpers: &ExcHelpers,
        at: usize,
        ret_ty: cranelift_codegen::ir::Type,
    ) -> Result<(), JitError> {
        let obj = {
            let f = self.module.declare_func_in_func(helpers.get, fb.func);
            let c = fb.ins().call(f, &[]);
            fb.inst_results(c)[0]
        };
        // 把异常对象存进通道槽，handler 入口从那里读（跨块只能用内存传）
        let store_exc = |fb: &mut FunctionBuilder, obj: Value| {
            let a = fb.ins().stack_addr(types::I64, exc_slot, 0);
            fb.ins().store(MemFlags::new(), obj, a, 0);
        };
        for &(start, end, handler, catch_type) in exc_table {
            if !((start as usize) <= at && at < (end as usize)) {
                continue;
            }
            let hb = *blocks
                .get(&(handler as usize))
                .ok_or_else(|| JitError(format!("handler {handler} 没有对应块")))?;
            if catch_type == 0 {
                // catch-all（finally）
                let f = self.module.declare_func_in_func(helpers.clear, fb.func);
                fb.ins().call(f, &[]);
                store_exc(fb, obj);
                fb.ins().jump(hb, &[]);
                return Ok(());
            }
            let tag = class_const_tag(cf, catch_type)?;
            let ok = fb.create_block();
            let next = fb.create_block();
            let m = {
                let f = self.module.declare_func_in_func(helpers.catch_match, fb.func);
                let tg = fb.ins().iconst(types::I32, tag as i64);
                let c = fb.ins().call(f, &[obj, tg]);
                fb.inst_results(c)[0]
            };
            fb.ins().brif(m, ok, &[], next, &[]);
            fb.switch_to_block(ok);
            let f = self.module.declare_func_in_func(helpers.clear, fb.func);
            fb.ins().call(f, &[]);
            store_exc(fb, obj);
            fb.ins().jump(hb, &[]);
            fb.switch_to_block(next);
        }
        // 本方法接不住：把哨兵返回给调用方，pending 保持置位，由它继续找
        let z = zero_of(fb, ret_ty);
        fb.ins().return_(&[z]);
        Ok(())
    }
}

/// 这个类里有没有任何方法用到异常（异常表非空或出现 athrow）
    pub(super) fn class_uses_exceptions(cf: &ClassFile) -> bool {
    for m in &cf.methods {
        let Ok((_, body, table)) = code_of(cf, m) else { continue };
        if !table.is_empty() {
            return true;
        }
        let mut q = 0usize;
        while q < body.len() {
            if body[q] == 0xbf {
                return true;
            }
            q += insn_len(body[q]);
        }
    }
    false
}
