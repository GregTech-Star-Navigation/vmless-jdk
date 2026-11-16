//! 类标签、字段偏移与数组元素步长。

use cranelift_codegen::ir::{types, InstBuilder, Value};
use cranelift_frontend::FunctionBuilder;
use cranelift_jit::JITModule;
use cranelift_module::Module;

use crate::{ClassFile, CpEntry};

use super::call::convert_to;
use super::rt;
use super::JitError;

/// 解析常量池里的**字段**引用 → (类名, 字段名, 描述符)
    pub(super) fn field_ref_of(cf: &ClassFile, index: u16) -> Result<(String, String, String), JitError> {
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

/// 字段描述符 → (访存用的机器类型, 栈上应保持的类型)
///
/// 字段槽统一 8 字节，所以 float 也按 F64 存取（本编译器不产 float 字段）。
    pub(super) fn field_slot_type(desc: &str) -> (cranelift_codegen::ir::Type, cranelift_codegen::ir::Type) {
    match desc.as_bytes().first().copied() {
        Some(b'J') => (types::I64, types::I64),
        Some(b'D') => (types::F64, types::F64),
        Some(b'F') => (types::F64, types::F64),
        Some(b'L') | Some(b'[') => (types::I64, types::I64),
        _ => (types::I32, types::I32),
    }
}

/// 数组元素地址 = 数组指针 + 8 + 下标 * 元素字节数
    pub(super) fn elem_addr(
    fb: &mut FunctionBuilder,
    p: Value,
    idx: Value,
    esize: i64,
) -> Result<Value, JitError> {
    let idx = convert_to(fb, idx, types::I32);
    let off = fb.ins().uextend(types::I64, idx);
    let stride = fb.ins().iconst(types::I64, esize);
    let byte_off = fb.ins().imul(off, stride);
    let addr = fb.ins().iadd(p, byte_off);
    let eight = fb.ins().iconst(types::I64, 8);
    Ok(fb.ins().iadd(addr, eight))
}

/// 分配数组：栈顶是长度，按元素字节数与数组描述符的类型标签调运行时
    pub(super) fn push_new_array(
    fb: &mut FunctionBuilder,
    stack: &mut Vec<Value>,
    module: &mut JITModule,
    new_arr_id: cranelift_module::FuncId,
    esize: i64,
    desc: &str,
) -> Result<(), JitError> {
    let n = stack.pop().ok_or_else(|| JitError("栈下溢".into()))?;
    let n = convert_to(fb, n, types::I32);
    let es = fb.ins().iconst(types::I32, esize);
    let tag = fb.ins().iconst(types::I32, rt::class_tag(desc) as i64);
    let fref = module.declare_func_in_func(new_arr_id, fb.func);
    let c = fb.ins().call(fref, &[n, es, tag]);
    stack.push(fb.inst_results(c)[0]);
    Ok(())
}

/// 常量池里的 Class 项 → 类型标签
    pub(super) fn class_const_tag(cf: &ClassFile, index: u16) -> Result<u32, JitError> {
    match cf.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
        Some(CpEntry::Class(n)) => {
            let name = String::from_utf8_lossy(cf.utf8(*n).unwrap_or_default()).into_owned();
            Ok(rt::class_tag(&name))
        }
        other => Err(JitError(format!("索引 {index} 不是 Class 项: {other:?}"))),
    }
}
