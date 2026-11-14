//! 方法级 JIT：JVM 字节码 → Cranelift IR → 本地机器码。
//!
//! 覆盖范围（够跑我们自己编译器产出的 int 方法）：
//! 常量、iload/istore、(iinc)、算术、比较跳转、goto、ireturn/return。
//! 操作数栈用 Cranelift 的 SSA 值表示；局部变量放在一块内存里，
//! 由调用方传入指针（与解释器共用同一份 locals 布局）。

use crate::{parse, ClassFile, CpEntry, Member};
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{types, AbiParam, InstBuilder, MemFlags, Signature, Value};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Linkage, Module};

pub struct Jit {
    module: JITModule,
    /// (方法名, 描述符) → 已声明/已编译的函数 id
    compiled: std::collections::HashMap<(String, String), (cranelift_module::FuncId, usize, i32)>,
}

#[derive(Debug)]
pub struct JitError(pub String);

type JitFn = unsafe extern "C" fn(*mut i64) -> i32;

pub struct CompiledMethod {
    pub name: String,
    pub desc: String,
    pub code: JitFn,
    pub max_locals: usize,
}

impl Jit {
    pub fn new() -> Self {
        let mut flag_builder = settings::builder();
        flag_builder.set("use_colocated_libcalls", "false").unwrap();
        flag_builder.set("is_pic", "false").unwrap();
        let isa_builder = cranelift_native::builder().expect("native isa");
        let isa = isa_builder
            .finish(settings::Flags::new(flag_builder))
            .expect("isa");
        let mut builder = JITBuilder::with_isa(isa, default_libcall_names());
        builder.symbol("vmless_new_object", rt::new_object as *const u8);
        builder.symbol("vmless_new_array", rt::new_array as *const u8);
        builder.symbol("vmless_println_int", rt::println_int as *const u8);
        builder.symbol("vmless_println_str", rt::println_str as *const u8);
        builder.symbol("vmless_str_const", strtab::str_const as *const u8);
        builder.symbol("vmless_println_obj", strtab::println_str as *const u8);
        builder.symbol("vmless_drem", rt::drem as *const u8);
        let module = JITModule::new(builder);
        Jit { module, compiled: std::collections::HashMap::new() }
    }

