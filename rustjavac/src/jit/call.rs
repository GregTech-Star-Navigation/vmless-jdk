//! 方法解析、参数槽位、返回类型与 `CompiledMethod`。

use cranelift_codegen::ir::{types, InstBuilder, Value};
use cranelift_frontend::FunctionBuilder;

use crate::{ClassFile, CpEntry, Member};

use super::rt;
use super::JitError;
use super::JitFn;

pub struct CompiledMethod {
    pub name: String,
    pub desc: String,
    pub code: JitFn,
    pub max_locals: usize,
}

/// 返回 (max_locals, max_stack, 字节码, 异常表)。异常表每项 (start, end, handler, catch_type)，
/// catch_type = 0 表示 catch-all（finally）。
    pub(super) fn code_of(
        cf: &ClassFile,
        m: &Member,
    ) -> Result<(usize, usize, Vec<u8>, Vec<(u16, u16, u16, u16)>), JitError> {
    let code_name = cf
        .utf8_index(b"Code")
        .ok_or_else(|| JitError("常量池里没有 Code".into()))?;
    for a in &m.attributes {
        if a.name_index == code_name && a.info.len() >= 8 {
            let max_locals = u16::from_be_bytes([a.info[2], a.info[3]]) as usize;
            let max_stack = u16::from_be_bytes([a.info[0], a.info[1]]) as usize;
            let len = u32::from_be_bytes([a.info[4], a.info[5], a.info[6], a.info[7]]) as usize;
            let end = (8 + len).min(a.info.len());
            // 异常表紧跟在字节码后面：u2 个数 + 每项 4 个 u2
            let mut table = Vec::new();
            let mut q = end;
            if q + 2 <= a.info.len() {
                let n = u16::from_be_bytes([a.info[q], a.info[q + 1]]) as usize;
                q += 2;
                for _ in 0..n {
                    if q + 8 > a.info.len() {
                        break;
                    }
                    let rd = |i: usize| u16::from_be_bytes([a.info[i], a.info[i + 1]]);
                    table.push((rd(q), rd(q + 2), rd(q + 4), rd(q + 6)));
                    q += 8;
                }
            }
            return Ok((max_locals, max_stack, a.info[8..end].to_vec(), table));
        }
    }
    Err(JitError("方法没有 Code".into()))
}

/// 解析常量池里的方法引用 → (类名, 方法名, 描述符)
    pub(super) fn method_ref_of(cf: &ClassFile, index: u16) -> Result<(String, String, String), JitError> {
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

/// 方法返回值的机器类型：long → I64，double → F64，float → F32，其余（含 void）→ I32
    pub(super) fn ret_type(desc: &str) -> cranelift_codegen::ir::Type {
    match crate::ret_of(desc) {
        Some(b'J') => types::I64,
        Some(b'D') => types::F64,
        // float 用 F32 而不是 F64：f32 的舍入语义要由指令本身保证，
        // 拿 F64 代替会让 `1.0f/3.0f` 打出 double 的十进制形态
        Some(b'F') => types::F32,
        // 引用是 8 字节指针，必须按 I64 返回，否则会被截断成 i32
        Some(b'L') | Some(b'[') => types::I64,
        _ => types::I32,
    }
}

/// 把栈上的值转成 `ty` 要求的类型（只在确实不同型时才插转换指令）
    pub(super) fn convert_to(fb: &mut FunctionBuilder, v: Value, ty: cranelift_codegen::ir::Type) -> Value {
    let from = fb.func.dfg.value_type(v);
    if from == ty {
        return v;
    }
    if from == types::I32 && ty == types::I64 {
        fb.ins().sextend(types::I64, v)
    } else if from == types::I64 && ty == types::I32 {
        fb.ins().ireduce(types::I32, v)
    } else if ty == types::F64 && from == types::F32 {
        fb.ins().fpromote(types::F64, v)
    } else if ty == types::F32 && from == types::F64 {
        fb.ins().fdemote(types::F32, v)
    } else if ty == types::F64 {
        fb.ins().fcvt_from_sint(types::F64, v)
    } else if ty == types::F32 {
        fb.ins().fcvt_from_sint(types::F32, v)
    } else if from == types::F64 && ty == types::I64 {
        fb.ins().fcvt_to_sint_sat(types::I64, v)
    } else if from == types::F64 {
        fb.ins().fcvt_to_sint_sat(types::I32, v)
    } else if from == types::F32 && ty == types::I64 {
        fb.ins().fcvt_to_sint_sat(types::I64, v)
    } else if from == types::F32 {
        fb.ins().fcvt_to_sint_sat(types::I32, v)
    } else {
        v
    }
}

/// 该类型的零值（补默认返回 / 不可达块收尾用）
    pub(super) fn zero_of(fb: &mut FunctionBuilder, ty: cranelift_codegen::ir::Type) -> Value {
    if ty == types::F64 {
        fb.ins().f64const(0.0)
    } else if ty == types::F32 {
        fb.ins().f32const(0.0)
    } else if ty == types::I64 {
        fb.ins().iconst(types::I64, 0)
    } else {
        fb.ins().iconst(types::I32, 0)
    }
}

/// 描述符里每个参数所在的 JVM 槽位（long/double 占两槽；静态方法从 0 起）
/// 方法描述符里的参数描述符（按顺序，不含 this）
pub(crate) fn param_descs(desc: &str) -> Vec<String> {
    let b = desc.as_bytes();
    let mut i = 1usize;
    let mut out = Vec::new();
    while i < b.len() && b[i] != b')' {
        let start = i;
        match b[i] {
            b'J' | b'D' | b'F' | b'I' | b'B' | b'C' | b'S' | b'Z' => i += 1,
            b'L' => {
                while i < b.len() && b[i] != b';' {
                    i += 1;
                }
                i += 1;
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
            }
            _ => i += 1,
        }
        out.push(String::from_utf8_lossy(&b[start..i.min(b.len())]).into_owned());
    }
    out
}

pub(crate) fn param_slots(desc: &str) -> Vec<usize> {
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
    pub(super) fn argc_of(desc: &str) -> i32 {
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

impl CompiledMethod {
    /// 调用：args 依次写入参数字槽，返回值即方法返回值
    ///
    /// 对外签名固定成 i32（老调用方都按 int 用），但被调方法可能真的返回
    /// i64/f64 —— 那种情况下要按它自己的返回类型去调，否则读错寄存器。
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
        let p = locals.as_mut_ptr();
        let ty = ret_type(&self.desc);
        // 顶层：异常没被任何 handler 接住 → 按 JVM 的格式打一行并退出（码 1）
        let r = unsafe {
            if ty == types::F64 {
                let f: unsafe extern "C" fn(*mut i64) -> f64 = std::mem::transmute(self.code);
                f(p) as i32
            } else if ty == types::F32 {
                let f: unsafe extern "C" fn(*mut i64) -> f32 = std::mem::transmute(self.code);
                f(p) as i32
            } else if ty == types::I64 {
                let f: unsafe extern "C" fn(*mut i64) -> i64 = std::mem::transmute(self.code);
                f(p) as i32
            } else {
                (self.code)(p)
            }
        };
        if rt::exc_pending() != 0 {
            rt::print_uncaught(rt::exc_get());
        }
        r
    }
}
