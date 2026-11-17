//! 字节码 → Cranelift IR 的主翻译器。

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{types, AbiParam, InstBuilder, MemFlags, Value};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{Linkage, Module};

use crate::{ClassFile, CpEntry};

use super::call::{
    argc_of, code_of, convert_to, method_ref_of, param_descs, param_slots, ret_type, zero_of,
};
use super::exc::{class_uses_exceptions, ExcHelpers};
use super::insn::{insn_len, insn_len_at};
use super::layout::{
    class_const_tag, elem_addr, emit_ref_addr, field_off, field_ref_of, field_slot_idx,
    field_slot_type, is_ref_desc, push_new_array,
};
use super::refmap;
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
        let (max_locals, max_stack, body, exc_table, is_static) = {
            let m = cf
                .methods
                .iter()
                .find(|m| {
                    let n = String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default());
                    let d = String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default());
                    n == name && d == desc
                })
                .ok_or_else(|| JitError(format!("{class} 里找不到方法 {name}{desc}")))?;
            let is_static = m.access_flags & 0x0008 != 0;
            let (ml, ms, body, table) = code_of(&cf, m)?;
            (ml, ms, body, table, is_static)
        };

        // `invokedynamic` 在**进任何分析之前**就挡住：这个方法整体交给解释器执行，
        // 所以不能让同一方法里别的 JIT 限制（比如 switch 汇合点的栈模型）抢先报错 ——
        // 那样 `run_jit` 会看到一个与 indy 无关的失败，降级判据就失效了。
        if let Some(pc) = find_invokedynamic(&body) {
            return Err(JitError(format!(
                "JIT 不支持 invokedynamic（0xba@{pc}）：{class}.{name}{desc} 里的 lambda / \
                 字符串拼接只能由解释器执行（JIT 未接入调用点缓存）"
            )));
        }
        // 同一个道理：`java.lang.invoke.*` 的调用 JIT 也没有接手的地方
        if let Some(m) = find_invoke_api(&cf, &body) {
            return Err(JitError(format!(
                "JIT 不接管 java.lang.invoke（{m}）：{class}.{name}{desc} 只能由解释器执行"
            )));
        }

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
        sig_new_obj.params.push(AbiParam::new(types::I32)); // class_id（字段布局在 LayoutTable 里）
        sig_new_obj.returns.push(AbiParam::new(types::I64)); // 对象引用（堆内偏移）
        let new_obj_id = self
            .module
            .declare_function("vmless_new_object", Linkage::Import, &sig_new_obj)
            .map_err(|e| JitError(e.to_string()))?;
        let mut sig_new_arr = self.module.make_signature();
        sig_new_arr.params.push(AbiParam::new(types::I32)); // class_id（元素宽度在布局表里）
        sig_new_arr.params.push(AbiParam::new(types::I32)); // 长度
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
        let mut sig_pc = self.module.make_signature();
        sig_pc.params.push(AbiParam::new(types::I32));
        let println_char_id = self
            .module
            .declare_function("vmless_println_char", Linkage::Import, &sig_pc)
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

        let mut sig_pf = self.module.make_signature();
        sig_pf.params.push(AbiParam::new(types::F32));
        let println_float_id = self
            .module
            .declare_function("vmless_println_float", Linkage::Import, &sig_pf)
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
        let mut sig_dp = self.module.make_signature();
        sig_dp.params.push(AbiParam::new(types::I32));
        sig_dp.returns.push(AbiParam::new(types::I64));
        let desc_ptr_id = self
            .module
            .declare_function("vmless_desc_ptr", Linkage::Import, &sig_dp)
            .map_err(|e| JitError(e.to_string()))?;

        let mut sig_ps = self.module.make_signature();
        sig_ps.params.push(AbiParam::new(types::I64));
        let println_obj_id = self
            .module
            .declare_function("vmless_println_obj", Linkage::Import, &sig_ps)
            .map_err(|e| JitError(e.to_string()))?;

        let mut sig_se = self.module.make_signature();
        sig_se.params.push(AbiParam::new(types::I64));
        sig_se.params.push(AbiParam::new(types::I64));
        sig_se.returns.push(AbiParam::new(types::I32));
        let str_equals_id = self
            .module
            .declare_function("vmless_str_equals", Linkage::Import, &sig_se)
            .map_err(|e| JitError(e.to_string()))?;

        let mut sig_ff = self.module.make_signature();
        sig_ff.params.push(AbiParam::new(types::F64));
        sig_ff.params.push(AbiParam::new(types::F64));
        sig_ff.returns.push(AbiParam::new(types::F64));
        let drem_id = self
            .module
            .declare_function("vmless_drem", Linkage::Import, &sig_ff)
            .map_err(|e| JitError(e.to_string()))?;

        // frem（0x72）：Cranelift 没有浮点取余指令，走 CRT 的 fmodf（F32 版）
        let mut sig_fff = self.module.make_signature();
        sig_fff.params.push(AbiParam::new(types::F32));
        sig_fff.params.push(AbiParam::new(types::F32));
        sig_fff.returns.push(AbiParam::new(types::F32));
        let frem_id = self
            .module
            .declare_function("vmless_frem", Linkage::Import, &sig_fff)
            .map_err(|e| JitError(e.to_string()))?;

        // 字符串拼接的内建 `java/lang/StringBuilder`（没有 .class 文件）。
        // 对象里存一个 int 缓冲编号，文本本身留在运行时侧的 Vec<Vec<u8>> 里。
        let mut sig_sb_new = self.module.make_signature();
        sig_sb_new.returns.push(AbiParam::new(types::I64));
        let sb_new_id = self
            .module
            .declare_function("vmless_sb_new", Linkage::Import, &sig_sb_new)
            .map_err(|e| JitError(e.to_string()))?;
        let mut mk_sb_app = |name: &str, val: cranelift_codegen::ir::Type| -> Result<_, cranelift_module::ModuleError> {
            let mut s = self.module.make_signature();
            s.params.push(AbiParam::new(types::I64)); // builder 对象
            s.params.push(AbiParam::new(val));
            self.module.declare_function(name, Linkage::Import, &s)
        };
        let sb_app_int_id =
            mk_sb_app("vmless_sb_append_int", types::I32).map_err(|e| JitError(e.to_string()))?;
        let sb_app_bool_id =
            mk_sb_app("vmless_sb_append_bool", types::I32).map_err(|e| JitError(e.to_string()))?;
        let sb_app_long_id =
            mk_sb_app("vmless_sb_append_long", types::I64).map_err(|e| JitError(e.to_string()))?;
        let sb_app_obj_id =
            mk_sb_app("vmless_sb_append_obj", types::I64).map_err(|e| JitError(e.to_string()))?;
        let sb_app_dbl_id =
            mk_sb_app("vmless_sb_append_double", types::F64).map_err(|e| JitError(e.to_string()))?;
        let sb_app_flt_id =
            mk_sb_app("vmless_sb_append_float", types::F32).map_err(|e| JitError(e.to_string()))?;
        let sb_app_char_id =
            mk_sb_app("vmless_sb_append_char", types::I32).map_err(|e| JitError(e.to_string()))?;
        let mut sig_sb_str = self.module.make_signature();
        sig_sb_str.params.push(AbiParam::new(types::I64));
        sig_sb_str.returns.push(AbiParam::new(types::I64));
        let sb_to_str_id = self
            .module
            .declare_function("vmless_sb_to_string", Linkage::Import, &sig_sb_str)
            .map_err(|e| JitError(e.to_string()))?;

        // 内建 JDK 类：三个通用入口（参数一律 i64，double 由调用点 bitcast）
        let mut sig_bv = self.module.make_signature();
        sig_bv.params.push(AbiParam::new(types::I64)); // recv
        sig_bv.params.push(AbiParam::new(types::I32)); // op
        sig_bv.params.push(AbiParam::new(types::I64)); // a
        sig_bv.params.push(AbiParam::new(types::I64)); // b
        sig_bv.returns.push(AbiParam::new(types::I64));
        let builtin_vcall_id = self
            .module
            .declare_function("vmless_builtin_vcall", Linkage::Import, &sig_bv)
            .map_err(|e| JitError(e.to_string()))?;
        let mut sig_bs = self.module.make_signature();
        sig_bs.params.push(AbiParam::new(types::I32)); // op
        sig_bs.params.push(AbiParam::new(types::I64)); // a
        sig_bs.params.push(AbiParam::new(types::I64)); // b
        sig_bs.returns.push(AbiParam::new(types::I64));
        let builtin_static_id = self
            .module
            .declare_function("vmless_builtin_static", Linkage::Import, &sig_bs)
            .map_err(|e| JitError(e.to_string()))?;
        let mut sig_bsf = self.module.make_signature();
        sig_bsf.params.push(AbiParam::new(types::I32));
        sig_bsf.returns.push(AbiParam::new(types::I64));
        let builtin_sfield_id = self
            .module
            .declare_function("vmless_builtin_sfield", Linkage::Import, &sig_bsf)
            .map_err(|e| JitError(e.to_string()))?;
        let mut sig_ac = self.module.make_signature();
        for _ in 0..5 {
            sig_ac.params.push(AbiParam::new(types::I64));
        }
        sig_ac.returns.push(AbiParam::new(types::I64));
        let arraycopy_id = self
            .module
            .declare_function("vmless_arraycopy", Linkage::Import, &sig_ac)
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
        // 引用写屏障：老对象存年轻引用必须把卡标脏，否则 minor 会把活对象回收掉
        let mut sig_sr = self.module.make_signature();
        sig_sr.params.push(AbiParam::new(types::I64));
        sig_sr.params.push(AbiParam::new(types::I32));
        sig_sr.params.push(AbiParam::new(types::I64));
        let set_ref_id = self
            .module
            .declare_function("vmless_set_ref", Linkage::Import, &sig_sr)
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
                // tableswitch / lookupswitch：每个 case + default 都是块首，
                // 另外指令之后的地址单独建一个块（switch 不落空，那段是死代码，
                // 给它一个无人跳入的块，才不会继续往已终结的块里塞指令）
                0xaa | 0xab => {
                    let len = insn_len_at(&body, pc);
                    for t in switch_targets(&body, pc, len)? {
                        targets.push((pc as isize + t as isize) as usize);
                    }
                    targets.push(pc + len);
                    pc += len;
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

        // 每个 pc 上「操作数栈哪一位是引用」：GC 点同步影子栈、GC 后读回新偏移都靠它
        let refs = refmap::analyze(&cf, &body, &blocks, &handler_pcs)?;
        // 整个方法都不碰引用（纯数值代码）就不必维护影子栈：省掉每次调用/分配的
        // 根区登记与读写
        let has_refs =
            refs.uses_ref || !is_static || param_descs(desc).iter().any(|d| is_ref_desc(d));

        // ---- 影子栈（根区）----
        // 每帧在自己的栈帧里留 max_locals + max_stack 个 u32：**引用型局部变量只住这里**，
        // 操作数栈上的引用在 GC 点前也写进来。整块登记进 Heap 的根表，GC 搬完对象就地
        // 改写它，本帧再从根区把新偏移读回去。
        let root_slots = if has_refs { (max_locals + max_stack).max(1) as i32 } else { 0 };
        let root_area = fb.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
            cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
            (root_slots * 4) as u32,
            2,
        ));
        let root_ptr = fb.ins().stack_addr(types::I64, root_area, 0);
        {
            // 栈槽不保证是 0：先清零，别让垃圾值被当成偏移
            let zero = fb.ins().iconst(types::I32, 0);
            for i in 0..root_slots {
                let addr = fb.ins().iadd_imm(root_ptr, (i * 4) as i64);
                fb.ins().store(MemFlags::new(), zero, addr, 0);
            }
            // 引用型参数从 locals 搬进根区（局部变量区只留基本类型）
            let mut slots = param_slots(desc);
            let mut kinds: Vec<bool> = Vec::new();
            if !is_static {
                // 实例方法：this 在槽 0，参数从槽 1 起（与调用点写参的布局一致）
                for s in slots.iter_mut() {
                    *s += 1;
                }
                slots.insert(0, 0);
                kinds.push(true);
            }
            for d in param_descs(desc) {
                kinds.push(is_ref_desc(&d));
            }
            for (k, slot) in slots.iter().enumerate() {
                if kinds.get(k).copied().unwrap_or(false) && *slot < max_locals {
                    let addr = fb.ins().iadd_imm(locals_ptr, (*slot * 8) as i64);
                    let v = fb.ins().load(types::I64, MemFlags::new(), addr, 0);
                    store_root(&mut fb, root_ptr, *slot, v);
                }
            }
        }
        if has_refs {
            self.emit_roots(&mut fb, root_ptr, root_slots, true)?;
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
        let mut cur_start = usize::MAX; // pc==0 也可能是跳转目标（`:loop` 回到方法开头），别和 entry 混为一谈
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
                        const_load(&mut fb, &mut stack, &cf, idx);
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
                        const_load(&mut fb, &mut stack, &cf, idx);
                    }
                }
                0x15 => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = load_local(&mut fb, locals_ptr, slot);
                    stack.push(v);
                }
                // fload / fload_0..3：局部变量按 F32 读（槽是 8 字节，只读低 4 字节）
                0x17 => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    let v = fb.ins().load(types::F32, MemFlags::new(), addr, 0);
                    stack.push(v);
                }
                0x22..=0x25 => {
                    let slot = (op - 0x22) as usize;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    let v = fb.ins().load(types::F32, MemFlags::new(), addr, 0);
                    stack.push(v);
                }
                0x43..=0x46 => {
                    // fstore_0..3
                    let slot = (op - 0x43) as usize;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let addr = fb.ins().iadd_imm(locals_ptr, (slot * 8) as i64);
                    fb.ins().store(MemFlags::new(), v, addr, 0);
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
                    if op == 0x58 {
                        // pop2：宽值只弹一个，否则再弹一个
                        let top_is_wide = stack
                            .last()
                            .map(|v| is_wide_ty(fb.func.dfg.value_type(*v)))
                            .unwrap_or(false);
                        if !top_is_wide {
                            stack.pop();
                        }
                    }
                }
                // dup / dup_x1 / dup_x2 / dup2 / dup2_x1 / dup2_x2 / swap
                0x59 => {
                    let v = *stack.last().ok_or_else(|| JitError("栈下溢".into()))?;
                    stack.push(v);
                }
                0x5a => {
                    // … v2, v1 → … v1, v2, v1
                    let n = stack.len();
                    if n >= 2 {
                        let v1 = stack[n - 1];
                        stack.insert(n - 2, v1);
                    }
                }
                0x5b => {
                    // dup_x2
                    let n = stack.len();
                    if n >= 2 {
                        let v1 = stack[n - 1];
                        let w2 = is_wide_ty(fb.func.dfg.value_type(stack[n - 2]));
                        if w2 {
                            stack.insert(n - 2, v1); // 形式 2
                        } else if n >= 3 {
                            stack.insert(n - 3, v1); // 形式 1
                        }
                    }
                }
                0x5c => {
                    // dup2
                    let n = stack.len();
                    let top = *stack.last().ok_or_else(|| JitError("栈下溢".into()))?;
                    if is_wide_ty(fb.func.dfg.value_type(top)) {
                        stack.push(top);
                    } else if n >= 2 {
                        let a = stack[n - 2];
                        stack.push(a);
                        stack.push(top);
                    }
                }
                0x5d => {
                    // dup2_x1
                    let n = stack.len();
                    if n >= 2 {
                        let v1 = stack[n - 1];
                        if is_wide_ty(fb.func.dfg.value_type(v1)) {
                            stack.insert(n - 2, v1); // 形式 2
                        } else if n >= 3 {
                            let v2 = stack[n - 2];
                            stack.insert(n - 3, v2);
                            stack.insert(n - 2, v1);
                        }
                    }
                }
                0x5e => {
                    // dup2_x2：形式 4 = 栈顶两项都宽；形式 3 = 只有栈顶宽；
                    // 形式 2 = 第三项宽；形式 1 = 四个都是 1 槽
                    let n = stack.len();
                    if n >= 2 {
                        let v1 = stack[n - 1];
                        let top_wide = is_wide_ty(fb.func.dfg.value_type(v1));
                        if top_wide && is_wide_ty(fb.func.dfg.value_type(stack[n - 2])) {
                            stack.insert(n - 2, v1); // 形式 4
                        } else if top_wide && n >= 3 {
                            stack.insert(n - 3, v1); // 形式 3
                        } else if n >= 3 {
                            let v2 = stack[n - 2];
                            if is_wide_ty(fb.func.dfg.value_type(stack[n - 3])) {
                                stack.insert(n - 3, v2); // 形式 2
                                stack.insert(n - 2, v1);
                            } else if n >= 4 {
                                stack.insert(n - 4, v2); // 形式 1
                                stack.insert(n - 3, v1);
                            }
                        }
                    }
                }
                0x5f => {
                    // swap：两个 1 槽值互换
                    let n = stack.len();
                    if n >= 2 {
                        stack.swap(n - 1, n - 2);
                    }
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
                // tableswitch / lookupswitch：发一串「查表值 == key」的条件跳转，
                // 落空链的最后跳到 default。每个目标块都要求栈空，所以先弹掉 key。
                0xaa | 0xab => {
                    let len = insn_len_at(&body, pc - 1);
                    let start = pc - 1;
                    let (pairs, default_off) = parse_switch(&body, start, len)?;
                    let key = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let key = convert_to(&mut fb, key, types::I32);
                    let blk = |off: i32| -> Result<cranelift_codegen::ir::Block, JitError> {
                        let target = (start as isize + off as isize) as usize;
                        blocks
                            .get(&target)
                            .copied()
                            .ok_or_else(|| JitError(format!("switch 目标 {target} 没有块")))
                    };
                    let default_block = blk(default_off)?;
                    for (v, off) in &pairs {
                        let target = blk(*off)?;
                        let next = fb.create_block();
                        let cv = fb.ins().iconst(types::I32, *v as i64);
                        let eq = fb.ins().icmp(IntCC::Equal, key, cv);
                        fb.ins().brif(eq, target, &[], next, &[]);
                        fb.switch_to_block(next);
                    }
                    fb.ins().jump(default_block, &[]);
                    pc = start + len;
                    // switch 不落空：指令之后那段代码单独一个（无人跳入的）块
                    let fall = *blocks.get(&pc).ok_or_else(|| JitError("缺少 switch 落空块".into()))?;
                    open.pop();
                    open.push(fall);
                    fb.switch_to_block(fall);
                    terminated = false;
                    cur_start = pc;
                    cur_block = fall;
                }
                // ireturn / lreturn / freturn / dreturn / areturn：统一按方法签名声明的返回类型收口
                0xac | 0xad | 0xae | 0xaf | 0xb0 => {
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let v = convert_to(&mut fb, v, ret_ty);
                    if has_refs {
                        self.emit_frame_exit(&mut fb, root_ptr, root_slots)?;
                    }
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
                    if has_refs {
                        self.emit_frame_exit(&mut fb, root_ptr, root_slots)?;
                    }
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
                    self.emit_dispatch(
                            &mut fb,
                            &cf,
                            &exc_table,
                            &blocks,
                            exc_slot,
                            &helpers,
                            op_pc,
                            ret_ty,
                            root_ptr,
                            root_slots,
                        )?;
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
                    let (bcls, bname, bdesc) = method_ref_of(&cf, idx)?;
                    if let Some(op) = crate::jdk::sop(&bcls, &bname, &bdesc) {
                        // 内建静态方法（`Integer.parseInt` / `System.arraycopy` …）
                        let params = param_descs(&bdesc);
                        let mut args = Vec::new();
                        for _ in 0..params.len() {
                            args.push(stack.pop().ok_or_else(|| JitError("栈下溢".into()))?);
                        }
                        args.reverse();
                        let op_pc = pc - 3;
                        let refs_pc = refs.at.get(&op_pc).cloned().unwrap_or_default();
                        spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
                        if has_refs {
                            self.emit_roots(&mut fb, root_ptr, root_slots, true)?;
                        }
                        let raw = if bcls == "java/lang/System" && bname == "arraycopy" {
                            // 五参数：单独一个入口
                            let mut vs = Vec::new();
                            for (i, v) in args.iter().enumerate() {
                                vs.push(abi_arg(&mut fb, *v, &params[i]));
                            }
                            let fref = self.module.declare_func_in_func(arraycopy_id, fb.func);
                            let c = fb.ins().call(fref, &vs);
                            fb.inst_results(c)[0]
                        } else {
                            let a64 = match args.first() {
                                Some(v) => abi_arg(&mut fb, *v, &params[0]),
                                None => fb.ins().iconst(types::I64, 0),
                            };
                            let b64 = match args.get(1) {
                                Some(v) => abi_arg(&mut fb, *v, &params[1]),
                                None => fb.ins().iconst(types::I64, 0),
                            };
                            let oi = fb.ins().iconst(types::I32, op as i64);
                            let fref =
                                self.module.declare_func_in_func(builtin_static_id, fb.func);
                            let c = fb.ins().call(fref, &[oi, a64, b64]);
                            fb.inst_results(c)[0]
                        };
                        if has_refs {
                            self.emit_roots(&mut fb, root_ptr, root_slots, false)?;
                        }
                        reload_refs(&mut fb, &mut stack, &refs_pc, root_ptr, max_locals);
                        let ret = bdesc.rsplit(')').next().unwrap_or("V").to_string();
                        if ret != "V" {
                            let v = abi_result(&mut fb, raw, &ret);
                            stack.push(v);
                        }
                    } else {
                    let (fid, callee_max_locals, argc) = *call_targets
                        .get(&idx)
                        .ok_or_else(|| JitError("调用目标未编译".into()))?;
                    // <clinit> 与调用本身都可能分配对象：先把操作数栈上的引用写进根区并登记
                    let op_pc = pc - 3;
                    let refs_pc = refs.at.get(&op_pc).cloned().unwrap_or_default();
                    spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
                    if has_refs {
            self.emit_roots(&mut fb, root_ptr, root_slots, true)?;
        }
                    // 首次调用这个类的静态方法时先跑它的 <clinit>
                    let callee_owner = method_ref_of(&cf, idx)?.0;
                    self.emit_clinit_call(&mut fb, &callee_owner)?;
                    self.emit_roots(&mut fb, root_ptr, root_slots, false)?;
                    reload_refs(&mut fb, &mut stack, &refs_pc, root_ptr, max_locals);
                    spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
                    if has_refs {
            self.emit_roots(&mut fb, root_ptr, root_slots, true)?;
        }
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
                    // 被调方已按自己的返回类型返回（i64/f64/i32）
                    let rv = if callee_desc.ends_with(")V") {
                        None
                    } else {
                        Some(fb.inst_results(call)[0])
                    };
                    // 被调方可能分配过对象：撤根区，把（可能被搬过的）新偏移读回操作数栈
                    self.emit_roots(&mut fb, root_ptr, root_slots, false)?;
                    reload_refs(&mut fb, &mut stack, &refs_pc, root_ptr, max_locals);
                    if let Some(v) = rv {
                        stack.push(v);
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
                        self.emit_dispatch(
                            &mut fb,
                            &cf,
                            &exc_table,
                            &blocks,
                            exc_slot,
                            &helpers,
                            op_pc,
                            ret_ty,
                            root_ptr,
                            root_slots,
                        )?;
                        fb.switch_to_block(cont);
                    }
                    }
                }
                // 引用类型局部变量（8 字节槽）
                0x2a..=0x2d => {
                    let slot = (op - 0x2a) as usize;
                    // 引用型局部变量只住影子栈（根区）：GC 搬过对象后那里是最新偏移
                    let v = load_root(&mut fb, root_ptr, slot);
                    stack.push(v);
                }
                0x19 => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = load_root(&mut fb, root_ptr, slot);
                    stack.push(v);
                }
                0x4b..=0x4e => {
                    let slot = (op - 0x4b) as usize;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    store_root(&mut fb, root_ptr, slot, v);
                }
                0x3a => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    store_root(&mut fb, root_ptr, slot, v);
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
                    let op_pc = pc - 3;
                    let refs_pc = refs.at.get(&op_pc).cloned().unwrap_or_default();
                    spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
                    if has_refs {
            self.emit_roots(&mut fb, root_ptr, root_slots, true)?;
        }
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
                    let rv = if callee_desc.ends_with(")V") {
                        None
                    } else {
                        Some(fb.inst_results(c)[0])
                    };
                    self.emit_roots(&mut fb, root_ptr, root_slots, false)?;
                    reload_refs(&mut fb, &mut stack, &refs_pc, root_ptr, max_locals);
                    if let Some(v) = rv {
                        stack.push(v);
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
                0x0b..=0x0d => {
                    // fconst_0 / fconst_1 / fconst_2
                    stack.push(fb.ins().f32const((op - 0x0b) as f32));
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
                // float 算术 / 取反（F32 指令，f32 舍入由指令本身保证）
                0x62 => lng(&mut fb, &mut stack, |f, a, b| f.ins().fadd(a, b))?,
                0x66 => lng(&mut fb, &mut stack, |f, a, b| f.ins().fsub(a, b))?,
                0x6a => lng(&mut fb, &mut stack, |f, a, b| f.ins().fmul(a, b))?,
                0x6e => lng(&mut fb, &mut stack, |f, a, b| f.ins().fdiv(a, b))?,
                0x72 => {
                    // frem：cranelift 没有 frem 指令，调 CRT 的 fmodf
                    let b = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let fref = self.module.declare_func_in_func(frem_id, fb.func);
                    let c = fb.ins().call(fref, &[a, b]);
                    stack.push(fb.inst_results(c)[0]);
                }
                0x76 => {
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
                0x95 | 0x96 => {
                    // fcmpl(0x95: NaN → -1) / fcmpg(0x96: NaN → +1)
                    let b = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let (cc_lt, cc_gt) = if op == 0x95 {
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
                // float 与其它数值类型的互转（F32 参与，位宽按目标类型走）
                0x86 => {
                    // i2f
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    stack.push(fb.ins().fcvt_from_sint(types::F32, a));
                }
                0x89 => {
                    // l2f
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    stack.push(fb.ins().fcvt_from_sint(types::F32, a));
                }
                0x8b => {
                    // f2i（Rust/Cranelift 的饱和转换与 Java 语义一致：NaN → 0，超界钳到边界）
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    stack.push(fb.ins().fcvt_to_sint_sat(types::I32, a));
                }
                0x8c => {
                    // f2l
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    stack.push(fb.ins().fcvt_to_sint_sat(types::I64, a));
                }
                0x8d => {
                    // f2d
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    stack.push(fb.ins().fpromote(types::F64, a));
                }
                0x90 => {
                    // d2f
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    stack.push(fb.ins().fdemote(types::F32, a));
                }
                // i2b / i2c / i2s：截断到目标宽度后**符号扩展**回 int
                0x91 => {
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let a = fb.ins().ireduce(types::I8, a);
                    stack.push(fb.ins().sextend(types::I32, a));
                }
                0x92 => {
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    stack.push(fb.ins().band_imm(a, 0xFFFF));
                }
                0x93 => {
                    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let a = fb.ins().ireduce(types::I16, a);
                    stack.push(fb.ins().sextend(types::I32, a));
                }
                // getstatic：静态字段（System.out 仍是 PrintStream 占位）
                0xb2 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let (owner, fname, fdesc) = field_ref_of(&cf, idx)?;
                    if owner == "java/lang/System" && fname == "out" {
                        let z = fb.ins().iconst(types::I64, 0);
                        stack.push(z);
                    } else if let Some(op) = crate::jdk::FIELDS
                        .iter()
                        .position(|(c, f, _)| *c == owner && *f == fname)
                    {
                        // 内建常量：`Integer.MAX_VALUE` / `Boolean.TRUE`（没有 .class）
                        let oi = fb.ins().iconst(types::I32, op as i64);
                        let fref = self.module.declare_func_in_func(builtin_sfield_id, fb.func);
                        let c = fb.ins().call(fref, &[oi]);
                        let raw = fb.inst_results(c)[0];
                        let v = abi_result(&mut fb, raw, &fdesc);
                        stack.push(v);
                    } else {
                        self.emit_clinit_call(&mut fb, &owner)?;
                        let slot = rt::static_slot(&owner, &fname, is_ref_desc(&fdesc));
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
                    let slot = rt::static_slot(&owner, &fname, is_ref_desc(&fdesc));
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
                            "(D)V" => (println_double_id, types::F64),
                            "(F)V" => (println_float_id, types::F32),
                            "(Z)V" => (println_bool_id, types::I32),
                            "(C)V" => (println_char_id, types::I32),
                            "(Ljava/lang/String;)V" | "(Ljava/lang/Object;)V" => {
                                (println_obj_id, types::I64)
                            }
                            _ => (println_int_id, types::I32),
                        };
                        let v = convert_to(&mut fb, v, want);
                        let fref = self.module.declare_func_in_func(fid, fb.func);
                        fb.ins().call(fref, &[v]);
                    } else if cls == "java/lang/StringBuilder" {
                        // 内置：字符串拼接（`StringBuilder` 没有 .class，进不了虚方法表）
                        if name == "toString" {
                            let recv = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                            let obj = convert_to(&mut fb, recv, types::I64);
                            let op_pc = pc - if op == 0xb9 { 5 } else { 3 };
                            let refs_pc = refs.at.get(&op_pc).cloned().unwrap_or_default();
                            spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
                            if has_refs {
            self.emit_roots(&mut fb, root_ptr, root_slots, true)?;
        }
                            let fref = self.module.declare_func_in_func(sb_to_str_id, fb.func);
                            let c = fb.ins().call(fref, &[obj]);
                            if has_refs {
            self.emit_roots(&mut fb, root_ptr, root_slots, false)?;
        }
                            reload_refs(&mut fb, &mut stack, &refs_pc, root_ptr, max_locals);
                            stack.push(fb.inst_results(c)[0]);
                        } else if name == "append" {
                            let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                            let recv = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                            let obj = convert_to(&mut fb, recv, types::I64);
                            let want = if desc.starts_with("(J)") || desc.starts_with("(L") {
                                types::I64
                            } else if desc.starts_with("(D)") {
                                types::F64
                            } else if desc.starts_with("(F)") {
                                types::F32
                            } else {
                                types::I32
                            };
                            let fid = if desc.starts_with("(D)") {
                                sb_app_dbl_id
                            } else if desc.starts_with("(F)") {
                                sb_app_flt_id
                            } else if desc.starts_with("(C)") {
                                sb_app_char_id
                            } else if desc.starts_with("(J)") {
                                sb_app_long_id
                            } else if desc.starts_with("(L") {
                                sb_app_obj_id
                            } else if desc.starts_with("(Z)") {
                                sb_app_bool_id
                            } else {
                                sb_app_int_id
                            };
                            let v = convert_to(&mut fb, v, want);
                            let fref = self.module.declare_func_in_func(fid, fb.func);
                            fb.ins().call(fref, &[obj, v]);
                            // append 返回 this：编译器在调用点 pop 掉
                            stack.push(recv);
                        } else {
                            return Err(JitError(format!("StringBuilder 没有方法 `{name}`")));
                        }
                    } else if cls == "java/lang/String"
                        && name == "equals"
                        && desc == "(Ljava/lang/Object;)Z"
                    {
                        // 内置：按内容比（`String` 没有 .class，进不了虚方法表）
                        let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                        let recv = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                        let a = convert_to(&mut fb, recv, types::I64);
                        let b = convert_to(&mut fb, v, types::I64);
                        let fref = self.module.declare_func_in_func(str_equals_id, fb.func);
                        let call = fb.ins().call(fref, &[a, b]);
                        let res = fb.inst_results(call)[0];
                        stack.push(res);
                    } else if let Some(op) = self.builtin_vop(&cls, &name, &desc) {
                        // 内建 JDK 类：统一走 `vmless_builtin_vcall(recv, op, a, b)`
                        let params = param_descs(&desc);
                        let mut args = Vec::new();
                        for _ in 0..params.len() {
                            args.push(stack.pop().ok_or_else(|| JitError("栈下溢".into()))?);
                        }
                        args.reverse();
                        let recv = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                        let op_pc = pc - if op == 0xb9 { 5 } else { 3 };
                        let refs_pc = refs.at.get(&op_pc).cloned().unwrap_or_default();
                        spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
                        if has_refs {
                            self.emit_roots(&mut fb, root_ptr, root_slots, true)?;
                        }
                        let recv64 = convert_to(&mut fb, recv, types::I64);
                        let a64 = match args.first() {
                            Some(v) => abi_arg(&mut fb, *v, &params[0]),
                            None => fb.ins().iconst(types::I64, 0),
                        };
                        let b64 = match args.get(1) {
                            Some(v) => abi_arg(&mut fb, *v, &params[1]),
                            None => fb.ins().iconst(types::I64, 0),
                        };
                        let oi = fb.ins().iconst(types::I32, op as i64);
                        let fref = self.module.declare_func_in_func(builtin_vcall_id, fb.func);
                        let c = fb.ins().call(fref, &[recv64, oi, a64, b64]);
                        let raw = fb.inst_results(c)[0];
                        if has_refs {
                            self.emit_roots(&mut fb, root_ptr, root_slots, false)?;
                        }
                        reload_refs(&mut fb, &mut stack, &refs_pc, root_ptr, max_locals);
                        let ret = desc.rsplit(')').next().unwrap_or("V").to_string();
                        if ret != "V" {
                            let v = abi_result(&mut fb, raw, &ret);
                            stack.push(v);
                        }
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
                        let op_pc = pc - if op == 0xb9 { 5 } else { 3 };
                        let refs_pc = refs.at.get(&op_pc).cloned().unwrap_or_default();
                        spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
                        if has_refs {
            self.emit_roots(&mut fb, root_ptr, root_slots, true)?;
        }
                        let ptr = fb.ins().stack_addr(types::I64, slot, 0);
                        let key = fb.ins().iconst(types::I32, sig as i64);
                        let fref = self.module.declare_func_in_func(vcall_id, fb.func);
                        let c = fb.ins().call(fref, &[ptr, key]);
                        let rv = if desc.ends_with(")V") {
                            None
                        } else {
                            Some(fb.inst_results(c)[0])
                        };
                        self.emit_roots(&mut fb, root_ptr, root_slots, false)?;
                        reload_refs(&mut fb, &mut stack, &refs_pc, root_ptr, max_locals);
                        if let Some(raw) = rv {
                            // vcall 统一按 i64 回来，按真实返回类型转一次
                            let want = ret_type(&desc);
                            let v = if want == types::F64 {
                                fb.ins().bitcast(types::F64, MemFlags::new(), raw)
                            } else if want == types::F32 {
                                // float 的位模式在低 32 位
                                let lo = fb.ins().ireduce(types::I32, raw);
                                fb.ins().bitcast(types::F32, MemFlags::new(), lo)
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
                            self.emit_dispatch(
                            &mut fb,
                            &cf,
                            &exc_table,
                            &blocks,
                            exc_slot,
                            &helpers,
                            op_pc,
                            ret_ty,
                            root_ptr,
                            root_slots,
                        )?;
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
                    // 内置：字符串拼接用的 StringBuilder（没有 .class，布局由运行时定）
                    if cls == "java/lang/StringBuilder" {
                        let refs_pc = refs.at.get(&(pc - 3)).cloned().unwrap_or_default();
                        spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
                        if has_refs {
            self.emit_roots(&mut fb, root_ptr, root_slots, true)?;
        }
                        let fref = self.module.declare_func_in_func(sb_new_id, fb.func);
                        let c = fb.ins().call(fref, &[]);
                        if has_refs {
            self.emit_roots(&mut fb, root_ptr, root_slots, false)?;
        }
                        reload_refs(&mut fb, &mut stack, &refs_pc, root_ptr, max_locals);
                        stack.push(fb.inst_results(c)[0]);
                        continue;
                    }
                    let tag = rt::class_tag(&cls);
                    // 类布局进 LayoutTable：GC 靠它知道哪些字段是引用（每字段两槽）
                    let descs = self.classes.field_descs(&cls);
                    let class_id = rt::register_class(tag, &cls, &descs);
                    let refs_pc = refs.at.get(&(pc - 3)).cloned().unwrap_or_default();
                    // <clinit> 里也可能分配对象 → 先登记根区再调
                    spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
                    if has_refs {
            self.emit_roots(&mut fb, root_ptr, root_slots, true)?;
        }
                    self.emit_clinit_call(&mut fb, &cls)?;
                    self.emit_roots(&mut fb, root_ptr, root_slots, false)?;
                    reload_refs(&mut fb, &mut stack, &refs_pc, root_ptr, max_locals);
                    // 分配本身也是 GC 点
                    spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
                    if has_refs {
            self.emit_roots(&mut fb, root_ptr, root_slots, true)?;
        }
                    let cid = fb.ins().iconst(types::I32, class_id as i64);
                    let fref = self.module.declare_func_in_func(new_obj_id, fb.func);
                    let c = fb.ins().call(fref, &[cid]);
                    self.emit_roots(&mut fb, root_ptr, root_slots, false)?;
                    reload_refs(&mut fb, &mut stack, &refs_pc, root_ptr, max_locals);
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
                    let addr = emit_ref_addr(&mut fb, p);
                    let off = field_off(f);
                    let v = if is_ref_desc(&desc) {
                        // 引用槽只占低 4 字节，读出来是堆内偏移
                        let v32 = fb.ins().load(types::I32, MemFlags::new(), addr, off);
                        fb.ins().uextend(types::I64, v32)
                    } else {
                        let (ty, want) = field_slot_type(&desc);
                        let v = fb.ins().load(ty, MemFlags::new(), addr, off);
                        convert_to(&mut fb, v, want)
                    };
                    stack.push(v);
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
                    if is_ref_desc(&desc) {
                        // 引用字段走写屏障：老对象存年轻引用必须把卡标脏
                        let idx = fb.ins().iconst(types::I32, field_slot_idx(f) as i64);
                        let p64 = convert_to(&mut fb, p, types::I64);
                        let v64 = convert_to(&mut fb, v, types::I64);
                        let fref = self.module.declare_func_in_func(set_ref_id, fb.func);
                        fb.ins().call(fref, &[p64, idx, v64]);
                    } else {
                        let (ty, want) = field_slot_type(&desc);
                        let v = convert_to(&mut fb, v, want);
                        let _ = ty;
                        let addr = emit_ref_addr(&mut fb, p);
                        fb.ins().store(MemFlags::new(), v, addr, field_off(f));
                    }
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
                    let n = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let n = convert_to(&mut fb, n, types::I32);
                    let refs_pc = refs.at.get(&(pc - 2)).cloned().unwrap_or_default();
                    spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
                    if has_refs {
            self.emit_roots(&mut fb, root_ptr, root_slots, true)?;
        }
                    let v = push_new_array(&mut self.module, &mut fb, new_arr_id, desc, n);
                    self.emit_roots(&mut fb, root_ptr, root_slots, false)?;
                    reload_refs(&mut fb, &mut stack, &refs_pc, root_ptr, max_locals);
                    stack.push(v);
                    let _ = esize;
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
                    // 描述符串只给 Rust 侧读，走独立表（不是 Java 对象）
                    let sidx = strtab::register_desc(&desc);
                    let sref = self.module.declare_func_in_func(desc_ptr_id, fb.func);
                    let si = fb.ins().iconst(types::I32, sidx as i64);
                    let sc = fb.ins().call(sref, &[si]);
                    let sp = fb.inst_results(sc)[0];
                    let refs_pc = refs.at.get(&(pc - 4)).cloned().unwrap_or_default();
                    spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
                    if has_refs {
            self.emit_roots(&mut fb, root_ptr, root_slots, true)?;
        }
                    let fref = self.module.declare_func_in_func(new_multi_id, fb.func);
                    let c = fb.ins().call(fref, &[ptr, nd, sp]);
                    self.emit_roots(&mut fb, root_ptr, root_slots, false)?;
                    reload_refs(&mut fb, &mut stack, &refs_pc, root_ptr, max_locals);
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
                    let n = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let n = convert_to(&mut fb, n, types::I32);
                    let refs_pc = refs.at.get(&(pc - 3)).cloned().unwrap_or_default();
                    spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
                    if has_refs {
            self.emit_roots(&mut fb, root_ptr, root_slots, true)?;
        }
                    let v = push_new_array(&mut self.module, &mut fb, new_arr_id, &desc, n);
                    self.emit_roots(&mut fb, root_ptr, root_slots, false)?;
                    reload_refs(&mut fb, &mut stack, &refs_pc, root_ptr, max_locals);
                    stack.push(v);
                }
                // arraylength（长度在偏移 0）
                0xbe => {
                    let p = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let addr = emit_ref_addr(&mut fb, p);
                    // librustjrt 的数组头 16 字节：class_id / flags / 长度 / 填充
                    let v = fb.ins().load(types::I32, MemFlags::new(), addr, 8);
                    stack.push(v);
                }
                // iaload / laload / faload / daload / aaload / baload / caload / saload
                // （元素宽度按指令选；boolean/byte/char/short 与 int 一样是 4 字节）
                0x2e | 0x2f | 0x30 | 0x31 | 0x32 | 0x33 | 0x34 | 0x35 => {
                    let esize: i64 = if matches!(op, 0x2f | 0x31) { 8 } else { 4 };
                    let ty = match op {
                        0x2f => types::I64,
                        0x31 => types::F64,
                        0x30 => types::F32, // faload：元素 4 字节
                        0x32 => types::I64,
                        _ => types::I32,
                    };
                    let idx = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let p = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let paddr = emit_ref_addr(&mut fb, p);
                    let addr = elem_addr(&mut fb, paddr, idx, esize)?;
                    let v = if op == 0x32 {
                        // 引用元素是 4 字节偏移
                        let v32 = fb.ins().load(types::I32, MemFlags::new(), addr, 0);
                        fb.ins().uextend(types::I64, v32)
                    } else {
                        fb.ins().load(ty, MemFlags::new(), addr, 0)
                    };
                    stack.push(v);
                }
                // iastore / lastore / fastore / dastore / aastore / bastore / castore / sastore
                0x4f | 0x50 | 0x51 | 0x52 | 0x53 | 0x54 | 0x55 | 0x56 => {
                    let esize: i64 = if matches!(op, 0x50 | 0x52) { 8 } else { 4 };
                    let want = match op {
                        0x50 => types::I64,
                        0x52 => types::F64,
                        0x51 => types::F32, // fastore：元素 4 字节
                        0x53 => types::I64,
                        _ => types::I32,
                    };
                    let v = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let idx = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    let p = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
                    if op == 0x53 {
                        // 引用元素走写屏障（老数组存年轻引用）
                        let p64 = convert_to(&mut fb, p, types::I64);
                        let i32v = convert_to(&mut fb, idx, types::I32);
                        let v64 = convert_to(&mut fb, v, types::I64);
                        let fref = self.module.declare_func_in_func(set_ref_id, fb.func);
                        fb.ins().call(fref, &[p64, i32v, v64]);
                    } else {
                        let v = convert_to(&mut fb, v, want);
                        let paddr = emit_ref_addr(&mut fb, p);
                        let addr = elem_addr(&mut fb, paddr, idx, esize)?;
                        fb.ins().store(MemFlags::new(), v, addr, 0);
                    }
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
                // invokedynamic：JIT 侧**明确不支持**，不假装支持。
                // 这个 opcode 要跑引导方法、构造/缓存 CallSite，属于解释器那条路径
                // （`vm::indy` 已完整实现：字符串拼接、lambda、方法引用、condy）。
                // 真正接进 JIT 需要把调用点缓存做成机器码里的可替换跳板，是另一件事；
                // 在那之前 `run_jit` 看到这个错误会整体退回解释器执行。
                0xba => {
                    return Err(JitError(format!(
                        "JIT 不支持 invokedynamic（0xba@{pc}）：{class}.{name}{desc} 里的 \
                         lambda/字符串拼接只能由解释器执行（JIT 未接入调用点缓存）"
                    )))
                }
                other => return Err(JitError(format!("JIT 暂不支持字节码 0x{other:02x}"))),
            }
        }
        // 所有还没终结的块统一补该返回类型的零值
        for b in open {
            fb.switch_to_block(b);
            let z = zero_of(&mut fb, ret_ty);
            if has_refs {
                self.emit_frame_exit(&mut fb, root_ptr, root_slots)?;
            }
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

    /// 这个调用是不是**内建 JDK 方法**（String / 包装类 / Object 协议）→ op 号。
    ///
    /// 只有「类里没有真实现」时才走内建：类自己覆写了 `hashCode` 就必须正常虚分派。
    fn builtin_vop(&mut self, cls: &str, name: &str, desc: &str) -> Option<u32> {
        if self.classes.resolve_method(cls, name, desc).is_some() {
            return None;
        }
        if let Some(op) = crate::jdk::vop(cls, name, desc) {
            return Some(op);
        }
        // 没覆写的 `toString`/`hashCode`/`equals`：继承自 Object（我们没有它的 .class）
        if crate::jdk::object_protocol(name, argc_of(desc) as usize).as_deref() == Some(desc) {
            return crate::jdk::vop("java/lang/Object", name, desc);
        }
        None
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
        } else if rty == types::F32 {
            rt::RET_F32
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

/// 内建 ABI 的实参：统一转成 i64（double/float 走位模式）
fn abi_arg(fb: &mut FunctionBuilder, v: Value, desc: &str) -> Value {
    match desc.as_bytes().first().copied() {
        Some(b'D') => fb.ins().bitcast(types::I64, MemFlags::new(), v),
        // float：位模式塞进 i64 的低 32 位（运行时按 i64 收，解码时只取低 32 位）
        Some(b'F') => {
            let b = fb.ins().bitcast(types::I32, MemFlags::new(), v);
            fb.ins().uextend(types::I64, b)
        }
        _ => convert_to(fb, v, types::I64),
    }
}

/// 内建 ABI 的返回值：i64 → 描述符要求的机器类型（double/float 走位模式还原）
fn abi_result(fb: &mut FunctionBuilder, raw: Value, ret: &str) -> Value {
    match ret.as_bytes().first().copied() {
        Some(b'D') => fb.ins().bitcast(types::F64, MemFlags::new(), raw),
        Some(b'F') => {
            let lo = fb.ins().ireduce(types::I32, raw);
            fb.ins().bitcast(types::F32, MemFlags::new(), lo)
        }
        Some(b'J') | Some(b'L') | Some(b'[') => raw,
        _ => convert_to(fb, raw, types::I32),
    }
}

/// 这个机器类型是不是 JVM 意义上的 cat-2（long/double 占两个栈槽；F32 不算）
fn is_wide_ty(ty: cranelift_codegen::ir::Type) -> bool {
    ty == types::I64 || ty == types::F64
}

/// 解析 `tableswitch` / `lookupswitch`：返回 `(case 值, 相对操作码的偏移)` 列表 + default 偏移。
///
/// `pc` 指向操作码，`len` 是整条指令的字节数（含 padding）。
fn parse_switch(body: &[u8], pc: usize, len: usize) -> Result<(Vec<(i32, i32)>, i32), JitError> {
    let mut q = pc + 1;
    while q % 4 != 0 {
        q += 1;
    }
    let rd = |at: usize| -> i32 {
        let b = |k: usize| body.get(k).copied().unwrap_or(0) as i32;
        (b(at) << 24) | (b(at + 1) << 16) | (b(at + 2) << 8) | b(at + 3)
    };
    if q + 8 > pc + len {
        return Err(JitError("switch 指令被截断".into()));
    }
    let default = rd(q);
    let mut pairs = Vec::new();
    if body[pc] == 0xaa {
        let low = rd(q + 4);
        let high = rd(q + 8);
        for v in low..=high {
            let at = q + 12 + ((v - low) as usize) * 4;
            if at + 4 > pc + len {
                break;
            }
            pairs.push((v, rd(at)));
        }
    } else {
        let n = rd(q + 4).max(0) as usize;
        for i in 0..n {
            let at = q + 8 + i * 8;
            if at + 8 > pc + len {
                break;
            }
            pairs.push((rd(at), rd(at + 4)));
        }
    }
    Ok((pairs, default))
}

/// switch 的全部跳转目标（相对操作码的偏移，已去重）
fn switch_targets(body: &[u8], pc: usize, len: usize) -> Result<Vec<i32>, JitError> {
    let (pairs, default) = parse_switch(body, pc, len)?;
    let mut out = vec![default];
    for (_, off) in pairs {
        if !out.contains(&off) {
            out.push(off);
        }
    }
    Ok(out)
}

fn bin(
    fb: &mut FunctionBuilder,
    stack: &mut Vec<Value>,
    f: impl FnOnce(&mut FunctionBuilder, Value, Value) -> Value,
) -> Result<(), JitError> {    let b = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
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

/// `ldc`/`ldc_w` 的非字符串常量：Float 压 F32，其余（Integer 等）压 I32
fn const_load(
    fb: &mut FunctionBuilder,
    stack: &mut Vec<Value>,
    cf: &ClassFile,
    idx: u16,
) {
    match cf.constant_pool.get(idx as usize).and_then(|e| e.as_ref()) {
        Some(CpEntry::Float(bits)) => {
            stack.push(fb.ins().f32const(f32::from_bits(*bits)));
        }
        _ => {
            let v = const_int(cf, idx);
            stack.push(fb.ins().iconst(types::I32, v));
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

// ---------- 影子栈（根区）辅助 ----------

/// 把引用型局部变量写进根区（局部变量区只放基本类型，引用只住根区）
fn store_root(fb: &mut FunctionBuilder, root_ptr: Value, slot: usize, v: Value) {
    let v = convert_to(fb, v, types::I64);
    let v32 = fb.ins().ireduce(types::I32, v);
    let addr = fb.ins().iadd_imm(root_ptr, (slot * 4) as i64);
    fb.ins().store(MemFlags::new(), v32, addr, 0);
}

/// 从根区读一个引用（GC 之后这里是最新偏移）
fn load_root(fb: &mut FunctionBuilder, root_ptr: Value, slot: usize) -> Value {
    let addr = fb.ins().iadd_imm(root_ptr, (slot * 4) as i64);
    let v32 = fb.ins().load(types::I32, MemFlags::new(), addr, 0);
    fb.ins().uextend(types::I64, v32)
}

/// GC 点前：把操作数栈上的引用写进根区（局部变量早就在里面了）
fn spill_refs(fb: &mut FunctionBuilder, stack: &[Value], refs: &[bool], root_ptr: Value, base: usize) {
    for (j, v) in stack.iter().enumerate() {
        if refs.get(j).copied().unwrap_or(false) {
            store_root(fb, root_ptr, base + j, *v);
        }
    }
}

/// GC 点后：把根区里（可能被 GC 改写过的）新偏移读回操作数栈
fn reload_refs(
    fb: &mut FunctionBuilder,
    stack: &mut [Value],
    refs: &[bool],
    root_ptr: Value,
    base: usize,
) {
    for (j, v) in stack.iter_mut().enumerate() {
        if refs.get(j).copied().unwrap_or(false) {
            *v = load_root(fb, root_ptr, base + j);
        }
    }
}

impl Jit {
    /// 把本帧根区登记进 / 撤出堆的根表
    pub(super) fn emit_roots(
        &mut self,
        fb: &mut FunctionBuilder,
        root: Value,
        n: i32,
        push: bool,
    ) -> Result<(), JitError> {
        // 方法里没有任何引用（root_slots == 0）：整块影子栈都不需要，连调用都不发
        if n == 0 {
            return Ok(());
        }
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I32));
        let name = if push { "vmless_roots_push" } else { "vmless_roots_pop" };
        let id = self
            .module
            .declare_function(name, Linkage::Import, &sig)
            .map_err(|e| JitError(e.to_string()))?;
        let fref = self.module.declare_func_in_func(id, fb.func);
        let nn = fb.ins().iconst(types::I32, n as i64);
        fb.ins().call(fref, &[root, nn]);
        Ok(())
    }

    /// 离开本帧：撤掉根区（异常传播的 return 路径也必须走到这里）
    pub(super) fn emit_frame_exit(
        &mut self,
        fb: &mut FunctionBuilder,
        root: Value,
        n: i32,
    ) -> Result<(), JitError> {
        self.emit_roots(fb, root, n, false)
    }
}

/// 方法体里第一条 `invokedynamic`（0xba）的偏移。
///
/// 按指令长度走，不会把操作数里的 0xba 当成指令（`insn_len_at` 处理变长的 switch）。
fn find_invokedynamic(body: &[u8]) -> Option<usize> {
    let mut pc = 0usize;
    while pc < body.len() {
        if body[pc] == 0xba {
            return Some(pc);
        }
        pc += insn_len_at(body, pc).max(1);
    }
    None
}

/// 方法体里第一条打向 `java.lang.invoke.*` 的调用（返回 `类.方法描述符`）。
///
/// JIT 完全没有这一层：方法句柄/调用点的语义在解释器的 side table 里，JIT 有自己的
/// 堆和静态区，吞不进去。所以这里**明确拒绝**，而不是让它在链接阶段报
/// 「调用目标未编译」那种让人看不懂的错。
fn find_invoke_api(cf: &ClassFile, body: &[u8]) -> Option<String> {
    let mut pc = 0usize;
    while pc < body.len() {
        let op = body[pc];
        if matches!(op, 0xb6..=0xb9) && pc + 2 < body.len() {
            let idx = u16::from_be_bytes([body[pc + 1], body[pc + 2]]);
            if let Ok((c, n, d)) = method_ref_of(cf, idx) {
                if c.starts_with("java/lang/invoke/") {
                    return Some(format!("{c}.{n}{d}"));
                }
            }
        }
        pc += insn_len_at(body, pc).max(1);
    }
    None
}
