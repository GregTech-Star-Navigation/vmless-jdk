//! 字节码 → Cranelift IR 的主翻译器。

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{types, AbiParam, InstBuilder, MemFlags, Value};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{Linkage, Module};

use crate::{ClassFile, CpEntry};

use super::call::{
    argc_of, code_of, convert_to, method_ref_of, param_descs, param_slots, ret_type, zero_of,
};
use super::divmod;
use super::exc::{class_uses_exceptions, ExcHelpers};
use super::classes::ClassSet;
use super::gcfree;
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
        let key = (class.to_string(), name.to_string(), desc.to_string());
        if let Some(v) = self.compiled.get(&key) {
            return Ok(*v);
        }
        // 已经判定「只能解释执行」的方法：不再重试，直接回同一个原因
        if let Some(why) = self.interpreted.get(&key) {
            return Err(JitError(format!("{class}.{name}{desc} 只能解释执行：{why}")));
        }
        match self.compile_into_inner(class, name, desc) {
            Ok(v) => {
                self.n_compiled += 1;
                Ok(v)
            }
            Err(e) => {
                // 失败的方法不能留在缓存里：留着会让后续调用点拿到一个**从没定义过**的
                // 函数 id（生成代码会跳到空洞）
                self.compiled.remove(&key);
                self.interpreted.insert(key, e.0.clone());
                Err(e)
            }
        }
    }

    /// 真正编译一个方法。内层失败**不**记进解释执行集合（由外层 `compile_into` 记），
    /// 也不负责把它从 `compiled` 里摘掉。
    fn compile_into_inner(
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
        let (max_locals, max_stack0, body, exc_table0, is_static, is_sync) = {
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
            let is_sync = m.access_flags & 0x0020 != 0;
            let (ml, ms, body, table) = code_of(&cf, m)?;
            (ml, ms, body, table, is_static, is_sync)
        };

        // `invokedynamic` 不再在这里整体挡住：能内联的（`StringConcatFactory`）在 0xba
        // 那个点就地展开，接不了的（`LambdaMetafactory` 等）才报错 —— 报错只让**这个方法**
        // 交给解释器执行，调用方会把它记进「解释执行集合」并改发解释器桩。
        // `synchronized` 是**绝不能静默放过**的一类：JIT 拿到监视器表的入口是解释器那条路，
        // 它自己既不会加锁也不会解锁。放过去 = 悄悄丢掉互斥语义（比拒绝更糟）。
        if is_sync {
            return Err(JitError(format!(
                "JIT 不支持 synchronized 方法（ACC_SYNCHRONIZED）：{class}.{name}{desc} 
                 只能由解释器执行（JIT 未接入对象监视器）"
            )));
        }
        if let Some((pc, op)) = find_monitor(&body) {
            let what = if op == 0xc2 { "monitorenter" } else { "monitorexit" };
            return Err(JitError(format!(
                "JIT 不支持同步指令 {what}（0x{op:02x}@{pc}）：{class}.{name}{desc} 
                 只能由解释器执行（JIT 未接入对象监视器）"
            )));
        }
        // 线程/并发：JIT 的堆是 `thread_local` 的（每个线程一个堆、一张静态区），
        // 跨线程共享对象在架构上就不成立 → 明确拒绝，别等运行期报
        // 「类 java/lang/Thread 没有虚方法」。`invokedynamic` 修好之后含
        // `new Thread(() -> …)` 的方法会走到这里，必须在这里挡住。
        if let Some(m) = find_thread_api(&cf, &body) {
            return Err(JitError(format!(
                "JIT 不支持多线程（{m}）：{class}.{name}{desc} 只能由解释器执行"
            )));
        }
        // 同一个道理：`java.lang.invoke.*` 的调用 JIT 也没有接手的地方
        if let Some(m) = find_invoke_api(&cf, &body) {
            return Err(JitError(format!(
                "JIT 不接管 java.lang.invoke（{m}）：{class}.{name}{desc} 只能由解释器执行"
            )));
        }

        // ---- 小方法内联 ----
        // 在**字节码层**把「小 + 纯基本类型 + 无异常表 + 单态」的调用点就地展开
        // （见 `inline.rs`）。跳转/switch 偏移与异常表由它一起重定位，所以后面的
        // 块划分与栈形状分析看到的就是一份自洽的普通字节码。
        // `max_locals` / `max_stack` 都会变大（内联体的槽位排在后面），根区大小随之算。
        let (body, max_locals, stack_extra, exc_table) = {
            let (b, ml, ms, tbl, n) = super::inline::inline_all(
                &self.classes, &cf, class, name, desc, &body, max_locals, max_stack0, &exc_table0,
            );
            self.n_inlined += n;
            (b, ml, ms, tbl)
        };
        // 内联体让操作数栈也可能变深：根区大小是 max_locals + max_stack，
        // 两边都得跟着涨（内联体本身没有 GC 点，但把余量算足不给以后埋雷）
        let max_stack = max_stack0 + stack_extra;
        // 本方法自己会不会触发 GC：不会的话，整帧根区的那对 push/pop 也可以省掉
        // （没有 GC 点就没有对象被搬动，也就没有「回来要刷新」这回事）
        let self_may_gc = gcfree::body_may_gc(&body, !exc_table.is_empty());

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

        // `System.out.print(...)` 与 `System.out.println()`：`println_*` 之外的两个补充桩
        let mut sig_ov = self.module.make_signature();
        sig_ov.params.push(AbiParam::new(types::I32)); // 参数类型标签（`rt::K_*`）
        sig_ov.params.push(AbiParam::new(types::I64)); // 值（double 走位模式）
        let out_val_id = self
            .module
            .declare_function("vmless_out_val", Linkage::Import, &sig_ov)
            .map_err(|e| JitError(e.to_string()))?;
        let sig_nl = self.module.make_signature();
        let out_nl_id = self
            .module
            .declare_function("vmless_out_nl", Linkage::Import, &sig_nl)
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

        // invokedynamic 的字符串拼接桩：`indy_concat(计划编号, i64 值数组) -> 字符串引用`
        let mut sig_concat = self.module.make_signature();
        sig_concat.params.push(AbiParam::new(types::I32));
        sig_concat.params.push(AbiParam::new(types::I64));
        sig_concat.returns.push(AbiParam::new(types::I64));
        let concat_id = self
            .module
            .declare_function("vmless_indy_concat", Linkage::Import, &sig_concat)
            .map_err(|e| JitError(e.to_string()))?;

        // 方法级回退的解释器桩：`interp_call(解释器条目编号, i64 实参数组) -> 返回值`
        let mut sig_interp = self.module.make_signature();
        sig_interp.params.push(AbiParam::new(types::I32));
        sig_interp.params.push(AbiParam::new(types::I64));
        sig_interp.returns.push(AbiParam::new(types::I64));
        let interp_call_id = self
            .module
            .declare_function("vmless_interp_call", Linkage::Import, &sig_interp)
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

        // ---- 帧根区（指针式登记）----
        // 每帧在自己的栈帧里留 max_locals + max_stack 个 u32：**引用型局部变量常驻这里**，
        // 操作数栈上的引用在每个 GC 点前也写进来。
        //
        // 帧入口把这块内存的**地址**交给 GC（`roots_push`），从这一刻起 GC 就能就地
        // 读到本帧的全部引用，并在搬动对象后就地写回 —— 于是**调用点完全不用碰根表**
        // （旧实现是把这块内存拷贝进根表，每个调用点都得 push/pop 一遍，实测
        // 34.7ns/对；16M 次虚调用里它占 83% 的时间）。
        //
        // 地址稳定性：`create_sized_stack_slot` 落在帧内固定偏移上，帧存续期间不变。
        let root_slots = if has_refs { (max_locals + max_stack).max(1) as i32 } else { 0 };
        // 要登记的槽数。本方法**没有任何 GC 点**时不登记 —— 注意 `body_may_gc` 把
        // 任何 invoke 都算 GC 点，所以凡是能发出调用的方法都必然登记，调用点因此
        // 不需要自己的登记。
        let reg = if self_may_gc { root_slots } else { 0 };
        // 根区**本身**只在两种情况下存在：要登记给 GC（有 GC 点），或者字节码里真的
        // 读写了引用型局部变量（那才需要一块「GC 能就地改写」的内存）。
        // 其余情况（`int v() { return 1; }` 这类：实例方法有 `this` 但从不 aload）
        // 连栈槽都不开 —— 省掉每个叶方法序言的清零与 `this` 搬运，收益落在**每一次**
        // 调用上。
        let ref_local_ops = {
            let mut q = 0usize;
            let mut found = false;
            while q < body.len() {
                if matches!(body[q], 0x19 | 0x3a | 0x2a..=0x2d | 0x4b..=0x4e) {
                    found = true;
                    break;
                }
                q += insn_len_at(&body, q).max(1);
            }
            found
        };
        let area_slots = if reg > 0 || ref_local_ops { root_slots } else { 0 };
        let root_area = fb.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
            cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
            (area_slots * 4).max(4) as u32,
            2,
        ));
        // 没有根区时 `root_ptr` 落在任何 spill/load 都不可能到达的路径上，给个 0 即可
        let root_ptr = if area_slots > 0 {
            fb.ins().stack_addr(types::I64, root_area, 0)
        } else {
            fb.ins().iconst(types::I64, 0)
        };
        if area_slots > 0 {
            // 栈槽不保证是 0：必须先清零。指针式登记之后这条更重要 —— GC 会在**任意
            // 时刻**按地址读整块（含还没被 spill 过的槽位），垃圾值会被当成偏移。
            let zero = fb.ins().iconst(types::I32, 0);
            for i in 0..area_slots {
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
        // 整帧只登记这一次（退出时在每条 return 路径上注销，见 `emit_frame_exit`）
        self.emit_roots(&mut fb, root_ptr, reg, true)?;
        // ---- 局部变量驻留寄存器 ----
        // 以前每条 iload/istore 都是一次 `load.i64 locals_ptr+off` / `store`：既付
        // 4~5 周期的 store→load 转发延迟，又让 Cranelift 没法把它们当 SSA 值优化
        // —— 对 locals 的 store 会让它后面的所有 load 全部失效，GVN/常量折叠/LICM
        // 一个也用不上。改成 `cranelift_frontend::Variable`：块边界由
        // FunctionBuilder 自动插 phi 参数（配合收尾的 `seal_all_blocks`），
        // 只有在**真的需要内存**时才落盘：
        //   * 引用型局部变量仍然只住根区（GC 要就地改写它们）
        //   * 同一个槽被两种宽度访问（多类型复用 / 非法字节码）→ 退回内存，行为不变
        let mut lvars = LocalVars::plan(&body, max_locals);
        lvars.init(&mut fb, locals_ptr, desc, is_static);

        // 先把本方法里所有 invokestatic 目标编译好，得到 FuncId 映射
        let mut call_targets: std::collections::HashMap<u16, (cranelift_module::FuncId, usize, i32)> =
            std::collections::HashMap::new();
        // 编不了的目标 → 解释器桩（调用点编号 → (解释器条目 id, 描述符)）
        let mut interp_targets: std::collections::HashMap<u16, (u32, String)> =
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
                            match self.compile_into(&owner, &info.1, &info.2) {
                                Ok((fid, ml, argc)) => {
                                    let extra = if op == 0xb7 { 1 } else { 0 };
                                    call_targets.insert(idx, (fid, ml, argc + extra));
                                }
                                // 方法级回退：目标本身编不了，但它**能跨边界**
                                // （参数/返回值都是基本类型）→ 这个调用点改发解释器桩，
                                // 方法其余部分照旧走 JIT。
                                Err(_) if !crate::vm::has_ref(&info.2) => {
                                    let id = crate::vm::register_interp(
                                        &owner, &info.1, &info.2,
                                    );
                                    interp_targets.insert(idx, (id, info.2.clone()));
                                }
                                // 目标要收/发引用，或构造器：整个方法只能一起回退
                                Err(e) => return Err(e),
                            }
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
        // 上一条指令若是 `l?cmp*`，它的操作数留在这里，供紧跟的条件跳转直接比较。
        // 只活一条指令：主循环每处理一条**非**条件跳转指令就清空（见 `pending = None`），
        // 所以它不可能是「隔了好几条指令的陈旧值」，也就不需要数据流分析。
        let mut pending: Option<PendingCmp> = None;

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
            // `pending` 只在「cmp 的下一条指令」有效；条件跳转由它自己 take()
            if !matches!(op, 0x99..=0x9e) {
                pending = None;
            }
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
                    let v = lvars.load(&mut fb, locals_ptr, slot, types::I32);
                    stack.push(v);
                }
                // fload / fload_0..3：局部变量按 F32 读（槽是 8 字节，只读低 4 字节）
                0x17 => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = lvars.load(&mut fb, locals_ptr, slot, types::F32);
                    stack.push(v);
                }
                0x22..=0x25 => {
                    let slot = (op - 0x22) as usize;
                    let v = lvars.load(&mut fb, locals_ptr, slot, types::F32);
                    stack.push(v);
                }
                0x43..=0x46 => {
                    // fstore_0..3
                    let slot = (op - 0x43) as usize;
                    let v = pop_ck(&mut stack, op, pc)?;
                    lvars.store(&mut fb, locals_ptr, slot, types::F32, v);
                }
                0x1a..=0x1d => {
                    let slot = (op - 0x1a) as usize;
                    let v = lvars.load(&mut fb, locals_ptr, slot, types::I32);
                    stack.push(v);
                }
                0x36 => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = pop_ck(&mut stack, op, pc)?;
                    lvars.store(&mut fb, locals_ptr, slot, types::I32, v);
                }
                0x3b..=0x3e => {
                    let slot = (op - 0x3b) as usize;
                    let v = pop_ck(&mut stack, op, pc)?;
                    lvars.store(&mut fb, locals_ptr, slot, types::I32, v);
                }
                0x84 => {
                    // iinc
                    let slot = body[pc] as usize;
                    let delta = body[pc + 1] as i8 as i64;
                    pc += 2;
                    let v = lvars.load(&mut fb, locals_ptr, slot, types::I32);
                    let nv = fb.ins().iadd_imm(v, delta);
                    lvars.store(&mut fb, locals_ptr, slot, types::I32, nv);
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
                    let v = peek_ck(&stack, op, pc)?;
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
                    let top = peek_ck(&stack, op, pc)?;
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
                // idiv / irem：除数是编译期常量时换乘魔数（idiv 是 20~40 周期的串行指令）
                0x6c | 0x70 => {
                    let b = pop_ck(&mut stack, op, pc)?;
                    let a = pop_ck(&mut stack, op, pc)?;
                    // 除数 0 不能走乘魔数（那会变成一次不抛异常的除法），留着走 idiv
                    let dv = iconst_of(&fb, b).filter(|d| *d != 0);
                    let v = match (dv, op) {
                        (Some(d), 0x6c) => divmod::emit_sdiv_const(
                            &mut fb,
                            a,
                            d as i32 as i64,
                            32,
                        ),
                        (Some(d), _) => divmod::emit_srem_const(
                            &mut fb,
                            a,
                            d as i32 as i64,
                            32,
                        ),
                        (None, 0x6c) => fb.ins().sdiv(a, b),
                        (None, _) => fb.ins().srem(a, b),
                    };
                    stack.push(v);
                }
                0x74 => {
                    let a = pop_ck(&mut stack, op, pc)?;
                    stack.push(fb.ins().ineg(a));
                }
                // 与 0 比较的条件跳转
                0x99..=0x9e => {
                    let off = i16::from_be_bytes([body[pc], body[pc + 1]]) as isize;
                    pc += 2;
                    let target = (pc as isize - 3 + off) as usize;
                    let tb = *blocks.get(&target).ok_or_else(|| JitError("缺少目标块".into()))?;
                    let fall = *blocks.get(&pc).ok_or_else(|| JitError("缺少落空块".into()))?;
                    // 快路：紧跟在 lcmp/fcmp/dcmp 后面 —— 直接在原始操作数上比较。
                    // 慢路（下面那个 else）：把 cmp 结果与 0 比较。
                    let cond = match pending.take() {
                        Some(p) if stack.last() == Some(&p.pushed) => {
                            stack.pop();
                            p.emit(&mut fb, op)
                        }
                        _ => {
                            let v = pop_ck(&mut stack, op, pc)?;
                            let zero = fb.ins().iconst(types::I32, 0);
                            let cc = match op {
                                0x99 => IntCC::Equal,
                                0x9a => IntCC::NotEqual,
                                0x9b => IntCC::SignedLessThan,
                                0x9c => IntCC::SignedGreaterThanOrEqual,
                                0x9d => IntCC::SignedGreaterThan,
                                _ => IntCC::SignedLessThanOrEqual,
                            };
                            fb.ins().icmp(cc, v, zero)
                        }
                    };
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
                    let v = pop_ck(&mut stack, op, pc)?;
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
                    let b = pop_ck(&mut stack, op, pc)?;
                    let a = pop_ck(&mut stack, op, pc)?;
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
                    let key = pop_ck(&mut stack, op, pc)?;
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
                    let v = pop_ck(&mut stack, op, pc)?;
                    let v = convert_to(&mut fb, v, ret_ty);
                    if has_refs {
                        self.emit_frame_exit(&mut fb, root_ptr, reg)?;
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
                        self.emit_frame_exit(&mut fb, root_ptr, reg)?;
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
                    let v = pop_ck(&mut stack, op, pc)?;
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
                            reg,
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
                    if let Some(sop) = crate::jdk::sop(&bcls, &bname, &bdesc) {
                        // 内建静态方法（`Integer.parseInt` / `System.arraycopy` …）
                        let params = param_descs(&bdesc);
                        let mut args = Vec::new();
                        for _ in 0..params.len() {
                            args.push(pop_ck(&mut stack, op, pc)?);
                        }
                        args.reverse();
                        let op_pc = pc - 3;
                        let refs_pc = refs.at.get(&op_pc).cloned().unwrap_or_default();
                        spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
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
                            let oi = fb.ins().iconst(types::I32, sop as i64);
                            let fref =
                                self.module.declare_func_in_func(builtin_static_id, fb.func);
                            let c = fb.ins().call(fref, &[oi, a64, b64]);
                            fb.inst_results(c)[0]
                        };
                        reload_refs(&mut fb, &mut stack, &refs_pc, root_ptr, max_locals);
                        let ret = bdesc.rsplit(')').next().unwrap_or("V").to_string();
                        if ret != "V" {
                            let v = abi_result(&mut fb, raw, &ret);
                            stack.push(v);
                        }
                    } else if let Some((mid, mdesc)) = interp_targets.get(&idx) {
                        // 方法级回退：被调方法编不了，改发解释器桩（参数与返回值都是
                        // 基本类型，跨边界安全 —— 见 `vm::bridge`）。
                        let mdesc = mdesc.clone();
                        let params = param_descs(&mdesc);
                        let mut args = Vec::new();
                        for _ in 0..params.len() {
                            args.push(pop_ck(&mut stack, op, pc)?);
                        }
                        args.reverse();
                        let slot = fb.create_sized_stack_slot(
                            cranelift_codegen::ir::StackSlotData::new(
                                cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
                                (args.len().max(1) * 8) as u32,
                                3,
                            ),
                        );
                        for (i, (v, p)) in args.iter().zip(params.iter()).enumerate() {
                            let raw = abi_arg(&mut fb, *v, p);
                            let addr = fb.ins().stack_addr(types::I64, slot, (i * 8) as i32);
                            fb.ins().store(MemFlags::new(), raw, addr, 0);
                        }
                        let ptr = fb.ins().stack_addr(types::I64, slot, 0);
                        let op_pc = pc - 3;
                        let refs_pc = refs.at.get(&op_pc).cloned().unwrap_or_default();
                        spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
                        let iid = fb.ins().iconst(types::I32, *mid as i64);
                        let fref = self.module.declare_func_in_func(interp_call_id, fb.func);
                        let c = fb.ins().call(fref, &[iid, ptr]);
                        reload_refs(&mut fb, &mut stack, &refs_pc, root_ptr, max_locals);
                        let ret = mdesc.rsplit(')').next().unwrap_or("V").to_string();
                        if ret != "V" {
                            let raw = fb.inst_results(c)[0];
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
                    // 首次调用这个类的静态方法时先跑它的 <clinit>
                    let callee_owner = method_ref_of(&cf, idx)?.0;
                    self.emit_clinit_call(&mut fb, &callee_owner)?;
                    reload_refs(&mut fb, &mut stack, &refs_pc, root_ptr, max_locals);
                    spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
                    // 参数出栈（逆序），写进为被调方法准备的栈槽
                    let mut args = Vec::new();
                    for _ in 0..argc {
                        args.push(pop_ck(&mut stack, op, pc)?);
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
                            reg,
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
                    let v = pop_ck(&mut stack, op, pc)?;
                    store_root(&mut fb, root_ptr, slot, v);
                }
                0x3a => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = pop_ck(&mut stack, op, pc)?;
                    store_root(&mut fb, root_ptr, slot, v);
                }
                0x59 => {
                    let v = peek_ck(&stack, op, pc)?;
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
                    let mut args = Vec::new();
                    for _ in 0..argc {
                        args.push(pop_ck(&mut stack, op, pc)?);
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
                    let v = lvars.load(&mut fb, locals_ptr, slot, types::F64);
                    stack.push(v);
                }
                0x26..=0x29 => {
                    // dload_0..3
                    let slot = (op - 0x26) as usize;
                    let v = lvars.load(&mut fb, locals_ptr, slot, types::F64);
                    stack.push(v);
                }
                0x38 | 0x39 => {
                    // fstore(0x38) / dstore(0x39)：都只是把栈顶值按值存进 8 字节槽
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = pop_ck(&mut stack, op, pc)?;
                    // 按**值本身的类型**存（不按操作码猜），与旧行为一致
                    let ty = fb.func.dfg.value_type(v);
                    lvars.store(&mut fb, locals_ptr, slot, ty, v);
                }
                0x47..=0x4a => {
                    // dstore_0..3
                    let slot = (op - 0x47) as usize;
                    let v = pop_ck(&mut stack, op, pc)?;
                    lvars.store(&mut fb, locals_ptr, slot, types::F64, v);
                }
                0x16 => {
                    // lload
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = lvars.load(&mut fb, locals_ptr, slot, types::I64);
                    stack.push(v);
                }
                0x1e..=0x21 => {
                    // lload_0..3
                    let slot = (op - 0x1e) as usize;
                    let v = lvars.load(&mut fb, locals_ptr, slot, types::I64);
                    stack.push(v);
                }
                0x37 => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = pop_ck(&mut stack, op, pc)?;
                    lvars.store(&mut fb, locals_ptr, slot, types::I64, v);
                }
                0x3f..=0x42 => {
                    let slot = (op - 0x3f) as usize;
                    let v = pop_ck(&mut stack, op, pc)?;
                    lvars.store(&mut fb, locals_ptr, slot, types::I64, v);
                }
                0x61 => lng(&mut fb, &mut stack, |f, a, b| f.ins().iadd(a, b))?,
                0x65 => lng(&mut fb, &mut stack, |f, a, b| f.ins().isub(a, b))?,
                0x69 => lng(&mut fb, &mut stack, |f, a, b| f.ins().imul(a, b))?,
                0x6d | 0x71 => {
                    let b = pop_ck(&mut stack, op, pc)?;
                    let a = pop_ck(&mut stack, op, pc)?;
                    let dv = iconst_of(&fb, b).filter(|d| *d != 0);
                    let v = match (dv, op) {
                        (Some(d), 0x6d) => divmod::emit_sdiv_const(&mut fb, a, d, 64),
                        (Some(d), _) => divmod::emit_srem_const(&mut fb, a, d, 64),
                        (None, 0x6d) => fb.ins().sdiv(a, b),
                        (None, _) => fb.ins().srem(a, b),
                    };
                    stack.push(v);
                }
                0x75 => {
                    let b = pop_ck(&mut stack, op, pc)?;
                    stack.push(fb.ins().ineg(b));
                }
                // double 算术 / 取反
                0x63 => lng(&mut fb, &mut stack, |f, a, b| f.ins().fadd(a, b))?,
                0x67 => lng(&mut fb, &mut stack, |f, a, b| f.ins().fsub(a, b))?,
                0x6b => lng(&mut fb, &mut stack, |f, a, b| f.ins().fmul(a, b))?,
                0x6f => lng(&mut fb, &mut stack, |f, a, b| f.ins().fdiv(a, b))?,
                0x73 => {
                    // drem：cranelift 没有 frem 指令，调 CRT 的 fmod
                    let b = pop_ck(&mut stack, op, pc)?;
                    let a = pop_ck(&mut stack, op, pc)?;
                    let fref = self.module.declare_func_in_func(drem_id, fb.func);
                    let c = fb.ins().call(fref, &[a, b]);
                    stack.push(fb.inst_results(c)[0]);
                }
                0x77 => {
                    let b = pop_ck(&mut stack, op, pc)?;
                    stack.push(fb.ins().fneg(b));
                }
                // float 算术 / 取反（F32 指令，f32 舍入由指令本身保证）
                0x62 => lng(&mut fb, &mut stack, |f, a, b| f.ins().fadd(a, b))?,
                0x66 => lng(&mut fb, &mut stack, |f, a, b| f.ins().fsub(a, b))?,
                0x6a => lng(&mut fb, &mut stack, |f, a, b| f.ins().fmul(a, b))?,
                0x6e => lng(&mut fb, &mut stack, |f, a, b| f.ins().fdiv(a, b))?,
                0x72 => {
                    // frem：cranelift 没有 frem 指令，调 CRT 的 fmodf
                    let b = pop_ck(&mut stack, op, pc)?;
                    let a = pop_ck(&mut stack, op, pc)?;
                    let fref = self.module.declare_func_in_func(frem_id, fb.func);
                    let c = fb.ins().call(fref, &[a, b]);
                    stack.push(fb.inst_results(c)[0]);
                }
                0x76 => {
                    let b = pop_ck(&mut stack, op, pc)?;
                    stack.push(fb.ins().fneg(b));
                }
                // 数值类型转换
                0x87 => {
                    // i2d
                    let a = pop_ck(&mut stack, op, pc)?;
                    stack.push(fb.ins().fcvt_from_sint(types::F64, a));
                }
                0x8a => {
                    // l2d
                    let a = pop_ck(&mut stack, op, pc)?;
                    stack.push(fb.ins().fcvt_from_sint(types::F64, a));
                }
                0x8e => {
                    // d2i
                    let a = pop_ck(&mut stack, op, pc)?;
                    stack.push(fb.ins().fcvt_to_sint_sat(types::I32, a));
                }
                0x8f => {
                    // d2l
                    let a = pop_ck(&mut stack, op, pc)?;
                    stack.push(fb.ins().fcvt_to_sint_sat(types::I64, a));
                }
                // long / double 比较：压出 -1 / 0 / 1
                0x94 => {
                    // lcmp
                    let b = pop_ck(&mut stack, op, pc)?;
                    let a = pop_ck(&mut stack, op, pc)?;
                    let lt = fb.ins().icmp(IntCC::SignedLessThan, a, b);
                    let gt = fb.ins().icmp(IntCC::SignedGreaterThan, a, b);
                    let lt = fb.ins().uextend(types::I32, lt);
                    let gt = fb.ins().uextend(types::I32, gt);
                    let d = fb.ins().isub(gt, lt);
                    pending = Some(PendingCmp { a, b, pushed: d, kind: CmpKind::Long });
                    stack.push(d);
                }
                0x97 | 0x98 => {
                    // dcmpl(0x97: NaN → -1) / dcmpg(0x98: NaN → +1)
                    let b = pop_ck(&mut stack, op, pc)?;
                    let a = pop_ck(&mut stack, op, pc)?;
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
                    pending = Some(PendingCmp { a, b, pushed: d, kind: CmpKind::Double(op == 0x98) });
                    stack.push(d);
                }
                0x95 | 0x96 => {
                    // fcmpl(0x95: NaN → -1) / fcmpg(0x96: NaN → +1)
                    let b = pop_ck(&mut stack, op, pc)?;
                    let a = pop_ck(&mut stack, op, pc)?;
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
                    pending = Some(PendingCmp { a, b, pushed: d, kind: CmpKind::Float(op == 0x96) });
                    stack.push(d);
                }
                0x88 => {
                    // l2i
                    let a = pop_ck(&mut stack, op, pc)?;
                    stack.push(fb.ins().ireduce(types::I32, a));
                }
                0x85 => {
                    // i2l
                    let a = pop_ck(&mut stack, op, pc)?;
                    stack.push(fb.ins().sextend(types::I64, a));
                }
                // float 与其它数值类型的互转（F32 参与，位宽按目标类型走）
                0x86 => {
                    // i2f
                    let a = pop_ck(&mut stack, op, pc)?;
                    stack.push(fb.ins().fcvt_from_sint(types::F32, a));
                }
                0x89 => {
                    // l2f
                    let a = pop_ck(&mut stack, op, pc)?;
                    stack.push(fb.ins().fcvt_from_sint(types::F32, a));
                }
                0x8b => {
                    // f2i（Rust/Cranelift 的饱和转换与 Java 语义一致：NaN → 0，超界钳到边界）
                    let a = pop_ck(&mut stack, op, pc)?;
                    stack.push(fb.ins().fcvt_to_sint_sat(types::I32, a));
                }
                0x8c => {
                    // f2l
                    let a = pop_ck(&mut stack, op, pc)?;
                    stack.push(fb.ins().fcvt_to_sint_sat(types::I64, a));
                }
                0x8d => {
                    // f2d
                    let a = pop_ck(&mut stack, op, pc)?;
                    stack.push(fb.ins().fpromote(types::F64, a));
                }
                0x90 => {
                    // d2f
                    let a = pop_ck(&mut stack, op, pc)?;
                    stack.push(fb.ins().fdemote(types::F32, a));
                }
                // i2b / i2c / i2s：截断到目标宽度后**符号扩展**回 int
                0x91 => {
                    let a = pop_ck(&mut stack, op, pc)?;
                    let a = fb.ins().ireduce(types::I8, a);
                    stack.push(fb.ins().sextend(types::I32, a));
                }
                0x92 => {
                    let a = pop_ck(&mut stack, op, pc)?;
                    stack.push(fb.ins().band_imm(a, 0xFFFF));
                }
                0x93 => {
                    let a = pop_ck(&mut stack, op, pc)?;
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
                    let v = pop_ck(&mut stack, op, pc)?;
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
                    if name == "println" || name == "print" {
                        // 栈上顺序是 [接收者, 参数]。**`println()` / `print()` 没有参数**：
                        // 无条件弹两个会让它们（以及它们之后的全部指令）报「栈下溢」——
                        // R5.main 末尾那句 `System.out.println()` 就是这么炸的。
                        let params = param_descs(&desc);
                        // 引用型实参要先过文本化判据（覆盖了 `toString()` 的类型这里给不出
                        // 正确文本 → 整个方法回退解释器）
                        if let Some(p) = params.first() {
                            if is_ref_desc(p) && !ref_text_ok(&self.classes, p) {
                                return Err(JitError(format!(
                                    "System.out.{name} 的引用实参 {p} 需要虚 toString 分派（JIT 未实现）"
                                )));
                            }
                        }
                        let arg = match params.first() {
                            Some(_) => Some(pop_ck(&mut stack, op, pc)?),
                            None => None,
                        };
                        let _recv = pop_ck(&mut stack, op, pc)?;
                        if name == "print" {
                            // 不换行的那一套：一个通用桩按参数类型格式化
                            if let Some(v) = arg {
                                let kind = match params[0].as_bytes()[0] {
                                    b'Z' => rt::K_BOOL,
                                    b'C' => rt::K_CHAR,
                                    b'I' | b'S' | b'B' => rt::K_INT,
                                    b'J' => rt::K_LONG,
                                    b'F' => rt::K_FLOAT,
                                    b'D' => rt::K_DOUBLE,
                                    _ => rt::K_REF,
                                };
                                let raw = abi_arg(&mut fb, v, &params[0]);
                                let ki = fb.ins().iconst(types::I32, kind as i64);
                                let fref = self.module.declare_func_in_func(out_val_id, fb.func);
                                fb.ins().call(fref, &[ki, raw]);
                            }
                        } else if let Some(v) = arg {
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
                        } else {
                            // println()：只换行
                            let fref = self.module.declare_func_in_func(out_nl_id, fb.func);
                            fb.ins().call(fref, &[]);
                        }
                    } else if cls == "java/lang/StringBuilder" {
                        // 内置：字符串拼接（`StringBuilder` 没有 .class，进不了虚方法表）
                        if name == "toString" {
                            let recv = pop_ck(&mut stack, op, pc)?;
                            let obj = convert_to(&mut fb, recv, types::I64);
                            let op_pc = pc - if op == 0xb9 { 5 } else { 3 };
                            let refs_pc = refs.at.get(&op_pc).cloned().unwrap_or_default();
                            spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
                            let fref = self.module.declare_func_in_func(sb_to_str_id, fb.func);
                            let c = fb.ins().call(fref, &[obj]);
                            reload_refs(&mut fb, &mut stack, &refs_pc, root_ptr, max_locals);
                            stack.push(fb.inst_results(c)[0]);
                        } else if name == "append" {
                            let v = pop_ck(&mut stack, op, pc)?;
                            let recv = pop_ck(&mut stack, op, pc)?;
                            // 引用型实参：同 `println`，覆盖了 `toString()` 就给不出正确文本
                            if let Some(p) = param_descs(&desc).first() {
                                if is_ref_desc(p) && !ref_text_ok(&self.classes, p) {
                                    return Err(JitError(format!(
                                        "StringBuilder.append 的引用实参 {p} 需要虚 toString 分派（JIT 未实现）"
                                    )));
                                }
                            }
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
                        let v = pop_ck(&mut stack, op, pc)?;
                        let recv = pop_ck(&mut stack, op, pc)?;
                        let a = convert_to(&mut fb, recv, types::I64);
                        let b = convert_to(&mut fb, v, types::I64);
                        let fref = self.module.declare_func_in_func(str_equals_id, fb.func);
                        let call = fb.ins().call(fref, &[a, b]);
                        let res = fb.inst_results(call)[0];
                        stack.push(res);
                    } else if let Some(vop) = self.builtin_vop(&cls, &name, &desc) {
                        // 内建 JDK 类：统一走 `vmless_builtin_vcall(recv, op, a, b)`
                        let params = param_descs(&desc);
                        let mut args = Vec::new();
                        for _ in 0..params.len() {
                            args.push(pop_ck(&mut stack, op, pc)?);
                        }
                        args.reverse();
                        let recv = pop_ck(&mut stack, op, pc)?;
                        let op_pc = pc - if op == 0xb9 { 5 } else { 3 };
                        let refs_pc = refs.at.get(&op_pc).cloned().unwrap_or_default();
                        spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
                        let recv64 = convert_to(&mut fb, recv, types::I64);
                        let a64 = match args.first() {
                            Some(v) => abi_arg(&mut fb, *v, &params[0]),
                            None => fb.ins().iconst(types::I64, 0),
                        };
                        let b64 = match args.get(1) {
                            Some(v) => abi_arg(&mut fb, *v, &params[1]),
                            None => fb.ins().iconst(types::I64, 0),
                        };
                        let oi = fb.ins().iconst(types::I32, vop as i64);
                        let fref = self.module.declare_func_in_func(builtin_vcall_id, fb.func);
                        let c = fb.ins().call(fref, &[recv64, oi, a64, b64]);
                        let raw = fb.inst_results(c)[0];
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
                            args.push(pop_ck(&mut stack, op, pc)?);
                        }
                        args.reverse();
                        // 被调方法的 locals 布局：槽 0 是 this，参数从槽 1 起按描述符排
                        let (ml, callee_gc_free) = self.vcall_max_locals(&cls, &name, &desc)?;
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
                        // 所有实现都不会 GC 时：这对根区登记纯属浪费（没有 GC 就没有搬动），
                        // 连 spill/reload 都不用做 —— 操作数栈留在 SSA 值里即可
                        if !callee_gc_free {
                            spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
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
                        if !callee_gc_free {
                            reload_refs(&mut fb, &mut stack, &refs_pc, root_ptr, max_locals);
                        }
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
                            reg,
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
                        let fref = self.module.declare_func_in_func(sb_new_id, fb.func);
                        let c = fb.ins().call(fref, &[]);
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
                    self.emit_clinit_call(&mut fb, &cls)?;
                    reload_refs(&mut fb, &mut stack, &refs_pc, root_ptr, max_locals);
                    // 分配本身也是 GC 点
                    spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
                    let cid = fb.ins().iconst(types::I32, class_id as i64);
                    let fref = self.module.declare_func_in_func(new_obj_id, fb.func);
                    let c = fb.ins().call(fref, &[cid]);
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
                    let p = pop_ck(&mut stack, op, pc)?;
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
                    let v = pop_ck(&mut stack, op, pc)?;
                    let p = pop_ck(&mut stack, op, pc)?;
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
                    let n = pop_ck(&mut stack, op, pc)?;
                    let n = convert_to(&mut fb, n, types::I32);
                    let refs_pc = refs.at.get(&(pc - 2)).cloned().unwrap_or_default();
                    spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
                    let v = push_new_array(&mut self.module, &mut fb, new_arr_id, desc, n);
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
                        counts.push(pop_ck(&mut stack, op, pc)?);
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
                    let fref = self.module.declare_func_in_func(new_multi_id, fb.func);
                    let c = fb.ins().call(fref, &[ptr, nd, sp]);
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
                    let n = pop_ck(&mut stack, op, pc)?;
                    let n = convert_to(&mut fb, n, types::I32);
                    let refs_pc = refs.at.get(&(pc - 3)).cloned().unwrap_or_default();
                    spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
                    let v = push_new_array(&mut self.module, &mut fb, new_arr_id, &desc, n);
                    reload_refs(&mut fb, &mut stack, &refs_pc, root_ptr, max_locals);
                    stack.push(v);
                }
                // arraylength（长度在偏移 0）
                0xbe => {
                    let p = pop_ck(&mut stack, op, pc)?;
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
                    let idx = pop_ck(&mut stack, op, pc)?;
                    let p = pop_ck(&mut stack, op, pc)?;
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
                    let v = pop_ck(&mut stack, op, pc)?;
                    let idx = pop_ck(&mut stack, op, pc)?;
                    let p = pop_ck(&mut stack, op, pc)?;
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
                    let obj = pop_ck(&mut stack, op, pc)?;
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
                    let obj = pop_ck(&mut stack, op, pc)?;
                    let obj = convert_to(&mut fb, obj, types::I64);
                    let tg = fb.ins().iconst(types::I32, tag as i64);
                    let fref = self.module.declare_func_in_func(checkcast_id, fb.func);
                    let c = fb.ins().call(fref, &[obj, tg]);
                    stack.push(fb.inst_results(c)[0]);
                }
                // 移位：int 版本两个操作数都是 I32；long 版本的位数由 I32 扩展成 I64
                0x78 | 0x7a | 0x7c => {
                    let b = pop_ck(&mut stack, op, pc)?;
                    let a = pop_ck(&mut stack, op, pc)?;
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
                    let b = pop_ck(&mut stack, op, pc)?;
                    let a = pop_ck(&mut stack, op, pc)?;
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
                    let b = pop_ck(&mut stack, op, pc)?;
                    let a = pop_ck(&mut stack, op, pc)?;
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
                    let b = pop_ck(&mut stack, op, pc)?;
                    let a = pop_ck(&mut stack, op, pc)?;
                    let a = convert_to(&mut fb, a, types::I64);
                    let b = convert_to(&mut fb, b, types::I64);
                    let v = match op {
                        0x7f => fb.ins().band(a, b),
                        0x81 => fb.ins().bor(a, b),
                        _ => fb.ins().bxor(a, b),
                    };
                    stack.push(v);
                }
                // invokedynamic：编译期解码（`StringConcatFactory` 内联成一条拼接计划 +
                // 一次运行时调用）。接不了的引导方法在这里报错 → **只这个方法**回退解释器。
                0xba => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    let op_pc = pc - 1;
                    pc += 4; // 2 字节索引 + 2 个保留字节（JVMS 规定必须为 0）
                    let site = super::indy::classify_in(&cf, class, idx).map_err(|why| {
                        JitError(format!(
                            "JIT 不支持 invokedynamic（0xba@{op_pc}，{why}）：{class}.{name}{desc}"
                        ))
                    })?;
                    match site {
                        super::indy::Site::Lambda(l) => {
                            // 捕获实参先留在**操作数栈**上：分配 lambda 对象是 GC 点，
                            // 留在栈上的引用会被 `spill_refs` 写进根区、搬动后能读回来。
                            let class_id = self.spin_lambda(&l)?;
                            let ncap = l.captured.len();
                            let refs_pc = refs.at.get(&op_pc).cloned().unwrap_or_default();
                            spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
                            let cid = fb.ins().iconst(types::I32, class_id as i64);
                            let fref = self.module.declare_func_in_func(new_obj_id, fb.func);
                            let c = fb.ins().call(fref, &[cid]);
                            reload_refs(&mut fb, &mut stack, &refs_pc, root_ptr, max_locals);
                            let obj = fb.inst_results(c)[0];
                            // 捕获变量进字段：引用走写屏障，基本类型直接存 8 字节槽
                            let mut vals = Vec::with_capacity(ncap);
                            for _ in 0..ncap {
                                vals.push(pop_ck(&mut stack, op, pc)?);
                            }
                            vals.reverse();
                            for (i, (v, d)) in vals.iter().zip(l.captured.iter()).enumerate() {
                                if is_ref_desc(d) {
                                    let si = fb.ins().iconst(
                                        types::I32,
                                        super::layout::field_slot_idx(i) as i64,
                                    );
                                    let v64 = convert_to(&mut fb, *v, types::I64);
                                    let sref =
                                        self.module.declare_func_in_func(set_ref_id, fb.func);
                                    fb.ins().call(sref, &[obj, si, v64]);
                                } else {
                                    let (ty, _) = field_slot_type(d);
                                    let v = convert_to(&mut fb, *v, ty);
                                    let addr = emit_ref_addr(&mut fb, obj);
                                    fb.ins().store(
                                        MemFlags::new(),
                                        v,
                                        addr,
                                        super::layout::field_off(i),
                                    );
                                }
                            }
                            stack.push(obj);
                        }
                        super::indy::Site::Concat { plan, params } => {
                            // 引用型动态参数同样要先过文本化判据
                            for p in &params {
                                if is_ref_desc(p) && !ref_text_ok(&self.classes, p) {
                                    return Err(JitError(format!(
                                        "字符串拼接的动态参数 {p} 需要虚 toString 分派（JIT 未实现）"
                                    )));
                                }
                            }
                            let mut args = Vec::with_capacity(params.len());
                            for _ in 0..params.len() {
                                args.push(pop_ck(&mut stack, op, pc)?);
                            }
                            args.reverse();
                            // 参数一律拍成 i64（float/double 走位模式），运行时按计划解码
                            let slot = fb.create_sized_stack_slot(
                                cranelift_codegen::ir::StackSlotData::new(
                                    cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
                                    (args.len().max(1) * 8) as u32,
                                    3,
                                ),
                            );
                            for (i, (v, p)) in args.iter().zip(params.iter()).enumerate() {
                                let raw = abi_arg(&mut fb, *v, p);
                                let addr = fb.ins().stack_addr(types::I64, slot, (i * 8) as i32);
                                fb.ins().store(MemFlags::new(), raw, addr, 0);
                            }
                            let ptr = fb.ins().stack_addr(types::I64, slot, 0);
                            let refs_pc = refs.at.get(&op_pc).cloned().unwrap_or_default();
                            spill_refs(&mut fb, &stack, &refs_pc, root_ptr, max_locals);
                            let pi = fb.ins().iconst(types::I32, plan as i64);
                            let fref = self.module.declare_func_in_func(concat_id, fb.func);
                            let c = fb.ins().call(fref, &[pi, ptr]);
                            reload_refs(&mut fb, &mut stack, &refs_pc, root_ptr, max_locals);
                            stack.push(fb.inst_results(c)[0]);
                        }
                    }
                }
                other => return Err(JitError(format!("JIT 暂不支持字节码 0x{other:02x}"))),
            }
        }
        // 所有还没终结的块统一补该返回类型的零值
        for b in open {
            fb.switch_to_block(b);
            let z = zero_of(&mut fb, ret_ty);
            if has_refs {
                self.emit_frame_exit(&mut fb, root_ptr, reg)?;
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
    /// 返回 (各实现里最大的 locals 槽数, 是否**所有**实现都不会触发 GC)。
    ///
    /// 第二个返回值决定调用点要不要发那对 `roots_push`/`roots_pop`：
    /// 没有 GC 点就没有对象被搬动，那对登记是纯浪费（实测占虚调用成本的 83%）。
    fn vcall_max_locals(
        &mut self,
        static_class: &str,
        name: &str,
        desc: &str,
    ) -> Result<(usize, bool), JitError> {
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
        // 槽位至少要装下「接收者 + 全部实参」。以前这里从 1 起、只按找到的实现放大，
        // 于是「静态类型下一个实现都没找到」（JDK 接口、lambda 合成类）时调用点会拿
        // 8 字节的栈槽去写 `this` + 实参 —— 直接写出栈槽外面（实测踩到返回地址）。
        let need = 1 + param_slots(desc).iter().copied().max().map(|m| m + 1).unwrap_or(0);
        let mut ml_max = need;
        let mut all_gc_free = true;
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
            if self.method_may_gc(&owner, name, desc) {
                all_gc_free = false;
            }
            let (fid, ml, _) = self.compile_into(&owner, name, desc)?;
            ml_max = ml_max.max(ml);
            let tag = rt::class_tag(&k);
            if !self.pending_v.iter().any(|(t, s, _, _)| *t == tag && *s == sig) {
                self.pending_v.push((tag, sig, fid, ret_kind));
            }
        }
        Ok((ml_max, all_gc_free))
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
) -> Result<(), JitError> {
    let b = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
    let a = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
    let v = f(fb, a, b);
    stack.push(v);
    Ok(())
}

/// 该 `Value` 是不是 `iconst` 直接定义的常量（`ldc` / `bipush` / `iconst_*` 都编成它）。
/// 用来做「除数是常量 → 换乘魔数」「移位量是常量 → 用 imm 形式」这类判定。
fn iconst_of(fb: &FunctionBuilder, v: Value) -> Option<i64> {
    match fb.func.dfg.value_def(v) {
        cranelift_codegen::ir::ValueDef::Result(inst, 0) => {
            match fb.func.dfg.insts[inst] {
                cranelift_codegen::ir::InstructionData::UnaryImm {
                    opcode: cranelift_codegen::ir::Opcode::Iconst,
                    imm,
                } => {
                    let bits = imm.bits();
                    if fb.func.dfg.value_type(v) == types::I32 {
                        Some(bits as i32 as i64)
                    } else {
                        Some(bits)
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn const_int(cf: &ClassFile, idx: u16) -> i64 {    match cf.constant_pool.get(idx as usize).and_then(|e| e.as_ref()) {
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

// ---------- 局部变量驻留 ----------

// ---------- cmp + 条件跳转融合 ----------

/// `lcmp` / `fcmpl|fcmpg` / `dcmpl|dcmpg` 压出的那个 `-1/0/1`，等在**紧跟的**一条
/// 条件跳转里被消费。
///
/// **为什么要这条快路**：`lcmp` 这里是「两次 icmp + 两次 uextend + isub」，条件跳转
/// 再补一条「iconst 0 + icmp」—— 一共 7 条指令，只为了问一个 `a < b`。而
/// `while (i < n)` 这种循环**每个迭代**都要付这一遍。实测 `V0`（`acc += j` 的空循环）
/// 32M 次迭代：融合前 35 ms，融合后 18 ms，与 HotSpot 25 持平。代价是栈上多一条
/// `-1/0/1` 的计算，它没有使用者，交给 Cranelift 的 DCE 删掉。
///
/// 只认「紧邻」这一种形状：中间隔了任何别的指令就 `pending` 失效（见主循环里的
/// `pending = None`）。这样不需要数据流分析，也就不可能配错栈位。
#[derive(Clone, Copy)]
struct PendingCmp {
    a: Value,
    b: Value,
    /// 这个 cmp 压进栈的那个值：用它核对「栈顶确实是它」，防止快路吃错操作数
    pushed: Value,
    kind: CmpKind,
}

#[derive(Clone, Copy, PartialEq)]
enum CmpKind {
    /// `lcmp`：-1/0/1，无 NaN 语义
    Long,
    /// `fcmpl` / `fcmpg`：NaN 分别折成 -1 / +1
    Float(bool),
    /// `dcmpl` / `dcmpg`：同上
    Double(bool),
}

/// 融合后要用的那一条比较条件
enum CmpCond {
    Int(IntCC),
    Float(FloatCC),
}

impl PendingCmp {
    /// `op` = 条件跳转的操作码（0x99 ifeq … 0x9e ifle）
    ///
    /// 展开依据：cmp 压出的结果 `r` 与操作数 `a`/`b` 的关系
    /// * `lcmp`：`r = sign(a - b)`
    /// * `*cmpl`：`r = NaN 时 -1`，否则 `sign(a - b)`
    /// * `*cmpg`：`r = NaN 时 +1`，否则 `sign(a - b)`
    ///
    /// 所以 `r < 0` 在 `cmpl` 下是「无序 **或** a<b」（`UnorderedOrLessThan`），
    /// 在 `cmpg` 下才是纯 `LessThan` —— 二者不能混，混了就是 NaN 语义出错。
    fn emit(self, fb: &mut FunctionBuilder, op: u8) -> Value {
        match self.kind {
            CmpKind::Long => {
                let cc = match op {
                    0x99 => IntCC::Equal,
                    0x9a => IntCC::NotEqual,
                    0x9b => IntCC::SignedLessThan,
                    0x9c => IntCC::SignedGreaterThanOrEqual,
                    0x9d => IntCC::SignedGreaterThan,
                    _ => IntCC::SignedLessThanOrEqual,
                };
                fb.ins().icmp(cc, self.a, self.b)
            }
            CmpKind::Float(nan_high) | CmpKind::Double(nan_high) => {
                // `nan_high` = 这条 cmp 把 NaN 折成 +1（`*cmpg`）；否则折成 -1（`*cmpl`）
                let cc = match (op, nan_high) {
                    (0x99, _) => FloatCC::Equal,
                    (0x9a, _) => FloatCC::NotEqual,
                    (0x9b, false) => FloatCC::UnorderedOrLessThan,
                    (0x9c, false) => FloatCC::GreaterThanOrEqual,
                    (0x9d, false) => FloatCC::GreaterThan,
                    (0x9e, false) => FloatCC::UnorderedOrLessThanOrEqual,
                    (0x9b, true) => FloatCC::LessThan,
                    (0x9c, true) => FloatCC::UnorderedOrGreaterThanOrEqual,
                    (0x9d, true) => FloatCC::UnorderedOrGreaterThan,
                    _ => FloatCC::LessThanOrEqual,
                };
                fb.ins().fcmp(cc, self.a, self.b)
            }
        }
    }
}

/// 一个 local 槽的驻留方案
#[derive(Clone, Copy, PartialEq)]
enum SlotTy {
    /// 还没被任何 load/store 碰过
    Unseen,
    /// 只被一种类型访问过 → 可以驻留 SSA
    One(cranelift_codegen::ir::Type),
    /// 引用型（住根区，GC 会改写）
    Ref,
    /// 多种类型混用同一个槽 → 退回内存
    Conflict,
}

/// 局部变量的驻留方案：能进 SSA 的进 SSA，不能的还住 `locals_ptr` 指向的内存。
///
/// 为什么要这么做：`load.i64 locals_ptr+off` / `store` 每字节码一次，除了付
/// store→load 的 4~5 周期转发延迟，更要命的是**对 locals 的 store 会让它后面
/// 所有的 load 失效** —— Cranelift 的 GVN/常量折叠/LICM 因此全部白给。
/// 改成 SSA 值之后，块边界由 `FunctionBuilder` 自动插 phi（收尾的
/// `seal_all_blocks` 负责补齐前驱传参）。
struct LocalVars {
    ty: Vec<SlotTy>,
    var: Vec<Option<cranelift_frontend::Variable>>,
}

impl LocalVars {
    /// 预扫一遍字节码，按访问它的 load/store 决定每个槽的类型
    fn plan(body: &[u8], max_locals: usize) -> Self {
        let mut ty = vec![SlotTy::Unseen; max_locals];
        let mut mark = |ty: &mut Vec<SlotTy>, slot: usize, t: Option<cranelift_codegen::ir::Type>| {
            if slot >= ty.len() {
                return;
            }
            ty[slot] = match (&ty[slot], t) {
                (_, None) => SlotTy::Ref,
                (SlotTy::Unseen, Some(t)) => SlotTy::One(t),
                (SlotTy::One(prev), Some(t)) if *prev == t => SlotTy::One(t),
                _ => SlotTy::Conflict,
            };
        };
        let mut pc = 0usize;
        while pc < body.len() {
            let op = body[pc];
            match op {
                // 变长形式：操作数里带槽号
                0x15 => mark(&mut ty, body[pc + 1] as usize, Some(types::I32)), // iload
                0x16 => mark(&mut ty, body[pc + 1] as usize, Some(types::I64)), // lload
                0x17 => mark(&mut ty, body[pc + 1] as usize, Some(types::F32)), // fload
                0x18 => mark(&mut ty, body[pc + 1] as usize, Some(types::F64)), // dload
                0x19 => mark(&mut ty, body[pc + 1] as usize, None),            // aload
                0x36 => mark(&mut ty, body[pc + 1] as usize, Some(types::I32)), // istore
                0x37 => mark(&mut ty, body[pc + 1] as usize, Some(types::I64)), // lstore
                0x38 => mark(&mut ty, body[pc + 1] as usize, Some(types::F32)), // fstore
                0x39 => mark(&mut ty, body[pc + 1] as usize, Some(types::F64)), // dstore
                0x3a => mark(&mut ty, body[pc + 1] as usize, None),            // astore
                0x84 => mark(&mut ty, body[pc + 1] as usize, Some(types::I32)), // iinc
                // 短形式：槽号编在操作码里
                0x1a..=0x1d => mark(&mut ty, (op - 0x1a) as usize, Some(types::I32)),
                0x1e..=0x21 => mark(&mut ty, (op - 0x1e) as usize, Some(types::I64)),
                0x22..=0x25 => mark(&mut ty, (op - 0x22) as usize, Some(types::F32)),
                0x26..=0x29 => mark(&mut ty, (op - 0x26) as usize, Some(types::F64)),
                0x2a..=0x2d => mark(&mut ty, (op - 0x2a) as usize, None),
                0x3b..=0x3e => mark(&mut ty, (op - 0x3b) as usize, Some(types::I32)),
                0x3f..=0x42 => mark(&mut ty, (op - 0x3f) as usize, Some(types::I64)),
                0x43..=0x46 => mark(&mut ty, (op - 0x43) as usize, Some(types::F32)),
                0x47..=0x4a => mark(&mut ty, (op - 0x47) as usize, Some(types::F64)),
                0x4b..=0x4e => mark(&mut ty, (op - 0x4b) as usize, None),
                _ => {}
            }
            // 用 insn_len_at：tableswitch/lookupswitch 是变长的，按 op 猜长度会
            // 走进 switch 的数据里，把偏移字节当成操作码（最坏只是多标一个保守类型，
            // 但没理由留着这个噪声源）
            pc += insn_len_at(body, pc).max(1);
        }
        let var = (0..max_locals)
            .map(|i| match ty[i] {
                SlotTy::One(_) => Some(cranelift_frontend::Variable::from_u32(i as u32)),
                _ => None,
            })
            .collect();
        LocalVars { ty, var }
    }

    /// 在入口块里声明所有 SSA 槽并垫零值，再把**形参**从调用方写好的 locals 区搬进来。
    /// 形参在字节码里只被读不被写，所以不在这里搬一次的话它们永远是零。
    fn init(&mut self, fb: &mut FunctionBuilder, ptr: Value, desc: &str, is_static: bool) {
        for i in 0..self.var.len() {
            let (Some(var), SlotTy::One(t)) = (self.var[i], self.ty[i]) else {
                continue;
            };
            fb.declare_var(var, t);
            let z = zero_of(fb, t);
            fb.def_var(var, z);
        }
        let mut slots = param_slots(desc);
        let mut kinds: Vec<Option<cranelift_codegen::ir::Type>> = Vec::new();
        if !is_static {
            for s in slots.iter_mut() {
                *s += 1;
            }
            slots.insert(0, 0);
            kinds.push(None); // this 是引用，住根区
        }
        for d in param_descs(desc) {
            kinds.push(if is_ref_desc(&d) { None } else { Some(desc_ty(&d)) });
        }
        for (k, slot) in slots.iter().enumerate() {
            let Some(t) = kinds.get(k).copied().flatten() else { continue };
            let Some(var) = self.var.get(*slot).copied().flatten() else { continue };
            if self.ty[*slot] != SlotTy::One(t) {
                continue;
            }
            let addr = fb.ins().iadd_imm(ptr, (*slot * 8) as i64);
            let v = fb.ins().load(t, MemFlags::new(), addr, 0);
            fb.def_var(var, v);
        }
    }

    /// 读一个 local：驻留了就直接 `use_var`，否则退回内存
    fn load(&mut self, fb: &mut FunctionBuilder, ptr: Value, slot: usize, want: cranelift_codegen::ir::Type) -> Value {
        if let Some(var) = self.var.get(slot).copied().flatten() {
            if self.ty[slot] == SlotTy::One(want) {
                return fb.use_var(var);
            }
        }
        let addr = fb.ins().iadd_imm(ptr, (slot * 8) as i64);
        fb.ins().load(want, MemFlags::new(), addr, 0)
    }

    /// 写一个 local：驻留了就 `def_var`，否则退回内存
    fn store(
        &mut self,
        fb: &mut FunctionBuilder,
        ptr: Value,
        slot: usize,
        want: cranelift_codegen::ir::Type,
        v: Value,
    ) {
        let v = convert_to(fb, v, want);
        if let Some(var) = self.var.get(slot).copied().flatten() {
            if self.ty[slot] == SlotTy::One(want) {
                fb.def_var(var, v);
                return;
            }
        }
        let addr = fb.ins().iadd_imm(ptr, (slot * 8) as i64);
        fb.ins().store(MemFlags::new(), v, addr, 0);
    }
}

/// 形参描述符 → 驻留类型（引用型不会走到这里）
fn desc_ty(d: &str) -> cranelift_codegen::ir::Type {
    match d.as_bytes().first() {
        Some(b'J') => types::I64,
        Some(b'D') => types::F64,
        Some(b'F') => types::F32,
        _ => types::I32,
    }
}


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
    /// 把本帧根区**登记 / 注销**到堆的根区表（指针式：只交地址，不拷贝值）。
    ///
    /// 只在帧入口（`push`）与帧的每条退出路径（`!push`）各发一次。调用点不再发 ——
    /// 登记一次之后就覆盖整帧，GC 随时能按地址就地读到本帧的引用。
    pub(super) fn emit_roots(
        &mut self,
        fb: &mut FunctionBuilder,
        root: Value,
        n: i32,
        push: bool,
    ) -> Result<(), JitError> {
        // 方法里没有任何引用（root_slots == 0）：整块根区都不存在，连调用都不发
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

/// 方法体里第一条 `monitorenter`(0xc2) / `monitorexit`(0xc3)。
///
/// 按指令长度走，不会把操作数里的 0xc2 当成指令。
fn find_monitor(body: &[u8]) -> Option<(usize, u8)> {
    let mut pc = 0usize;
    while pc < body.len() {
        if body[pc] == 0xc2 || body[pc] == 0xc3 {
            return Some((pc, body[pc]));
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

/// 操作数栈弹出（带诊断）：下溢时把**是哪一条指令**报出来。
///
/// 以前这里只有「栈下溢」三个字，定位一个下溢要在 1500 行的 match 里逐个猜 ——
/// 而栈形状算错时真正出问题的指令往往离根因好几条。
fn pop_ck(
    stack: &mut Vec<Value>,
    op: u8,
    pc: usize,
) -> Result<Value, JitError> {
    stack
        .pop()
        .ok_or_else(|| JitError(format!("栈下溢 op=0x{op:02x}@{}", pc - 1)))
}

/// 只看栈顶不弹出（带诊断，同上）
fn peek_ck(stack: &[Value], op: u8, pc: usize) -> Result<Value, JitError> {
    stack
        .last()
        .copied()
        .ok_or_else(|| JitError(format!("栈下溢 op=0x{op:02x}@{}", pc - 1)))
}

/// 引用型实参的**文本化**是否可信（`append(Object)` / `println(Object)` / 拼接的 `\u0001` 槽）。
///
/// JIT 把对象变成文本只有一条路：`natives::obj_to_string` —— 它认 `String`、认包装类，
/// 其余一律打成「类名@标识哈希」（= `Object.toString()` 的语义）。所以只要静态类型
/// （或它的祖先）**覆盖了 `toString()`**，这条路给的就是错文本：JVM 会调用户的
/// `toString()`，而桩函数在 Rust 侧只握着一个可能被 GC 搬走的偏移，**不能**回调 JIT 代码。
///
/// 覆盖了就返回 `false` 让**整个方法**回退解释器 —— 慢，但输出对。宁可回退也不许
/// 「编得出来、打出来是错的」。
fn ref_text_ok(classes: &ClassSet, desc: &str) -> bool {
    let Some(inner) = desc.strip_prefix('L').and_then(|d| d.strip_suffix(';')) else {
        return false; // 数组（`[C` / `[Ljava/lang/Object;`）：没有渲染路径
    };
    if inner == "java/lang/String" || crate::jdk::wrapper_prim(inner).is_some() {
        return true;
    }
    let mut cur = inner.to_string();
    for _ in 0..64 {
        let Some(cf) = classes.get(&cur) else {
            return false; // 不认识的类（JDK 侧）：无从判断它有没有覆盖
        };
        if cf.access_flags & 0x0200 != 0 {
            return false; // 接口：实现类可能覆盖，静态类型看不出来
        }
        let overrides = cf.methods.iter().any(|m| {
            cf.utf8(m.name_index).map(|b| b == b"toString").unwrap_or(false)
                && cf.utf8(m.descriptor_index)
                    .map(|b| b == b"()Ljava/lang/String;")
                    .unwrap_or(false)
        });
        if overrides {
            return false;
        }
        match classes.super_of(&cur) {
            Some(s) if s != "java/lang/Object" => cur = s,
            _ => return true, // 走到 Object 都没人覆盖：`类名@哈希` 就是 JVM 该给的
        }
    }
    false
}

/// 方法体里第一条打在「JIT 没有的东西」上的调用（线程 / 并发 / 监视器协议）。
///
/// JIT 的堆是 `thread_local`（每线程一个堆、一张静态字段表），所以跨线程共享对象
/// **在架构上**就不成立；`Object.wait/notify` 是监视器协议，而 JIT 连 `monitorenter`
/// 都不接。这类调用以前很少暴露，是因为含 lambda 的 `main` 整方法就编不了；
/// `invokedynamic` 接上之后 `new Thread(() -> …)` 会被编进来，必须在编译期挡住。
fn find_thread_api(cf: &ClassFile, body: &[u8]) -> Option<String> {
    let mut pc = 0usize;
    while pc < body.len() {
        let op = body[pc];
        if matches!(op, 0xb6..=0xb9) && pc + 2 < body.len() {
            let idx = u16::from_be_bytes([body[pc + 1], body[pc + 2]]);
            if let Ok((c, n, d)) = method_ref_of(cf, idx) {
                let bad = c == "java/lang/Thread"
                    || c.starts_with("java/util/concurrent/")
                    || (c == "java/lang/Object"
                        && matches!(n.as_str(), "wait" | "notify" | "notifyAll"));
                if bad {
                    return Some(format!("{c}.{n}{d}"));
                }
            }
        }
        pc += insn_len_at(body, pc).max(1);
    }
    None
}