    /// 编译（或复用）一个静态方法，返回 (函数 id, max_locals, 参数个数)
    pub fn compile_into(
        &mut self,
        cf: &ClassFile,
        name: &str,
        desc: &str,
    ) -> Result<(cranelift_module::FuncId, usize, i32), JitError> {
        if let Some(v) = self.compiled.get(&(name.to_string(), desc.to_string())) {
            return Ok(*v);
        }
        let m = cf
            .methods
            .iter()
            .find(|m| {
                let n = String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default());
                let d = String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default());
                n == name && d == desc
            })
            .ok_or_else(|| JitError(format!("找不到方法 {name}{desc}")))?;
        let (max_locals, body) = code_of(cf, m)?;
        let self_class_name = cf.this_class_name().unwrap_or_default();

        // 实例字段名 → 下标
        let mut fields: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut field_count = 0usize;
        for f in &cf.fields {
            if f.access_flags & 0x0008 != 0 {
                continue;
            }
            let n = String::from_utf8_lossy(cf.utf8(f.name_index).unwrap_or_default()).into_owned();
            fields.insert(n, field_count);
            field_count += 1;
        }

        // 方法签名：(locals_ptr: i64) -> i32
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I32));

        let func_id = self
            .module
            .declare_function(&format!("{name}_{}", desc.replace(['(', ')'], "")), Linkage::Export, &sig)
            .map_err(|e| JitError(e.to_string()))?;
        // 先登记，递归调用（fib 调 fib）才能命中缓存而不是无限递归
        self.compiled.insert(
            (name.to_string(), desc.to_string()),
            (func_id, max_locals, argc_of(desc)),
        );

        let mut ctx = self.module.make_context();
        ctx.func.signature = sig.clone();
        // 声明运行时辅助函数
        let mut sig_new_obj = self.module.make_signature();
        sig_new_obj.params.push(AbiParam::new(types::I32));
        sig_new_obj.returns.push(AbiParam::new(types::I64));
        let new_obj_id = self
            .module
            .declare_function("vmless_new_object", Linkage::Import, &sig_new_obj)
            .map_err(|e| JitError(e.to_string()))?;
        let mut sig_new_arr = self.module.make_signature();
        sig_new_arr.params.push(AbiParam::new(types::I32));
        sig_new_arr.returns.push(AbiParam::new(types::I64));
        let new_arr_id = self
            .module
            .declare_function("vmless_new_array", Linkage::Import, &sig_new_arr)
            .map_err(|e| JitError(e.to_string()))?;

        let mut sig_pi = self.module.make_signature();
        sig_pi.params.push(AbiParam::new(types::I32));
        let println_int_id = self
            .module
            .declare_function("vmless_println_int", Linkage::Import, &sig_pi)
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

        let mut bctx = FunctionBuilderContext::new();
        let mut fb = FunctionBuilder::new(&mut ctx.func, &mut bctx);

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
                0x99..=0xa4 | 0xa7 => {
                    let off = i16::from_be_bytes([body[pc + 1], body[pc + 2]]) as isize;
                    targets.push((pc as isize + off) as usize);
                    // 分支之后的指令也是基本块开头（即使当前不可达）
                    targets.push(pc + 3);
                    pc += 3;
                }
                0x10 => pc += 2,
                0x11 | 0x13 | 0x84 | 0xb2 | 0xb4 | 0xb5 | 0xb6 | 0xb7 | 0xb8 | 0xbb => pc += 3,
                0x12 => pc += 2,
                0xbc | 0x19 | 0x59 => pc += 2,
                _ => pc += insn_len(op),
            }
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
                    if let Ok(info) = method_ref_of(cf, idx) {
                        // 外部类（如 Object.<init>）不编译，调用时当空操作
                        if info.0 != self_class_name {
                            q += len;
                            continue;
                        }
                        let (fid, ml, argc) = self.compile_into(cf, &info.1, &info.2)?;
                        let extra = if op == 0xb7 { 1 } else { 0 };
                        call_targets.insert(idx, (fid, ml, argc + extra));
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

        macro_rules! flush_to {
            ($fb:expr, $target:expr) => {{
                let tb = *blocks.get(&$target).ok_or_else(|| JitError(format!("跳转目标 {target} 无对应块", target = $target)))?;
                if !stack.is_empty() {
                    return Err(JitError("跳转点操作数栈非空，暂不支持".into()));
                }
                if !terminated {
                    $fb.ins().jump(tb, &[]);
                }
                terminated = false;
                open.pop();
                open.push(tb);
                cur_block = tb;
                cur_start = $target;
                $fb.switch_to_block(tb);
                stack.clear();
            }};
        }

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

            let op = body[pc];
            pc += 1;
            match op {
                0x00 => {}
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
                        stack.push(fb.ins().iconst(types::I32, const_int(cf, idx)));
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
                        stack.push(fb.ins().iconst(types::I32, const_int(cf, idx)));
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
                0x57 => {
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
                    terminated = true;
                    open.pop();
                    open.push(fall);
                    fb.switch_to_block(fall);
                    terminated = false;
                    cur_start = pc;
                    cur_start = pc;
                }
                // 二元比较跳转
                0x9f..=0xa4 => {
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
                        _ => IntCC::SignedLessThanOrEqual,
                    };
                    let tb = *blocks.get(&target).ok_or_else(|| JitError("缺少目标块".into()))?;
                    let fall = *blocks.get(&pc).ok_or_else(|| JitError("缺少落空块".into()))?;
                    let cond = fb.ins().icmp(cc, a, b);
                    fb.ins().brif(cond, tb, &[], fall, &[]);
                    terminated = true;
                    open.pop();
                    open.push(fall);
                    fb.switch_to_block(fall);
                    terminated = false;
                    cur_start = pc;
                    cur_start = pc;
                }
                0xa7 => {
                    let off = i16::from_be_bytes([body[pc], body[pc + 1]]) as isize;
                    pc += 2;
                    let target = (pc as isize - 3 + off) as usize;
                    let tb = *blocks.get(&target).ok_or_else(|| JitError("缺少目标块".into()))?;
                    fb.ins().jump(tb, &[]);
                    terminated = true;
                    open.pop();
                    let fall = *blocks.get(&pc).ok_or_else(|| JitError("缺少落空块".into()))?;
                    fb.switch_to_block(fall);
                    open.push(fall);
                    terminated = false;
                    cur_start = pc;
                }
                0xac => {
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
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
                    let zero = fb.ins().iconst(types::I32, 0);
                    fb.ins().return_(&[zero]);
                    returns += 1;
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
                    let callee_desc = method_ref_of(cf, idx)?.2;
                    let pslots = param_slots(&callee_desc);
                    for (i, v) in args.iter().enumerate() {
                        let off = (pslots.get(i).copied().unwrap_or(i) * 8) as i32;
                        let addr = fb.ins().stack_addr(types::I64, slot, off);
                        fb.ins().store(MemFlags::new(), *v, addr, 0);
                    }
                    let ptr = fb.ins().stack_addr(types::I64, slot, 0);
                    let fref = self.module.declare_func_in_func(fid, fb.func);
                    let call = fb.ins().call(fref, &[ptr]);
                    let res = fb.inst_results(call)[0];
                    // JIT 签名统一返回 i32，按被调方的真实返回类型还原
                    let res = widen_ret(&mut fb, res, &callee_desc);
                    stack.push(res);
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
                        // 外部构造器（如 Object.<init>）：只把接收者和参数丢掉
                        let desc = method_ref_of(cf, idx)?.2;
                        for _ in 0..(argc_of(&desc) + 1) {
                            stack.pop();
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
                    pslots.extend(param_slots(&method_ref_of(cf, idx)?.2).iter().map(|s| s + 1));
                    for (i, v) in args.iter().enumerate() {
                        let off = (pslots.get(i).copied().unwrap_or(i) * 8) as i32;
                        let addr = fb.ins().stack_addr(types::I64, slot, off);
                        fb.ins().store(MemFlags::new(), *v, addr, 0);
                    }
                    let ptr = fb.ins().stack_addr(types::I64, slot, 0);
                    let fref = self.module.declare_func_in_func(fid, fb.func);
                    let c = fb.ins().call(fref, &[ptr]);
                    let _ = c;
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
                0x38 => {
                    // dstore
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
                    stack.push(fb.ins().fcvt_to_sint(types::I32, a));
                }
                0x8f => {
                    // d2l
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    stack.push(fb.ins().fcvt_to_sint(types::I64, a));
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
                0xaf => {
                    // dreturn：JIT 签名统一返回 i32，double 截断成 int
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let v32 = fb.ins().fcvt_to_sint(types::I32, v);
                    fb.ins().return_(&[v32]);
                    terminated = true;
                    open.pop();
                    let dead = fb.create_block();
                    open.push(dead);
                    fb.switch_to_block(dead);
                    stack.clear();
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
                0xad => {
                    // lreturn
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let v32 = fb.ins().ireduce(types::I32, v);
                    fb.ins().return_(&[v32]);
                    terminated = true;
                    open.pop();
                    let dead = fb.create_block();
                    open.push(dead);
                    fb.switch_to_block(dead);
                    stack.clear();
                }
                // getstatic：System.out 只当占位
                0xb2 => {
                    let _idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let z = fb.ins().iconst(types::I64, 0);
                    stack.push(z);
                }
                // invokevirtual：println 走运行时；本类方法走静态分派
                0xb6 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let (cls, name, desc) = method_ref_of(cf, idx)?;
                    if name == "println" {
                        // 栈上顺序是 [接收者, 参数]，先弹参数
                        let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                        let _recv = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                        if desc.contains("String") {
                            let fref = self.module.declare_func_in_func(println_obj_id, fb.func);
                            fb.ins().call(fref, &[v]);
                            continue;
                        }
                        let fref = self.module.declare_func_in_func(println_int_id, fb.func);
                        fb.ins().call(fref, &[v]);
                    } else {
                        let _ = cls;
                        let argc = argc_of(&desc) + 1;
                        let (fid, ml, _) = self.compile_into(cf, &name, &desc)?;
                        let mut args = Vec::new();
                        for _ in 0..argc {
                            args.push(stack.pop().ok_or_else(|| JitError("栈下溢".into()))?);
                        }
                        args.reverse();
                        let slot = fb.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
                            cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
                            (ml.max(1) * 8) as u32,
                            3,
                        ));
                        // 槽 0 是 this，参数从槽 1 起按描述符排
                        let mut pslots = vec![0usize];
                        pslots.extend(param_slots(&desc).iter().map(|s| s + 1));
                        for (i, v) in args.iter().enumerate() {
                            let off = (pslots.get(i).copied().unwrap_or(i) * 8) as i32;
                            let addr = fb.ins().stack_addr(types::I64, slot, off);
                            fb.ins().store(MemFlags::new(), *v, addr, 0);
                        }
                        let ptr = fb.ins().stack_addr(types::I64, slot, 0);
                        let fref = self.module.declare_func_in_func(fid, fb.func);
                        let c = fb.ins().call(fref, &[ptr]);
                        if !desc.ends_with(")V") {
                            let r = fb.inst_results(c)[0];
                            let r = widen_ret(&mut fb, r, &desc);
                            stack.push(r);
                        }
                    }
                }
                // new：分配对象（字段数来自本类）
                0xbb => {
                    let _idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let nf = fb.ins().iconst(types::I32, field_count as i64);
                    let fref = self.module.declare_func_in_func(new_obj_id, fb.func);
                    let c = fb.ins().call(fref, &[nf]);
                    let p = fb.inst_results(c)[0];
                    stack.push(p);
                }
                // getfield（偏移 = 8 + 4*下标）
                0xb4 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let name = field_ref_of(cf, idx)?.1;
                    let f = *fields.get(&name).ok_or_else(|| JitError(format!("未知字段 {name}")))?;
                    let p = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let v = fb.ins().load(types::I32, MemFlags::new(), p, (8 + 4 * f) as i32);
                    stack.push(v);
                }
                // putfield
                0xb5 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let name = field_ref_of(cf, idx)?.1;
                    let f = *fields.get(&name).ok_or_else(|| JitError(format!("未知字段 {name}")))?;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let p = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    fb.ins().store(MemFlags::new(), v, p, (8 + 4 * f) as i32);
                }
                // newarray
                0xbc => {
                    let _atype = body[pc];
                    pc += 1;
                    let n = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let fref = self.module.declare_func_in_func(new_arr_id, fb.func);
                    let c = fb.ins().call(fref, &[n]);
                    let p = fb.inst_results(c)[0];
                    stack.push(p);
                }
                // arraylength
                0xbe => {
                    let p = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let v = fb.ins().load(types::I32, MemFlags::new(), p, 0);
                    stack.push(v);
                }
                // iaload
                0x2e => {
                    let idx = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let p = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let off = fb.ins().uextend(types::I64, idx);
                    let four = fb.ins().iconst(types::I64, 4);
                    let byte_off = fb.ins().imul(off, four);
                    let eight = fb.ins().iconst(types::I64, 8);
                    let addr = fb.ins().iadd(p, byte_off);
                    let addr = fb.ins().iadd(addr, eight);
                    let v = fb.ins().load(types::I32, MemFlags::new(), addr, 0);
                    stack.push(v);
                }
                // iastore
                0x4f => {
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let idx = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let p = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let off = fb.ins().uextend(types::I64, idx);
                    let four = fb.ins().iconst(types::I64, 4);
                    let byte_off = fb.ins().imul(off, four);
                    let eight = fb.ins().iconst(types::I64, 8);
                    let addr = fb.ins().iadd(p, byte_off);
                    let addr = fb.ins().iadd(addr, eight);
                    fb.ins().store(MemFlags::new(), v, addr, 0);
                }
                other => return Err(JitError(format!("JIT 暂不支持字节码 0x{other:02x}"))),
            }
        }
        // 所有还没终结的块统一补 return 0
        for b in open {
            fb.switch_to_block(b);
            let z = fb.ins().iconst(types::I32, 0);
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
        self.compiled
            .insert((name.to_string(), desc.to_string()), (func_id, max_locals, argc));
        Ok((func_id, max_locals, argc))
    }

    /// 编译一个静态方法并拿到可调用的函数指针
    pub fn compile_static(
        &mut self,
        cf: &ClassFile,
        name: &str,
        desc: &str,
    ) -> Result<CompiledMethod, JitError> {
        let (func_id, max_locals, _) = self.compile_into(cf, name, desc)?;
        self.module
            .finalize_definitions()
            .map_err(|e| JitError(e.to_string()))?;
        let ptr = self.module.get_finalized_function(func_id);
        let code: JitFn = unsafe { std::mem::transmute(ptr) };
        Ok(CompiledMethod {
            name: name.to_string(),
            desc: desc.to_string(),
            code,
            max_locals,
        })
    }
}

