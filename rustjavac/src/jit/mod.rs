//! 方法级 JIT：JVM 字节码 → Cranelift IR → 本地机器码。
//!
//! 覆盖范围（够跑我们自己编译器产出的 int 方法）：
//! 常量、iload/istore、(iinc)、算术、比较跳转、goto、ireturn/return。
//! 操作数栈用 Cranelift 的 SSA 值表示；局部变量放在一块内存里，
//! 由调用方传入指针（与解释器共用同一份 locals 布局）。
//!
//! 本文件只剩入口与 `Jit` 的定义 —— 具体实现按职责分散在同目录各模块里。

use cranelift_codegen::settings::{self, Configurable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::default_libcall_names;

use crate::{parse, ClassFile};

mod call;
mod exc;
mod insn;
mod layout;
pub mod rt;
pub mod strtab;
mod translate;

pub use call::CompiledMethod;
pub(crate) use call::param_slots;

pub struct Jit {
    module: JITModule,
    /// (方法名, 描述符) → 已声明/已编译的函数 id
    compiled: std::collections::HashMap<(String, String), (cranelift_module::FuncId, usize, i32)>,
}

#[derive(Debug)]
pub struct JitError(pub String);

type JitFn = unsafe extern "C" fn(*mut i64) -> i32;

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
        builder.symbol("vmless_println_bool", rt::println_bool as *const u8);
        builder.symbol("vmless_println_long", rt::println_long as *const u8);
        builder.symbol("vmless_println_double", rt::println_double as *const u8);
        builder.symbol("vmless_instanceof", rt::instanceof as *const u8);
        builder.symbol("vmless_checkcast", rt::checkcast as *const u8);
        builder.symbol("vmless_println_str", rt::println_str as *const u8);
        builder.symbol("vmless_str_const", strtab::str_const as *const u8);
        builder.symbol("vmless_println_obj", strtab::println_str as *const u8);
        builder.symbol("vmless_drem", rt::drem as *const u8);
        builder.symbol("vmless_exc_throw", rt::exc_throw as *const u8);
        builder.symbol("vmless_exc_pending", rt::exc_pending as *const u8);
        builder.symbol("vmless_exc_get", rt::exc_get as *const u8);
        builder.symbol("vmless_exc_clear", rt::exc_clear as *const u8);
        builder.symbol("vmless_exc_set_msg", rt::exc_set_msg as *const u8);
        builder.symbol("vmless_catch_match", rt::catch_match as *const u8);
        let module = JITModule::new(builder);
        Jit { module, compiled: std::collections::HashMap::new() }
    }

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

/// 从文件编译并调用（测试/基准用）
pub fn compile_file(path: &str, name: &str, desc: &str) -> Result<(Jit, CompiledMethod), JitError> {
    let bytes = std::fs::read(path).map_err(|e| JitError(e.to_string()))?;
    let cf = parse(&bytes).map_err(|e| JitError(e.to_string()))?;
    let mut jit = Jit::new();
    let m = jit.compile_static(&cf, name, desc)?;
    Ok((jit, m))
}
