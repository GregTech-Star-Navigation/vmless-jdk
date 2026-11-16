//! 字节码 → Cranelift IR 的主翻译器。

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{types, AbiParam, InstBuilder, MemFlags, Value};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{Linkage, Module};

use crate::{ClassFile, CpEntry};

use super::call::{argc_of, code_of, convert_to, method_ref_of, param_slots, ret_type, zero_of};
use super::exc::{class_uses_exceptions, ExcHelpers};
use super::insn::insn_len;
use super::layout::{class_const_tag, elem_addr, field_ref_of, field_slot_type, push_new_array};
use super::rt;
use super::strtab;
use super::{Jit, JitError};

impl Jit {
    /// 编译（或复用）某个类里声明的一个方法，返回 (函数 id, max_locals, 参数个数)
    ///
    /// 缓存按 **(类名, 方法名, 描述符)** 区分 —— 父子类同名方法必须编成两个函数。
    pub fn compile_into(
        &mut self,
        class: &str,
        name: &str,
        desc: &str,
    ) -> Result<(cranelift_module::FuncId, usize, i32), JitError> {
        if let Some(v) = self.compiled.get(&(class.to_string(), name.to_string(), desc.to_string())) {
            return Ok(*v);
        }
        let cf = self
            .classes
            .get(class)
            .ok_or_else(|| JitError(format!("类 {class} 没有加载")))?;
        let (max_locals, body, exc_table) = {
            let m = cf
                .methods
                .iter()
                .find(|m| {
                    let n = String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default());
                    let d = String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default());
                    n == name && d == desc
                })
                .ok_or_else(|| JitError(format!("{class} 里找不到方法 {name}{desc}")))?;
            code_of(&cf, m)?
        };

        // 方法签名：(locals_ptr: i64) -> 返回值
        // 返回值必须按描述符给出真类型：long → i64、double → f64。
        // 统一返回 i32 再在调用点还原会把 long/double 截断掉，跨调用就丢精度。
        let ret_ty = ret_type(desc);
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(ret_ty));

        // 函数名必须按类区分：父子类同名方法要编成两个函数
        let fname = format!("{}_{}_{}", class.replace('/', "_"), name, desc.replace(['(', ')', ';', '/', '[', ']'], "_"));
        let func_id = self
            .module
            .declare_function(&fname, Linkage::Export, &sig)
            .map_err(|e| JitError(e.to_string()))?;
        // 先登记，递归调用（fib 调 fib）才能命中缓存而不是无限递归
        self.compiled.insert(
            (class.to_string(), name.to_string(), desc.to_string()),
            (func_id, max_locals, argc_of(desc)),
        );

        let mut ctx = self.module.make_context();
        ctx.func.signature = sig.clone();
        // 声明运行时辅助函数
        let mut sig_new_obj = self.module.make_signature();
        sig_new_obj.params.push(AbiParam::new(types::I32)); // 字段个数
        sig_new_obj.params.push(AbiParam::new(types::I32)); // 类型标签
        sig_new_obj.returns.push(AbiParam::new(types::I64));
        let new_obj_id = self
            .module
            .declare_function("vmless_new_object", Linkage::Import, &sig_new_obj)
            .map_err(|e| JitError(e.to_string()))?;
        let mut sig_new_arr = self.module.make_signature();
        sig_new_arr.params.push(AbiParam::new(types::I32)); // 长度
        sig_new_arr.params.push(AbiParam::new(types::I32)); // 元素字节数
        sig_new_arr.params.push(AbiParam::new(types::I32)); // 类型标签
        sig_new_arr.returns.push(AbiParam::new(types::I64));
        let new_arr_id = self
            .module
            .declare_function("vmless_new_array", Linkage::Import, &sig_new_arr)
            .map_err(|e| JitError(e.to_string()))?;
        // multianewarray：counts 指针 + 维数 + 描述符串指针
        let mut sig_new_multi = self.module.make_signature();
        sig_new_multi.params.push(AbiParam::new(types::I64));
        sig_new_multi.params.push(AbiParam::new(types::I32));
        sig_new_multi.params.push(AbiParam::new(types::I64));
        sig_new_multi.returns.push(AbiParam::new(types::I64));
        let new_multi_id = self
            .module
            .declare_function("vmless_new_multi_array", Linkage::Import, &sig_new_multi)
            .map_err(|e| JitError(e.to_string()))?;

        let mut sig_pi = self.module.make_signature();
        sig_pi.params.push(AbiParam::new(types::I32));
        let println_int_id = self
            .module
            .declare_function("vmless_println_int", Linkage::Import, &sig_pi)
            .map_err(|e| JitError(e.to_string()))?;
        let println_bool_id = self
            .module
            .declare_function("vmless_println_bool", Linkage::Import, &sig_pi)
            .map_err(|e| JitError(e.to_string()))?;

        let mut sig_io = self.module.make_signature();
        sig_io.params.push(AbiParam::new(types::I64)); // 对象指针
        sig_io.params.push(AbiParam::new(types::I32)); // 目标类型标签
        sig_io.returns.push(AbiParam::new(types::I32));
        let instanceof_id = self
            .module
            .declare_function("vmless_instanceof", Linkage::Import, &sig_io)
            .map_err(|e| JitError(e.to_string()))?;
        let mut sig_cc = sig_io.clone();
        sig_cc.returns[0] = AbiParam::new(types::I64);
        let checkcast_id = self
            .module
            .declare_function("vmless_checkcast", Linkage::Import, &sig_cc)
            .map_err(|e| JitError(e.to_string()))?;

        let mut sig_pl = self.module.make_signature();
        sig_pl.params.push(AbiParam::new(types::I64));
        let println_long_id = self
            .module
            .declare_function("vmless_println_long", Linkage::Import, &sig_pl)
            .map_err(|e| JitError(e.to_string()))?;

        let mut sig_pd = self.module.make_signature();
        sig_pd.params.push(AbiParam::new(types::F64));
        let println_double_id = self
            .module
            .declare_function("vmless_println_double", Linkage::Import, &sig_pd)
            .map_err(|e| JitError(e.to_string()))?;

        // 登记常量池里的字符串常量（ldc 用）
        let mut str_ids: std::collections::HashMap<u16, u32> = std::collections::HashMap::new();
        for (i, e) in cf.constant_pool.iter().enumerate() {
            if let Some(CpEntry::String(u)) = e {
                if let Some(b) = cf.utf8(*u) {
                    str_ids.insert(i as u16, strtab::register(&String::from_utf8_lossy(b)));
                }
            }
        }
        let mut sig_sc = self.module.make_signature();
        sig_sc.params.push(AbiParam::new(types::I32));
        sig_sc.returns.push(AbiParam::new(types::I64));
        let str_const_id = self
            .module
            .declare_function("vmless_str_const", Linkage::Import, &sig_sc)
            .map_err(|e| JitError(e.to_string()))?;
        let mut sig_ps = self.module.make_signature();
        sig_ps.params.push(AbiParam::new(types::I64));
        let println_obj_id = self
            .module
            .declare_function("vmless_println_obj", Linkage::Import, &sig_ps)
            .map_err(|e| JitError(e.to_string()))?;

        let mut sig_ff = self.module.make_signature();
        sig_ff.params.push(AbiParam::new(types::F64));
        sig_ff.params.push(AbiParam::new(types::F64));
        sig_ff.returns.push(AbiParam::new(types::F64));
        let drem_id = self
            .module
            .declare_function("vmless_drem", Linkage::Import, &sig_ff)
            .map_err(|e| JitError(e.to_string()))?;

        // 异常机制：pending 标志 / 取异常对象 / 清标志 / catch 类型匹配 / 抛 / 记消息
        let mut sig_i0 = self.module.make_signature();
        sig_i0.returns.push(AbiParam::new(types::I32));
        let exc_pending_id = self
            .module
            .declare_function("vmless_exc_pending", Linkage::Import, &sig_i0)
            .map_err(|e| JitError(e.to_string()))?;
        let sig_void = self.module.make_signature();
        let exc_clear_id = self
            .module
            .declare_function("vmless_exc_clear", Linkage::Import, &sig_void)
            .map_err(|e| JitError(e.to_string()))?;
        let mut sig_l0 = self.module.make_signature();
        sig_l0.returns.push(AbiParam::new(types::I64));
        let exc_get_id = self
            .module
            .declare_function("vmless_exc_get", Linkage::Import, &sig_l0)
            .map_err(|e| JitError(e.to_string()))?;
        let mut sig_li = self.module.make_signature();
        sig_li.params.push(AbiParam::new(types::I64));
        let exc_throw_id = self
            .module
            .declare_function("vmless_exc_throw", Linkage::Import, &sig_li)
            .map_err(|e| JitError(e.to_string()))?;
        let mut sig_ll = self.module.make_signature();
        sig_ll.params.push(AbiParam::new(types::I64));
        sig_ll.params.push(AbiParam::new(types::I64));
        let exc_msg_id = self
            .module
            .declare_function("vmless_exc_set_msg", Linkage::Import, &sig_ll)
            .map_err(|e| JitError(e.to_string()))?;
        let mut sig_li2 = self.module.make_signature();
        sig_li2.params.push(AbiParam::new(types::I64));
        sig_li2.params.push(AbiParam::new(types::I32));
        sig_li2.returns.push(AbiParam::new(types::I32));
        let catch_match_id = self
            .module
            .declare_function("vmless_catch_match", Linkage::Import, &sig_li2)
            .map_err(|e| JitError(e.to_string()))?;
        // 虚方法分派：vcall(locals: i64, sig: i32) -> i64
        let mut sig_vc = self.module.make_signature();
        sig_vc.params.push(AbiParam::new(types::I64));
        sig_vc.params.push(AbiParam::new(types::I32));
        sig_vc.returns.push(AbiParam::new(types::I64));
        let vcall_id = self
            .module
            .declare_function("vmless_vcall", Linkage::Import, &sig_vc)
            .map_err(|e| JitError(e.to_string()))?;
        let helpers = ExcHelpers {
            pending: exc_pending_id,
            clear: exc_clear_id,
            get: exc_get_id,
            throw_: exc_throw_id,
            set_msg: exc_msg_id,
            catch_match: catch_match_id,
        };
        // 本**类**有没有用到异常：没有就不发任何检查，避免拖慢不带异常的代码。
        // 注意必须按类判定 —— 只按方法判定的话，`a()` 里不含 try 但被调方抛异常时
        // 就没有检查点，异常会跑到 main 结束之后才被顶层发现（中间的语句照常执行）。
        let uses_exc = class_uses_exceptions(&cf);
        let mut bctx = FunctionBuilderContext::new();
        let mut fb = FunctionBuilder::new(&mut ctx.func, &mut bctx);

        // 异常对象从 dispatch 块传到 handler 块的通道（跨块只能用内存传）
        let exc_slot = fb.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
            cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
            8,
            3,
        ));

        let entry = fb.create_block();
        fb.append_block_params_for_function_params(entry);
        fb.switch_to_block(entry);

        let locals_ptr = fb.block_params(entry)[0];

        // 先找出所有跳转目标，为它们建块
        let mut targets: Vec<usize> = Vec::new();
        let mut pc = 0usize;
        while pc < body.len() {
            let op = body[pc];
            match op {
                // 条件跳转 / goto / ifnull / ifnonnull：目标 + 落空地址都是块首
                0x99..=0xa7 | 0xc6 | 0xc7 => {
                    let off = i16::from_be_bytes([body[pc + 1], body[pc + 2]]) as isize;
                    targets.push((pc as isize + off) as usize);
                    targets.push(pc + 3);
                    pc += 3;
                }
                _ => pc += insn_len(op),
            }
        }
        // handler 入口也是块首（栈上要放一个异常对象）
        let mut handler_pcs: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for &(_, _, h, _) in &exc_table {
            targets.push(h as usize);
            handler_pcs.insert(h as usize);
        }
        targets.sort_unstable();
        targets.dedup();

        let mut blocks: std::collections::HashMap<usize, cranelift_codegen::ir::Block> =
            std::collections::HashMap::new();
        for t in &targets {
            let b = fb.create_block();
            blocks.insert(*t, b);
        }

        // 局部变量在内存里：locals_ptr + i*4
        let load_local = |fb: &mut FunctionBuilder, ptr: Value, slot: usize| -> Value {
            let addr = fb.ins().iadd_imm(ptr, (slot * 8) as i64);
            fb.ins().load(types::I32, MemFlags::new(), addr, 0)
        };
        let store_local = |fb: &mut FunctionBuilder, ptr: Value, slot: usize, v: Value| {
            let addr = fb.ins().iadd_imm(ptr, (slot * 8) as i64);
            fb.ins().store(MemFlags::new(), v, addr, 0);
        };

        // 先把本方法里所有 invokestatic 目标编译好，得到 FuncId 映射
        let mut call_targets: std::collections::HashMap<u16, (cranelift_module::FuncId, usize, i32)> =
            std::collections::HashMap::new();
        {
            let mut q = 0usize;
            while q < body.len() {
                let op = body[q];
                let len = insn_len(op);
                if (op == 0xb8 || op == 0xb7) && len == 3 {
                    let idx = u16::from_be_bytes([body[q + 1], body[q + 2]]);
                    if let Ok(info) = method_ref_of(&cf, idx) {
                        // 从引用类沿继承链找**声明类**：父类里的方法也要能调到
                        if let Some((owner, _)) = self.classes.resolve_method(&info.0, &info.1, &info.2) {
                            let (fid, ml, argc) = self.compile_into(&owner, &info.1, &info.2)?;
                            let extra = if op == 0xb7 { 1 } else { 0 };
                            call_targets.insert(idx, (fid, ml, argc + extra));
                        }
                        // 找不到（java.* 的方法 / 构造器）就当作外部调用，按空操作处理
                    }
                }
                q += len;
            }
        }

        let mut stack: Vec<Value> = Vec::new();
        // 记录尚未写终结指令的块（收尾时统一补 return）
        let mut open: Vec<cranelift_codegen::ir::Block> = vec![entry];
        let mut cur_block = entry;
        let mut terminated = false;
        let mut cur_start = 0usize;
        let mut pc = 0usize;
        let mut returns = 0usize;

        while pc < body.len() {
            // 到达一个新块的开头？
            if pc != cur_start && blocks.contains_key(&pc) {
                if !stack.is_empty() {
                    return Err(JitError("跳转点操作数栈非空，暂不支持".into()));
                }
                let tb = blocks[&pc];
                if !terminated {
                    fb.ins().jump(tb, &[]);
                }
                terminated = false;
                open.pop();
                open.push(tb);
                fb.switch_to_block(tb);
                cur_block = tb;
                cur_start = pc;
            }
            let _ = cur_block;

            // handler 入口：操作数栈上有一个异常对象（从通道槽里取回来）。
            // 取不到就说明没人会跳进来，给个 0 占位即可。
            if handler_pcs.contains(&pc) {
                let a = fb.ins().stack_addr(types::I64, exc_slot, 0);
                let v = fb.ins().load(types::I64, MemFlags::new(), a, 0);
                stack.push(v);
            }

            let op = body[pc];
            pc += 1;
            match op {
                0x00 => {}
                0x01 => stack.push(fb.ins().iconst(types::I64, 0)), // aconst_null
                0x02 => stack.push(fb.ins().iconst(types::I32, -1)),
                0x03..=0x08 => stack.push(fb.ins().iconst(types::I32, (op as i64) - 3)),
                0x10 => {
                    let v = body[pc] as i8 as i64;
                    pc += 1;
                    stack.push(fb.ins().iconst(types::I32, v));
                }
                0x11 => {
                    let v = i16::from_be_bytes([body[pc], body[pc + 1]]) as i64;
                    pc += 2;
                    stack.push(fb.ins().iconst(types::I32, v));
                }
                0x12 => {
                    let idx = body[pc] as u16;
                    pc += 1;
                    if let Some(&sid) = str_ids.get(&idx) {
                        let c = fb.ins().iconst(types::I32, sid as i64);
                        let fref = self.module.declare_func_in_func(str_const_id, fb.func);
                        let call = fb.ins().call(fref, &[c]);
                        let p = fb.inst_results(call)[0];
                        stack.push(p);
                    } else {
                        stack.push(fb.ins().iconst(types::I32, const_int(&cf, idx)));
                    }
                }
                0x13 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    if let Some(&sid) = str_ids.get(&idx) {
                        let c = fb.ins().iconst(types::I32, sid as i64);
                        let fref = self.module.declare_func_in_func(str_const_id, fb.func);
                        let call = fb.ins().call(fref, &[c]);
                        let p = fb.inst_results(call)[0];
                        stack.push(p);
                    } else {
                        stack.push(fb.ins().iconst(types::I32, const_int(&cf, idx)));
                    }
                }
                0x15 => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = load_local(&mut fb, locals_ptr, slot);
                    stack.push(v);
                }
                0x1a..=0x1d => {
                    let slot = (op - 0x1a) as usize;
                    let v = load_local(&mut fb, locals_ptr, slot);
                    stack.push(v);
                }
                0x36 => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    store_local(&mut fb, locals_ptr, slot, v);
                }
                0x3b..=0x3e => {
                    let slot = (op - 0x3b) as usize;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    store_local(&mut fb, locals_ptr, slot, v);
                }
                0x84 => {
                    // iinc
                    let slot = body[pc] as usize;
                    let delta = body[pc + 1] as i8 as i64;
                    pc += 2;
                    let v = load_local(&mut fb, locals_ptr, slot);
                    let nv = fb.ins().iadd_imm(v, delta);
                    store_local(&mut fb, locals_ptr, slot, nv);
                }
                0x57 | 0x58 => {
                    // pop / pop2：栈上一个条目就是一个 JVM 值，宽值也只占一条
                    stack.pop();
                }
                0x60 => bin(&mut fb, &mut stack, |f, a, b| f.ins().iadd(a, b))?,
                0x64 => bin(&mut fb, &mut stack, |f, a, b| f.ins().isub(a, b))?,
                0x68 => bin(&mut fb, &mut stack, |f, a, b| f.ins().imul(a, b))?,
                0x6c => bin(&mut fb, &mut stack, |f, a, b| f.ins().sdiv(a, b))?,
                0x70 => bin(&mut fb, &mut stack, |f, a, b| f.ins().srem(a, b))?,
                0x74 => {
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    stack.push(fb.ins().ineg(a));
                }
                // 与 0 比较的条件跳转
                0x99..=0x9e => {
                    let off = i16::from_be_bytes([body[pc], body[pc + 1]]) as isize;
                    pc += 2;
                    let target = (pc as isize - 3 + off) as usize;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let zero = fb.ins().iconst(types::I32, 0);
                    let cc = match op {
                        0x99 => IntCC::Equal,
                        0x9a => IntCC::NotEqual,
                        0x9b => IntCC::SignedLessThan,
                        0x9c => IntCC::SignedGreaterThanOrEqual,
                        0x9d => IntCC::SignedGreaterThan,
                        _ => IntCC::SignedLessThanOrEqual,
                    };
                    let tb = *blocks.get(&target).ok_or_else(|| JitError("缺少目标块".into()))?;
                    let fall = *blocks.get(&pc).ok_or_else(|| JitError("缺少落空块".into()))?;
                    let cond = fb.ins().icmp(cc, v, zero);
                    fb.ins().brif(cond, tb, &[], fall, &[]);
                    open.pop();
                    open.push(fall);
                    fb.switch_to_block(fall);
                    terminated = false;
                    cur_start = pc;
                }
                // ifnull / ifnonnull：引用是 I64，与 0 比较
                0xc6 | 0xc7 => {
                    let off = i16::from_be_bytes([body[pc], body[pc + 1]]) as isize;
                    pc += 2;
                    let target = (pc as isize - 3 + off) as usize;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let v = convert_to(&mut fb, v, types::I64);
                    let zero = fb.ins().iconst(types::I64, 0);
                    let cc = if op == 0xc6 { IntCC::Equal } else { IntCC::NotEqual };
                    let tb = *blocks.get(&target).ok_or_else(|| JitError("缺少目标块".into()))?;
                    let fall = *blocks.get(&pc).ok_or_else(|| JitError("缺少落空块".into()))?;
                    let cond = fb.ins().icmp(cc, v, zero);
                    fb.ins().brif(cond, tb, &[], fall, &[]);
                    open.pop();
                    open.push(fall);
                    fb.switch_to_block(fall);
                    terminated = false;
                    cur_start = pc;
                }
                // 二元比较跳转（0xa5/0xa6 是引用比较，引用是 I64）
                0x9f..=0xa4 | 0xa5 | 0xa6 => {
                    let off = i16::from_be_bytes([body[pc], body[pc + 1]]) as isize;
                    pc += 2;
                    let target = (pc as isize - 3 + off) as usize;
                    let b = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let cc = match op {
                        0x9f => IntCC::Equal,
                        0xa0 => IntCC::NotEqual,
                        0xa1 => IntCC::SignedLessThan,
                        0xa2 => IntCC::SignedGreaterThanOrEqual,
                        0xa3 => IntCC::SignedGreaterThan,
                        0xa4 => IntCC::SignedLessThanOrEqual,
                        // if_acmpeq / if_acmpne：按 8 字节比较
                        _ => {
                            let a = convert_to(&mut fb, a, types::I64);
                            let b = convert_to(&mut fb, b, types::I64);
                            let tb = *blocks.get(&target).ok_or_else(|| JitError("缺少目标块".into()))?;
                            let fall = *blocks.get(&pc).ok_or_else(|| JitError("缺少落空块".into()))?;
                            let c = if op == 0xa5 { IntCC::Equal } else { IntCC::NotEqual };
                            let cond = fb.ins().icmp(c, a, b);
                            fb.ins().brif(cond, tb, &[], fall, &[]);
                            open.pop();
                            open.push(fall);
                            fb.switch_to_block(fall);
                            terminated = false;
                            cur_start = pc;
                            continue;
                        }
                    };
                    let tb = *blocks.get(&target).ok_or_else(|| JitError("缺少目标块".into()))?;
                    let fall = *blocks.get(&pc).ok_or_else(|| JitError("缺少落空块".into()))?;
                    let cond = fb.ins().icmp(cc, a, b);
                    fb.ins().brif(cond, tb, &[], fall, &[]);
                    open.pop();
                    open.push(fall);
                    fb.switch_to_block(fall);
                    terminated = false;
                    cur_start = pc;
                }
                0xa7 => {
                    let off = i16::from_be_bytes([body[pc], body[pc + 1]]) as isize;
                    pc += 2;
                    let target = (pc as isize - 3 + off) as usize;
                    let tb = *blocks.get(&target).ok_or_else(|| JitError("缺少目标块".into()))?;
                    fb.ins().jump(tb, &[]);
                    open.pop();
                    let fall = *blocks.get(&pc).ok_or_else(|| JitError("缺少落空块".into()))?;
                    fb.switch_to_block(fall);
                    open.push(fall);
                    terminated = false;
                    cur_start = pc;
                }
                // ireturn / lreturn / freturn / dreturn / areturn：统一按方法签名声明的返回类型收口
                0xac | 0xad | 0xae | 0xaf | 0xb0 => {
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let v = convert_to(&mut fb, v, ret_ty);
                    fb.ins().return_(&[v]);
                    returns += 1;
                    terminated = true;
                    open.pop();
                    // 后续代码放进一个不可达块
                    let dead = fb.create_block();
                    open.push(dead);
                    fb.switch_to_block(dead);
                    stack.clear();
                }
                0xb1 => {
                    let zero = zero_of(&mut fb, ret_ty);
                    fb.ins().return_(&[zero]);
                    returns += 1;
                    terminated = true;
                    open.pop();
                    let dead = fb.create_block();
                    open.push(dead);
                    fb.switch_to_block(dead);
                    stack.clear();
                }
                // athrow：把对象交给运行时，然后按本方法的异常表派发
                0xbf => {
                    let op_pc = pc - 1;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let v = convert_to(&mut fb, v, types::I64);
                    let f = self.module.declare_func_in_func(helpers.throw_, fb.func);
                    fb.ins().call(f, &[v]);
                    self.emit_dispatch(&mut fb, &cf, &exc_table, &blocks, exc_slot, &helpers, op_pc, ret_ty)?;
                    terminated = true;
                    open.pop();
                    let dead = fb.create_block();
                    open.push(dead);
                    fb.switch_to_block(dead);
                    stack.clear();
                }
                0xb8 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let (fid, callee_max_locals, argc) = *call_targets
                        .get(&idx)
                        .ok_or_else(|| JitError("调用目标未编译".into()))?;
                    // 首次调用这个类的静态方法时先跑它的 <clinit>
                    let callee_owner = method_ref_of(&cf, idx)?.0;
                    self.emit_clinit_call(&mut fb, &callee_owner)?;
                    // 参数出栈（逆序），写进为被调方法准备的栈槽
                    let mut args = Vec::new();
                    for _ in 0..argc {
                        args.push(stack.pop().ok_or_else(|| JitError("栈下溢".into()))?);
                    }
                    args.reverse();
                    let slot = fb.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
                        cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
                        (callee_max_locals.max(1) * 8) as u32,
                        2,
                    ));
                    // 按 JVM 槽位放参数：long/double 占两槽，后面那个参数要跳过一槽
                    let callee_desc = method_ref_of(&cf, idx)?.2;
                    let pslots = param_slots(&callee_desc);
                    for (i, v) in args.iter().enumerate() {
                        let off = (pslots.get(i).copied().unwrap_or(i) * 8) as i32;
                        let addr = fb.ins().stack_addr(types::I64, slot, off);
                        fb.ins().store(MemFlags::new(), *v, addr, 0);
                    }
                    let ptr = fb.ins().stack_addr(types::I64, slot, 0);
                    let fref = self.module.declare_func_in_func(fid, fb.func);
                    let call = fb.ins().call(fref, &[ptr]);
                    // 被调方已按自己的返回类型返回（i64/f64/i32），这里直接上栈
                    if !callee_desc.ends_with(")V") {
                        stack.push(fb.inst_results(call)[0]);
                    }
                    if uses_exc {
                        // 被调方抛了？命中就派发，否则进 cont 块继续
                        let op_pc = pc - 3;
                        let cont = fb.create_block();
                        let disp = fb.create_block();
                        let pf = self.module.declare_func_in_func(helpers.pending, fb.func);
                        let pcx = fb.ins().call(pf, &[]);
                        let flag = fb.inst_results(pcx)[0];
                        fb.ins().brif(flag, disp, &[], cont, &[]);
                        fb.switch_to_block(disp);
                        self.emit_dispatch(&mut fb, &cf, &exc_table, &blocks, exc_slot, &helpers, op_pc, ret_ty)?;
                        fb.switch_to_block(cont);
                    }
                }
                // 引用类型局部变量（8 字节槽）
                0x2a..=0x2d => {
                    let slot = (op - 0x2a) as usize;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    let v = fb.ins().load(types::I64, MemFlags::new(), addr, 0);
                    stack.push(v);
                }
                0x19 => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    let v = fb.ins().load(types::I64, MemFlags::new(), addr, 0);
                    stack.push(v);
                }
                0x4b..=0x4e => {
                    let slot = (op - 0x4b) as usize;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    fb.ins().store(MemFlags::new(), v, addr, 0);
                }
                0x3a => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    fb.ins().store(MemFlags::new(), v, addr, 0);
                }
                0x59 => {
                    let v = *stack.last().ok_or_else(|| JitError("栈下溢".into()))?;
                    stack.push(v);
                }
                0xb7 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let Some(&(fid, callee_max_locals, argc)) = call_targets.get(&idx) else {
                        // 外部构造器（Object.<init> / RuntimeException.<init>）：没有它的
                        // 字节码，只把 message 记到异常对象上（打印未捕获异常要用）
                        let desc = method_ref_of(&cf, idx)?.2;
                        if desc == "(Ljava/lang/String;)V" && stack.len() >= 2 {
                            let msg = stack.pop().unwrap();
                            let obj = stack.pop().unwrap();
                            let f = self.module.declare_func_in_func(helpers.set_msg, fb.func);
                            fb.ins().call(f, &[obj, msg]);
                        } else {
                            for _ in 0..(argc_of(&desc) + 1) {
                                stack.pop();
                            }
                        }
                        continue;
                    };
                    let mut args = Vec::new();
                    for _ in 0..argc {
                        args.push(stack.pop().ok_or_else(|| JitError("栈下溢".into()))?);
                    }
                    args.reverse();
                    let slot = fb.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
                        cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
                        (callee_max_locals.max(1) * 8) as u32,
                        3,
                    ));
                    let mut pslots = vec![0usize];
                    pslots.extend(param_slots(&method_ref_of(&cf, idx)?.2).iter().map(|s| s + 1));
                    for (i, v) in args.iter().enumerate() {
                        let off = (pslots.get(i).copied().unwrap_or(i) * 8) as i32;
                        let addr = fb.ins().stack_addr(types::I64, slot, off);
                        fb.ins().store(MemFlags::new(), *v, addr, 0);
                    }
                    let ptr = fb.ins().stack_addr(types::I64, slot, 0);
                    let fref = self.module.declare_func_in_func(fid, fb.func);
                    let c = fb.ins().call(fref, &[ptr]);
                    // super.m() 有返回值：直接调用，被调方返回的就是真类型
                    let callee_desc = method_ref_of(&cf, idx)?.2;
                    if !callee_desc.ends_with(")V") {
                        stack.push(fb.inst_results(c)[0]);
                    }
                }
                // long：常量
                0x09 | 0x0a => {
                    stack.push(fb.ins().iconst(types::I64, (op as i64) - 0x09));
                }
                0x14 => {
                    // ldc2_w（long / double 常量）
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    match cf.constant_pool.get(idx as usize).and_then(|e| e.as_ref()) {
                        Some(CpEntry::Double(bits)) => {
                            stack.push(fb.ins().f64const(f64::from_bits(*bits)));
                        }
                        Some(CpEntry::Long(v)) => stack.push(fb.ins().iconst(types::I64, *v as i64)),
                        _ => stack.push(fb.ins().iconst(types::I64, 0)),
                    }
                }
                0x0e | 0x0f => {
                    // dconst_0 / dconst_1
                    let v = if op == 0x0e { 0.0 } else { 1.0 };
                    stack.push(fb.ins().f64const(v));
                }
                0x18 => {
                    // dload
                    let slot = body[pc] as usize;
                    pc += 1;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    let v = fb.ins().load(types::F64, MemFlags::new(), addr, 0);
                    stack.push(v);
                }
                0x26..=0x29 => {
                    // dload_0..3
                    let slot = (op - 0x26) as usize;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    let v = fb.ins().load(types::F64, MemFlags::new(), addr, 0);
                    stack.push(v);
                }
                0x38 | 0x39 => {
                    // fstore(0x38) / dstore(0x39)：都只是把栈顶值按值存进 8 字节槽
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    fb.ins().store(MemFlags::new(), v, addr, 0);
                }
                0x47..=0x4a => {
                    // dstore_0..3
                    let slot = (op - 0x47) as usize;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    fb.ins().store(MemFlags::new(), v, addr, 0);
                }
                0x16 => {
                    // lload
                    let slot = body[pc] as usize;
                    pc += 1;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    let v = fb.ins().load(types::I64, MemFlags::new(), addr, 0);
                    stack.push(v);
                }
                0x1e..=0x21 => {
                    // lload_0..3
                    let slot = (op - 0x1e) as usize;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    let v = fb.ins().load(types::I64, MemFlags::new(), addr, 0);
                    stack.push(v);
                }
                0x37 => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    fb.ins().store(MemFlags::new(), v, addr, 0);
                }
                0x3f..=0x42 => {
                    let slot = (op - 0x3f) as usize;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    fb.ins().store(MemFlags::new(), v, addr, 0);
                }
                0x61 => lng(&mut fb, &mut stack, |f, a, b| f.ins().iadd(a, b))?,
                0x65 => lng(&mut fb, &mut stack, |f, a, b| f.ins().isub(a, b))?,
                0x69 => lng(&mut fb, &mut stack, |f, a, b| f.ins().imul(a, b))?,
                0x6d => lng(&mut fb, &mut stack, |f, a, b| f.ins().sdiv(a, b))?,
                0x71 => lng(&mut fb, &mut stack, |f, a, b| f.ins().srem(a, b))?,
                0x75 => {
                    let b = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    stack.push(fb.ins().ineg(b));
                }
                // double 算术 / 取反
                0x63 => lng(&mut fb, &mut stack, |f, a, b| f.ins().fadd(a, b))?,
                0x67 => lng(&mut fb, &mut stack, |f, a, b| f.ins().fsub(a, b))?,
                0x6b => lng(&mut fb, &mut stack, |f, a, b| f.ins().fmul(a, b))?,
                0x6f => lng(&mut fb, &mut stack, |f, a, b| f.ins().fdiv(a, b))?,
                0x73 => {
                    // drem：cranelift 没有 frem 指令，调 CRT 的 fmod
                    let b = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let fref = self.module.declare_func_in_func(drem_id, fb.func);
                    let c = fb.ins().call(fref, &[a, b]);
                    stack.push(fb.inst_results(c)[0]);
                }
                0x77 => {
                    let b = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    stack.push(fb.ins().fneg(b));
                }
                // 数值类型转换
                0x87 => {
                    // i2d
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    stack.push(fb.ins().fcvt_from_sint(types::F64, a));
                }
                0x8a => {
                    // l2d
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    stack.push(fb.ins().fcvt_from_sint(types::F64, a));
                }
                0x8e => {
                    // d2i
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    stack.push(fb.ins().fcvt_to_sint_sat(types::I32, a));
                }
                0x8f => {
                    // d2l
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    stack.push(fb.ins().fcvt_to_sint_sat(types::I64, a));
                }
                // long / double 比较：压出 -1 / 0 / 1
                0x94 => {
                    // lcmp
                    let b = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let lt = fb.ins().icmp(IntCC::SignedLessThan, a, b);
                    let gt = fb.ins().icmp(IntCC::SignedGreaterThan, a, b);
                    let lt = fb.ins().uextend(types::I32, lt);
                    let gt = fb.ins().uextend(types::I32, gt);
                    let d = fb.ins().isub(gt, lt);
                    stack.push(d);
                }
                0x97 | 0x98 => {
                    // dcmpl(0x97: NaN → -1) / dcmpg(0x98: NaN → +1)
                    let b = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let (cc_lt, cc_gt) = if op == 0x97 {
                        (FloatCC::UnorderedOrLessThan, FloatCC::GreaterThan)
                    } else {
                        (FloatCC::LessThan, FloatCC::UnorderedOrGreaterThan)
                    };
                    let lt = fb.ins().fcmp(cc_lt, a, b);
                    let gt = fb.ins().fcmp(cc_gt, a, b);
                    let lt = fb.ins().uextend(types::I32, lt);
                    let gt = fb.ins().uextend(types::I32, gt);
                    let d = fb.ins().isub(gt, lt);
                    stack.push(d);
                }
                0x88 => {
                    // l2i
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    stack.push(fb.ins().ireduce(types::I32, a));
                }
                0x85 => {
                    // i2l
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    stack.push(fb.ins().sextend(types::I64, a));
                }
                // getstatic：静态字段（System.out 仍是 PrintStream 占位）
                0xb2 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let (owner, fname, fdesc) = field_ref_of(&cf, idx)?;
                    if owner == "java/lang/System" && fname == "out" {
                        let z = fb.ins().iconst(types::I64, 0);
                        stack.push(z);
                    } else {
                        self.emit_clinit_call(&mut fb, &owner)?;
                        let slot = rt::static_slot(&owner, &fname);
                        let (_, want) = field_slot_type(&fdesc);
                        let mut sig = self.module.make_signature();
                        sig.params.push(AbiParam::new(types::I32));
                        sig.returns.push(AbiParam::new(types::I64));
                        let fid = self
                            .module
                            .declare_function("vmless_get_static", Linkage::Import, &sig)
                            .map_err(|e| JitError(e.to_string()))?;
                        let fref = self.module.declare_func_in_func(fid, fb.func);
                        let si = fb.ins().iconst(types::I32, slot as i64);
                        let c = fb.ins().call(fref, &[si]);
                        let raw = fb.inst_results(c)[0];
                        let v = if want == types::F64 {
                            fb.ins().bitcast(types::F64, MemFlags::new(), raw)
                        } else {
                            convert_to(&mut fb, raw, want)
                        };
                        stack.push(v);
                    }
                }
                // putstatic
                0xb3 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let (owner, fname, fdesc) = field_ref_of(&cf, idx)?;
                    self.emit_clinit_call(&mut fb, &owner)?;
                    let slot = rt::static_slot(&owner, &fname);
                    let (ty, _) = field_slot_type(&fdesc);
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    // 统一按 i64 位模式存
                    let v = if ty == types::F64 {
                        let b = fb.ins().bitcast(types::I64, MemFlags::new(), v);
                        b
                    } else {
                        convert_to(&mut fb, v, types::I64)
                    };
                    let mut sig = self.module.make_signature();
                    sig.params.push(AbiParam::new(types::I32));
                    sig.params.push(AbiParam::new(types::I64));
                    let fid = self
                        .module
                        .declare_function("vmless_put_static", Linkage::Import, &sig)
                        .map_err(|e| JitError(e.to_string()))?;
                    let fref = self.module.declare_func_in_func(fid, fb.func);
                    let si = fb.ins().iconst(types::I32, slot as i64);
                    fb.ins().call(fref, &[si, v]);
                }
                // invokevirtual：println 走运行时；本类方法走静态分派
                0xb6 | 0xb9 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    if op == 0xb9 {
                        pc += 2; // count + 保留字节（invokeinterface 比 invokevirtual 多两字节）
                    }
                    let (cls, name, desc) = method_ref_of(&cf, idx)?;
                    if name == "println" {
                        // 栈上顺序是 [接收者, 参数]，先弹参数
                        let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                        let _recv = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                        // 按重载选运行时函数：(I)V / (J)V / (D)V / (Ljava/lang/String;)V
                        let (fid, want) = match desc.as_str() {
                            "(J)V" => (println_long_id, types::I64),
                            "(D)V" | "(F)V" => (println_double_id, types::F64),
                            "(Z)V" => (println_bool_id, types::I32),
                            "(Ljava/lang/String;)V" | "(Ljava/lang/Object;)V" => {
                                (println_obj_id, types::I64)
                            }
                            _ => (println_int_id, types::I32),
                        };
                        let v = convert_to(&mut fb, v, want);
                        let fref = self.module.declare_func_in_func(fid, fb.func);
                        fb.ins().call(fref, &[v]);
                    } else {
                        // 虚方法：按**运行时类**分派。编译期把所有可能是接收者的类
                        // （静态类型 C 及其子类）的实现都编出来并登记进运行时表，
                        // 调用点只发一次 `vcall(locals, sig)` —— 这就是一张内联缓存
                        // 之前的方法表查找：多一次间接跳转，但没有分支预测失误。
                        let sig = rt::intern_sig(&name, &desc);
                        let argc = argc_of(&desc) + 1;
                        let mut args = Vec::new();
                        for _ in 0..argc {
                            args.push(stack.pop().ok_or_else(|| JitError("栈下溢".into()))?);
                        }
                        args.reverse();
                        // 被调方法的 locals 布局：槽 0 是 this，参数从槽 1 起按描述符排
                        let ml = self.vcall_max_locals(&cls, &name, &desc)?;
                        let slot = fb.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
                            cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
                            (ml.max(1) * 8) as u32,
                            3,
                        ));
                        let mut pslots = vec![0usize];
                        pslots.extend(param_slots(&desc).iter().map(|s| s + 1));
                        for (i, v) in args.iter().enumerate() {
                            let off = (pslots.get(i).copied().unwrap_or(i) * 8) as i32;
                            let addr = fb.ins().stack_addr(types::I64, slot, off);
                            fb.ins().store(MemFlags::new(), *v, addr, 0);
                        }
                        let ptr = fb.ins().stack_addr(types::I64, slot, 0);
                        let key = fb.ins().iconst(types::I32, sig as i64);
                        let fref = self.module.declare_func_in_func(vcall_id, fb.func);
                        let c = fb.ins().call(fref, &[ptr, key]);
                        if !desc.ends_with(")V") {
                            // vcall 统一按 i64 回来，按真实返回类型转一次
                            let raw = fb.inst_results(c)[0];
                            let want = ret_type(&desc);
                            let v = if want == types::F64 {
                                fb.ins().bitcast(types::F64, MemFlags::new(), raw)
                            } else {
                                convert_to(&mut fb, raw, want)
                            };
                            stack.push(v);
                        }
                        if uses_exc {
                            let op_pc = pc - if op == 0xb9 { 5 } else { 3 };
                            let cont = fb.create_block();
                            let disp = fb.create_block();
                            let pf = self.module.declare_func_in_func(helpers.pending, fb.func);
                            let pcx = fb.ins().call(pf, &[]);
                            let flag = fb.inst_results(pcx)[0];
                            fb.ins().brif(flag, disp, &[], cont, &[]);
                            fb.switch_to_block(disp);
                            self.emit_dispatch(&mut fb, &cf, &exc_table, &blocks, exc_slot, &helpers, op_pc, ret_ty)?;
                            fb.switch_to_block(cont);
                        }
                    }
                }
                // new：分配对象（8 字节头 + 每字段 8 字节）
                0xbb => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let cls = match cf.constant_pool.get(idx as usize).and_then(|e| e.as_ref()) {
                        Some(CpEntry::Class(n)) => {
                            String::from_utf8_lossy(cf.utf8(*n).unwrap_or_default()).into_owned()
                        }
                        other => return Err(JitError(format!("new 的操作数不是类: {other:?}"))),
                    };
                    let tag = rt::class_tag(&cls);
                    // 首次 new 这个类时先跑它的 <clinit>
                    self.emit_clinit_call(&mut fb, &cls)?;
                    // 字段个数按**被创建的那个类**的布局（含父类字段）算，不是本类的
                    let nf = fb.ins().iconst(types::I32, self.classes.field_count(&cls) as i64);
                    let tg = fb.ins().iconst(types::I32, tag as i64);
                    let fref = self.module.declare_func_in_func(new_obj_id, fb.func);
                    let c = fb.ins().call(fref, &[nf, tg]);
                    let p = fb.inst_results(c)[0];
                    stack.push(p);
                }
                // getfield（偏移 = 8 + 8*下标；类型按字段描述符选）
                0xb4 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let (owner, name, desc) = field_ref_of(&cf, idx)?;
                    // 偏移沿引用类的继承链算（父类字段在前，子类看到的偏移与父类一致）
                    let f = self
                        .classes
                        .field_offset(&owner, &name)
                        .ok_or_else(|| JitError(format!("未知字段 {owner}.{name}")))?;
                    let p = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let off = (8 + 8 * f) as i32;
                    let (ty, want) = field_slot_type(&desc);
                    let v = fb.ins().load(ty, MemFlags::new(), p, off);
                    stack.push(convert_to(&mut fb, v, want));
                }
                // putfield
                0xb5 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let (owner, name, desc) = field_ref_of(&cf, idx)?;
                    let f = self
                        .classes
                        .field_offset(&owner, &name)
                        .ok_or_else(|| JitError(format!("未知字段 {owner}.{name}")))?;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let p = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let (ty, want) = field_slot_type(&desc);
                    let v = convert_to(&mut fb, v, want);
                    let _ = ty;
                    fb.ins().store(MemFlags::new(), v, p, (8 + 8 * f) as i32);
                }
                // newarray：atype → (元素描述符, 元素字节数)
                0xbc => {
                    let atype = body[pc];
                    pc += 1;
                    let (desc, esize) = match atype {
                        11 => ("[J", 8),
                        7 => ("[D", 8),
                        6 => ("[F", 8),
                        4 => ("[Z", 4),
                        5 => ("[C", 4),
                        8 => ("[B", 4),
                        9 => ("[S", 4),
                        _ => ("[I", 4),
                    };
                    push_new_array(&mut fb, &mut stack, &mut self.module, new_arr_id, esize, desc)?;
                }
                // multianewarray：长度先落到一个栈槽里，再交给运行时递归建
                0xc5 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let ndims = body[pc] as usize;
                    pc += 1;
                    let desc = match cf.constant_pool.get(idx as usize).and_then(|e| e.as_ref()) {
                        Some(CpEntry::Class(n)) => {
                            String::from_utf8_lossy(cf.utf8(*n).unwrap_or_default()).into_owned()
                        }
                        other => {
                            return Err(JitError(format!("multianewarray 的操作数不是类: {other:?}")))
                        }
                    };
                    let mut counts = Vec::with_capacity(ndims);
                    for _ in 0..ndims {
                        counts.push(stack.pop().ok_or_else(|| JitError("栈下溢".into()))?);
                    }
                    counts.reverse();
                    let slot = fb.create_sized_stack_slot(
                        cranelift_codegen::ir::StackSlotData::new(
                            cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
                            (ndims.max(1) * 4) as u32,
                            2,
                        ),
                    );
                    for (i, v) in counts.iter().enumerate() {
                        let v = convert_to(&mut fb, *v, types::I32);
                        let addr = fb.ins().stack_addr(types::I64, slot, (i * 4) as i32);
                        fb.ins().store(MemFlags::new(), v, addr, 0);
                    }
                    let ptr = fb.ins().stack_addr(types::I64, slot, 0);
                    let nd = fb.ins().iconst(types::I32, ndims as i64);
                    // 描述符串：编译期登记，运行时取稳定指针
                    let sidx = strtab::register(&desc);
                    let sref = self.module.declare_func_in_func(str_const_id, fb.func);
                    let si = fb.ins().iconst(types::I32, sidx as i64);
                    let sc = fb.ins().call(sref, &[si]);
                    let sp = fb.inst_results(sc)[0];
                    let fref = self.module.declare_func_in_func(new_multi_id, fb.func);
                    let c = fb.ins().call(fref, &[ptr, nd, sp]);
                    stack.push(fb.inst_results(c)[0]);
                }
                // anewarray：操作数是元素类 → 数组描述符 [Lxxx; 或 [[xxx
                0xbd => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let comp = match cf.constant_pool.get(idx as usize).and_then(|e| e.as_ref()) {
                        Some(CpEntry::Class(n)) => {
                            String::from_utf8_lossy(cf.utf8(*n).unwrap_or_default()).into_owned()
                        }
                        other => return Err(JitError(format!("anewarray 的操作数不是类: {other:?}"))),
                    };
                    let desc = if comp.starts_with('[') {
                        format!("[{comp}")
                    } else {
                        format!("[L{comp};")
                    };
                    push_new_array(&mut fb, &mut stack, &mut self.module, new_arr_id, 8, &desc)?;
                }
                // arraylength（长度在偏移 0）
                0xbe => {
                    let p = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let v = fb.ins().load(types::I32, MemFlags::new(), p, 0);
                    stack.push(v);
                }
                // iaload / laload / faload / daload / aaload：元素宽度按指令选
                0x2e | 0x2f | 0x30 | 0x31 | 0x32 => {
                    let esize: i64 = if matches!(op, 0x2e) { 4 } else { 8 };
                    let ty = match op {
                        0x2f => types::I64,
                        0x30 | 0x31 => types::F64,
                        0x32 => types::I64,
                        _ => types::I32,
                    };
                    let idx = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let p = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let addr = elem_addr(&mut fb, p, idx, esize)?;
                    let v = fb.ins().load(ty, MemFlags::new(), addr, 0);
                    stack.push(v);
                }
                // iastore / lastore / fastore / dastore / aastore
                0x4f | 0x50 | 0x51 | 0x52 | 0x53 => {
                    let esize: i64 = if matches!(op, 0x4f) { 4 } else { 8 };
                    let want = match op {
                        0x50 => types::I64,
                        0x51 | 0x52 => types::F64,
                        0x53 => types::I64,
                        _ => types::I32,
                    };
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let idx = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let p = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let v = convert_to(&mut fb, v, want);
                    let addr = elem_addr(&mut fb, p, idx, esize)?;
                    fb.ins().store(MemFlags::new(), v, addr, 0);
                }
                // instanceof / checkcast：走运行时的类型标签比较
                0xc1 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let tag = class_const_tag(&cf, idx)?;
                    let obj = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let obj = convert_to(&mut fb, obj, types::I64);
                    let tg = fb.ins().iconst(types::I32, tag as i64);
                    let fref = self.module.declare_func_in_func(instanceof_id, fb.func);
                    let c = fb.ins().call(fref, &[obj, tg]);
                    stack.push(fb.inst_results(c)[0]);
                }
                0xc0 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let tag = class_const_tag(&cf, idx)?;
                    let obj = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let obj = convert_to(&mut fb, obj, types::I64);
                    let tg = fb.ins().iconst(types::I32, tag as i64);
                    let fref = self.module.declare_func_in_func(checkcast_id, fb.func);
                    let c = fb.ins().call(fref, &[obj, tg]);
                    stack.push(fb.inst_results(c)[0]);
                }
                // 移位：int 版本两个操作数都是 I32；long 版本的位数由 I32 扩展成 I64
                0x78 | 0x7a | 0x7c => {
                    let b = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let b = convert_to(&mut fb, b, types::I32);
                    let a = convert_to(&mut fb, a, types::I32);
                    let v = match op {
                        0x78 => fb.ins().ishl(a, b),  // ishl
                        0x7a => fb.ins().sshr(a, b),  // ishr
                        _ => fb.ins().ushr(a, b),     // iushr
                    };
                    stack.push(v);
                }
                0x79 | 0x7b | 0x7d => {
                    let b = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let b32 = convert_to(&mut fb, b, types::I32);
                    let b = fb.ins().sextend(types::I64, b32);
                    let a = convert_to(&mut fb, a, types::I64);
                    let v = match op {
                        0x79 => fb.ins().ishl(a, b), // lshl
                        0x7b => fb.ins().sshr(a, b), // lshr
                        _ => fb.ins().ushr(a, b),    // lushr
                    };
                    stack.push(v);
                }
                // 位运算
                0x7e | 0x80 | 0x82 => {
                    let b = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let a = convert_to(&mut fb, a, types::I32);
                    let b = convert_to(&mut fb, b, types::I32);
                    let v = match op {
                        0x7e => fb.ins().band(a, b),
                        0x80 => fb.ins().bor(a, b),
                        _ => fb.ins().bxor(a, b),
                    };
                    stack.push(v);
                }
                0x7f | 0x81 | 0x83 => {
                    let b = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let a = convert_to(&mut fb, a, types::I64);
                    let b = convert_to(&mut fb, b, types::I64);
                    let v = match op {
                        0x7f => fb.ins().band(a, b),
                        0x81 => fb.ins().bor(a, b),
                        _ => fb.ins().bxor(a, b),
                    };
                    stack.push(v);
                }
                other => return Err(JitError(format!("JIT 暂不支持字节码 0x{other:02x}"))),
            }
        }
        // 所有还没终结的块统一补该返回类型的零值
        for b in open {
            fb.switch_to_block(b);
            let z = zero_of(&mut fb, ret_ty);
            fb.ins().return_(&[z]);
        }
        let _ = returns;

        fb.seal_all_blocks();
        fb.finalize();

        if std::env::var_os("JIT_DUMP").is_some() {
            eprintln!("--- bytecode ---
{:02x?}", body);
            eprintln!("--- ir ---
{}", ctx.func.display());
        }
        self.module
            .define_function(func_id, &mut ctx)
            .map_err(|e| JitError(format!("{e:?}")))?;
        self.module.clear_context(&mut ctx);
        let argc = argc_of(desc);
        self.compiled.insert(
            (class.to_string(), name.to_string(), desc.to_string()),
            (func_id, max_locals, argc),
        );
        Ok((func_id, max_locals, argc))
    }

    /// 这个类自己有没有 `<clinit>`
    fn has_clinit(&self, class: &str) -> bool {
        let Some(cf) = self.classes.get(class) else { return false };
        cf.methods.iter().any(|m| {
            String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default()) == "<clinit>"
                && String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default()) == "()V"
        })
    }

    /// 确保 `class` 的 `<clinit>` 已经编成机器码并登记（返回是否真的有）
    pub(super) fn ensure_clinit(&mut self, class: &str) -> Result<bool, JitError> {
        if !self.has_clinit(class) {
            return Ok(false);
        }
        let tag = rt::class_tag(class);
        if !self.clinit_started.insert(tag) {
            return Ok(true); // 已经在编（或编过）
        }
        let (fid, _, _) = self.compile_into(class, "<clinit>", "()V")?;
        self.pending_clinit.push((tag, fid));
        Ok(true)
    }

    /// 在当前位置插一次 `vmless_clinit(tag)`（只在类真有 `<clinit>` 时）
    fn emit_clinit_call(
        &mut self,
        fb: &mut FunctionBuilder,
        class: &str,
    ) -> Result<(), JitError> {
        if !self.ensure_clinit(class)? {
            return Ok(());
        }
        let tag = rt::class_tag(class);
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I32));
        let fid = self
            .module
            .declare_function("vmless_clinit", Linkage::Import, &sig)
            .map_err(|e| JitError(e.to_string()))?;
        let fref = self.module.declare_func_in_func(fid, fb.func);
        let t = fb.ins().iconst(types::I32, tag as i64);
        fb.ins().call(fref, &[t]);
        Ok(())
    }

    /// 准备一个虚调用点：
    ///
    /// 1. 把「静态类型 `static_class` 及其所有子类」对该方法的实现都编译出来
    /// 2. 登记到运行时表 `(类的类型标签, 签名) → 函数地址`
    /// 3. 返回被调方法需要的 locals 槽数（各实现里最大的那个）
    ///
    /// 这样就得到一张**按类标签的完整方法表** —— 调用点一次查表 + 间接调用，
    /// 没有逐帧的字符串比较。
    fn vcall_max_locals(&mut self, static_class: &str, name: &str, desc: &str) -> Result<usize, JitError> {
        let sig = rt::intern_sig(name, desc);
        let rty = ret_type(desc);
        let ret_kind = if rty == types::F64 {
            rt::RET_F64
        } else if rty == types::I64 {
            rt::RET_I64
        } else {
            rt::RET_I32
        };
        let mut ml_max = 1usize;
        let mut names = self.classes.names();
        names.sort();
        for k in names {
            if !self.classes.is_subclass(&k, static_class) {
                continue;
            }
            let Some((owner, mi)) = self.classes.resolve_method(&k, name, desc) else { continue };
            // 接口里的抽象方法声明没有 Code：跳过（实现类的那份会被编到）
            if self.classes.is_abstract_method(&owner, mi) {
                continue;
            }
            let (fid, ml, _) = self.compile_into(&owner, name, desc)?;
            ml_max = ml_max.max(ml);
            let tag = rt::class_tag(&k);
            if !self.pending_v.iter().any(|(t, s, _, _)| *t == tag && *s == sig) {
                self.pending_v.push((tag, sig, fid, ret_kind));
            }
        }
        Ok(ml_max)
    }
}

fn bin(
    fb: &mut FunctionBuilder,
    stack: &mut Vec<Value>,
    f: impl FnOnce(&mut FunctionBuilder, Value, Value) -> Value,
) -> Result<(), JitError> {
    let b = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
    let v = f(fb, a, b);
    stack.push(v);
    Ok(())
}

fn const_int(cf: &ClassFile, idx: u16) -> i64 {
    match cf.constant_pool.get(idx as usize).and_then(|e| e.as_ref()) {
        Some(CpEntry::Integer(v)) => *v as i32 as i64,
        _ => 0,
    }
}

/// 二元 long 运算
fn lng(
    fb: &mut FunctionBuilder,
    stack: &mut Vec<Value>,
    f: impl FnOnce(&mut FunctionBuilder, Value, Value) -> Value,
) -> Result<(), JitError> {
    let b = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
    let v = f(fb, a, b);
    stack.push(v);
    Ok(())
}
