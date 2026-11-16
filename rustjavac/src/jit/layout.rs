//! 类标签、字段偏移与数组元素步长。

use cranelift_codegen::ir::{types, InstBuilder, MemFlags, Value};
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
/// 字段槽统一 8 字节，float 按 **F32** 存低 4 字节（与栈上的机器类型一致）。
pub(super) fn field_slot_type(desc: &str) -> (cranelift_codegen::ir::Type, cranelift_codegen::ir::Type) {
    match desc.as_bytes().first().copied() {
        Some(b'J') => (types::I64, types::I64),
        Some(b'D') => (types::F64, types::F64),
        Some(b'F') => (types::F32, types::F32),
        Some(b'L') | Some(b'[') => (types::I64, types::I64),
        _ => (types::I32, types::I32),
    }
}

/// 引用字段 / 引用元素
pub(super) fn is_ref_desc(desc: &str) -> bool {
    matches!(desc.as_bytes().first().copied(), Some(b'L') | Some(b'['))
}

/// 字段 `i` 在对象里的字节偏移。
///
/// 每个 Java 字段占 8 字节（两个 librustjrt 槽：值 + 填充），所以偏移与旧布局一致；
/// 但引用只写低 4 字节 —— 那正好是 GC 位图里的那个槽。
#[inline]
pub(super) fn field_off(i: usize) -> i32 {
    (8 + 8 * i) as i32
}

/// 字段 `i` 在 librustjrt 布局表里的槽下标（引用写屏障用）
#[inline]
pub(super) fn field_slot_idx(i: usize) -> u32 {
    (2 * i) as u32
}

/// 生成「读一次堆基址」的代码。
///
/// 偏移是相对堆基址的，而堆扩容会 `realloc` 搬整块 —— 基址必须每次现读，不能缓存。
pub(super) fn emit_base(fb: &mut FunctionBuilder) -> Value {
    let addr = fb.ins().iconst(types::I64, rt::base_addr());
    fb.ins().load(types::I64, MemFlags::new(), addr, 0)
}

/// 对象引用（偏移）→ 可访存的地址（基址 + 偏移）
pub(super) fn emit_ref_addr(fb: &mut FunctionBuilder, h: Value) -> Value {
    let base = emit_base(fb);
    let h = convert_to(fb, h, types::I64);
    fb.ins().iadd(base, h)
}

/// 数组元素地址 = 数组地址 + 16 + 下标 * 元素字节数
/// （librustjrt 的数组头 16 字节：class_id / flags / 长度 / 填充）
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
    let hdr = fb.ins().iconst(types::I64, 16);
    Ok(fb.ins().iadd(addr, hdr))
}

/// 分配数组：按数组描述符的 class_id 调运行时（元素宽度在布局表里）
pub(super) fn push_new_array(
    module: &mut JITModule,
    fb: &mut FunctionBuilder,
    new_arr_id: cranelift_module::FuncId,
    desc: &str,
    len: Value,
) -> Value {
    let class_id = rt::array_class_of(desc);
    let cid = fb.ins().iconst(types::I32, class_id as i64);
    let fref = module.declare_func_in_func(new_arr_id, fb.func);
    let c = fb.ins().call(fref, &[cid, len]);
    fb.inst_results(c)[0]
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