impl CompiledMethod {
    /// 调用：args 依次写入参数字槽，返回值即方法返回值
    pub fn call(&self, args: &[i64]) -> i32 {
        let slots = param_slots(&self.desc);
        let need = slots.iter().copied().max().map(|m| m + 1).unwrap_or(0);
        let mut locals = vec![0i64; self.max_locals.max(need).max(args.len())];
        for (i, a) in args.iter().enumerate() {
            let s = slots.get(i).copied().unwrap_or(i);
            if s < locals.len() {
                locals[s] = *a;
            }
        }
        unsafe { (self.code)(locals.as_mut_ptr()) }
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

fn code_of(cf: &ClassFile, m: &Member) -> Result<(usize, Vec<u8>), JitError> {
    let code_name = cf
        .constant_pool
        .iter()
        .enumerate()
        .find_map(|(i, e)| match e {
            Some(CpEntry::Utf8(b)) if b == b"Code" => Some(i as u16),
            _ => None,
        })
        .ok_or_else(|| JitError("常量池里没有 Code".into()))?;
    for a in &m.attributes {
        if a.name_index == code_name && a.info.len() >= 8 {
            let max_locals = u16::from_be_bytes([a.info[2], a.info[3]]) as usize;
            let len = u32::from_be_bytes([a.info[4], a.info[5], a.info[6], a.info[7]]) as usize;
            let end = (8 + len).min(a.info.len());
            return Ok((max_locals, a.info[8..end].to_vec()));
        }
    }
    Err(JitError("方法没有 Code".into()))
}

/// 从文件编译并调用（测试/基准用）
pub fn compile_file(path: &str, name: &str, desc: &str) -> Result<(Jit, CompiledMethod), JitError> {
    let bytes = std::fs::read(path).map_err(|e| JitError(e.to_string()))?;
    let cf = parse(&bytes).map_err(|e| JitError(e.to_string()))?;
    let mut jit = Jit::new();
    let m = jit.compile_static(&cf, name, desc)?;
    Ok((jit, m))
}

/// 解析常量池里的方法引用 → (类名, 方法名, 描述符)
fn method_ref_of(cf: &ClassFile, index: u16) -> Result<(String, String, String), JitError> {
    let e = cf
        .constant_pool
        .get(index as usize)
        .and_then(|e| e.as_ref())
        .ok_or_else(|| JitError(format!("常量池索引 {index} 为空")))?;
    let (c, nt) = match e {
        CpEntry::MethodRef(c, n) => (*c, *n),
        CpEntry::InterfaceMethodRef(c, n) => (*c, *n),
        other => return Err(JitError(format!("不是方法引用: {other:?}"))),
    };
    let class = match cf.constant_pool.get(c as usize).and_then(|e| e.as_ref()) {
        Some(CpEntry::Class(n)) => String::from_utf8_lossy(cf.utf8(*n).unwrap_or_default()).into_owned(),
        _ => return Err(JitError("类名解析失败".into())),
    };
    let (name, desc) = match cf.constant_pool.get(nt as usize).and_then(|e| e.as_ref()) {
        Some(CpEntry::NameAndType(n, d)) => (
            String::from_utf8_lossy(cf.utf8(*n).unwrap_or_default()).into_owned(),
            String::from_utf8_lossy(cf.utf8(*d).unwrap_or_default()).into_owned(),
        ),
        _ => return Err(JitError("NameAndType 解析失败".into())),
    };
    Ok((class, name, desc))
}

/// 指令长度（含操作码自身）。扫描跳转目标 / 调用目标时用，
/// 任何带操作数的指令漏算都会让线性扫描错位，所以集中在一处。
/// 不支持变长的 wide(0xc4)。
fn insn_len(op: u8) -> usize {
    match op {
        // 1 字节操作数
        0x10 | 0x12 | 0x15 | 0x16 | 0x17 | 0x18 | 0x19 | 0x36 | 0x37 | 0x38 | 0x39 | 0x3a | 0xa9
        | 0xbc => 2,
        // 2 字节操作数
        0x11 | 0x13 | 0x14 | 0x84 | 0x99..=0xa8 | 0xb2 | 0xb3 | 0xb4 | 0xb5 | 0xb6 | 0xb7 | 0xb8
        | 0xbb | 0xbd | 0xc0 | 0xc1 | 0xc6 | 0xc7 => 3,
        // 3 字节操作数
        0xc5 => 4,
        // invokeinterface / invokedynamic
        0xb9 | 0xba => 5,
        _ => 1,
    }
}

/// JIT 的方法签名统一是 `(i64*) -> i32`，被调方返回 double/long 时
/// 只能拿到截断后的 i32，调用方按描述符还原成 f64/i64 再继续算。
fn widen_ret(fb: &mut FunctionBuilder, v: Value, desc: &str) -> Value {
    match desc.as_bytes().last().copied() {
        Some(b'D') => fb.ins().fcvt_from_sint(types::F64, v),
        Some(b'J') => fb.ins().sextend(types::I64, v),
        _ => v,
    }
}

/// 描述符里每个参数所在的 JVM 槽位（long/double 占两槽；静态方法从 0 起）
fn param_slots(desc: &str) -> Vec<usize> {
    let b = desc.as_bytes();
    let mut i = 1usize;
    let mut slot = 0usize;
    let mut out = Vec::new();
    while i < b.len() && b[i] != b')' {
        out.push(slot);
        match b[i] {
            b'J' | b'D' => {
                slot += 2;
                i += 1;
            }
            b'L' => {
                while i < b.len() && b[i] != b';' {
                    i += 1;
                }
                i += 1;
                slot += 1;
            }
            b'[' => {
                i += 1;
                while i < b.len() && b[i] == b'[' {
                    i += 1;
                }
                if i < b.len() && b[i] == b'L' {
                    while i < b.len() && b[i] != b';' {
                        i += 1;
                    }
                }
                i += 1;
                slot += 1;
            }
            _ => {
                i += 1;
                slot += 1;
            }
        }
    }
    out
}

/// 描述符里的**参数**个数（只数到 `)`，不数返回值）
fn argc_of(desc: &str) -> i32 {
    let b = desc.as_bytes();
    let mut i = 1usize;
    let mut n = 0i32;
    while i < b.len() && b[i] != b')' {
        match b[i] {
            b'[' => {
                i += 1;
                while i < b.len() && (b[i] == b'[') {
                    i += 1;
                }
                if i < b.len() && b[i] == b'L' {
                    while i < b.len() && b[i] != b';' {
                        i += 1;
                    }
                }
                i += 1;
                n += 1;
            }
            b'L' => {
                while i < b.len() && b[i] != b';' {
                    i += 1;
                }
                i += 1;
                n += 1;
            }
            _ => {
                i += 1;
                n += 1;
            }
        }
    }
    n
}

/// JIT 运行时辅助函数（被生成的机器码直接调用）
pub mod rt {
    use std::sync::Mutex;

    const CAP: usize = 256 << 20;
    struct State {
        base: *mut u8,
        used: usize,
    }
    unsafe impl Send for State {}

    static STATE: Mutex<Option<State>> = Mutex::new(None);

    /// 分配一个对象：头部 8 字节（class_id + flags），后面 fields 个 4 字节槽
    pub extern "C" fn new_object(fields: u32) -> *mut u8 {
        unsafe { alloc_zeroed(8 + fields as usize * 4) }
    }

    /// 打印一个 int（对应 System.out.println(int)）
    pub extern "C" fn println_int(v: i32) {
        println!("{v}");
    }

    /// double 取余（drem 没有对应的 cranelift 指令，走 CRT 的 fmod）
    pub extern "C" fn drem(a: f64, b: f64) -> f64 {
        unsafe extern "C" {
            fn fmod(a: f64, b: f64) -> f64;
        }
        unsafe { fmod(a, b) }
    }

    /// 打印一段 UTF-8 文本（指针 + 长度）
    pub extern "C" fn println_str(ptr: *const u8, len: u32) {
        let s = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
        println!("{}", String::from_utf8_lossy(s));
    }

    /// 分配一个数组：前 8 字节存长度，之后 len 个 4 字节元素
    pub extern "C" fn new_array(len: u32) -> *mut u8 {
        let p = unsafe { alloc_zeroed(8 + len as usize * 4) };
        // 头 4 字节存长度（arraylength 读这里）
        unsafe { std::ptr::write_unaligned(p as *mut u32, len) };
        p
    }

    unsafe fn alloc_zeroed(size: usize) -> *mut u8 {
        let mut g = STATE.lock().unwrap();
        if g.is_none() {
            let layout = std::alloc::Layout::from_size_align(CAP, 8).unwrap();
            let p = std::alloc::alloc_zeroed(layout);
            *g = Some(State { base: p, used: 0 });
        }
        let s = g.as_mut().unwrap();
        let off = s.used;
        s.used += (size + 7) & !7;
        assert!(s.used <= CAP, "JIT heap full");
        let p = s.base.add(off);
        std::ptr::write_bytes(p, 0, size);
        p
    }
}

/// 解析常量池里的**字段**引用 → (类名, 字段名, 描述符)
fn field_ref_of(cf: &ClassFile, index: u16) -> Result<(String, String, String), JitError> {
    let e = cf
        .constant_pool
        .get(index as usize)
        .and_then(|e| e.as_ref())
        .ok_or_else(|| JitError(format!("常量池索引 {index} 为空")))?;
    let (c, nt) = match e {
        CpEntry::FieldRef(c, n) => (*c, *n),
        other => return Err(JitError(format!("不是字段引用: {other:?}"))),
    };
    let class = match cf.constant_pool.get(c as usize).and_then(|e| e.as_ref()) {
        Some(CpEntry::Class(n)) => String::from_utf8_lossy(cf.utf8(*n).unwrap_or_default()).into_owned(),
        _ => return Err(JitError("类名解析失败".into())),
    };
    let (name, desc) = match cf.constant_pool.get(nt as usize).and_then(|e| e.as_ref()) {
        Some(CpEntry::NameAndType(n, d)) => (
            String::from_utf8_lossy(cf.utf8(*n).unwrap_or_default()).into_owned(),
            String::from_utf8_lossy(cf.utf8(*d).unwrap_or_default()).into_owned(),
        ),
        _ => return Err(JitError("NameAndType 解析失败".into())),
    };
    Ok((class, name, desc))
}

/// 字符串常量表：JIT 编译期登记，运行期按序号取指针。
pub mod strtab {
    use std::sync::Mutex;

    struct Entry {
        len: u32,
        bytes: Box<[u8]>,
    }
    // 每个条目前面 4 字节是长度，后面是 UTF-8 字节
    static TABLE: Mutex<Vec<Box<[u8]>>> = Mutex::new(Vec::new());

    /// 登记一个字符串，返回序号
    pub fn register(s: &str) -> u32 {
        let mut t = TABLE.lock().unwrap();
        t.push(s.as_bytes().to_vec().into_boxed_slice());
        (t.len() - 1) as u32
    }

    /// 运行时辅助：返回指向「长度 + 内容」缓冲的指针
    pub extern "C" fn str_const(idx: u32) -> *const u8 {
        let t = TABLE.lock().unwrap();
        match t.get(idx as usize) {
            Some(b) => {
                let _ = Entry { len: b.len() as u32, bytes: Box::new([]) };
                // 用 Vec 布局：先写长度再写内容
                let mut buf = Vec::with_capacity(4 + b.len());
                buf.extend_from_slice(&(b.len() as u32).to_ne_bytes());
                buf.extend_from_slice(b);
                let boxed = buf.into_boxed_slice();
                let p = boxed.as_ptr();
                std::mem::forget(boxed);
                p
            }
            None => std::ptr::null(),
        }
    }

    /// 运行时辅助：打印一个「长度 + 内容」的字符串
    pub extern "C" fn println_str(s: *const u8) {
        if s.is_null() {
            println!();
            return;
        }
        unsafe {
            let mut lb = [0u8; 4];
            std::ptr::copy_nonoverlapping(s, lb.as_mut_ptr(), 4);
            let len = u32::from_ne_bytes(lb) as usize;
            let bytes = std::slice::from_raw_parts(s.add(4), len);
            println!("{}", String::from_utf8_lossy(bytes));
        }
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
