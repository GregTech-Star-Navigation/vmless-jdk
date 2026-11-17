//! `invokedynamic`（0xba）在 JIT 侧的**编译期**处理。
//!
//! JIT 不跑引导方法、也不构造 `CallSite`：调用点在编译期就被分类，能内联的直接展开成
//! 运行时桩的实参，不能接的返回一个**方法级回退原因**（调用方把那个方法整个交给解释器，
//! 同一个程序里其它方法照旧走 JIT）。
//!
//! 目前接的是 `StringConcatFactory`（`"a" + b` 编译出来的那个）：recipe 里的
//! `\u0001` 是栈上的动态参数、`\u0002` 是引导方法的静态常量，两者在编译期就定死了，
//! 于是整段拼接变成一条「拼接计划」+ 一次 `rt::indy_concat` 调用。

use crate::jit::call::param_descs;
use crate::jit::rt::{self, CPiece};
use crate::jit::strtab;
use crate::{ClassFile, CpEntry};

/// 一个 `invokedynamic` 调用点的编译期分类
pub enum Site {
    /// 字符串拼接：已登记的拼接计划编号 + 该站点参数描述符（按顺序）
    Concat { plan: u32, params: Vec<String> },
}

/// 分类一个调用点。`Err` 里是**方法级回退原因**（JIT 不接这个引导方法）。
pub fn classify(cf: &ClassFile, cp_index: u16) -> Result<Site, String> {
    let (_name, desc, bsm_index) = site_of(cf, cp_index)?;
    let table = crate::classfile::attr::bootstrap_methods(cf)
        .map_err(|e| format!("BootstrapMethods 属性解析失败: {e}"))?;
    let e = table
        .get(bsm_index as usize)
        .ok_or_else(|| format!("BootstrapMethods[{bsm_index}] 越界（共 {} 项）", table.len()))?;
    let (bcls, bname) = bsm_target(cf, e.method_ref)?;
    let params = param_descs(&desc);
    if bcls != "java/lang/invoke/StringConcatFactory" {
        return Err(format!("引导方法 {bcls}.{bname} 不在 JIT 白名单里"));
    }
    let pieces = match bname.as_str() {
        // 没有 recipe：参数按顺序全部追加
        "makeConcat" => params
            .iter()
            .map(|p| CPiece::Arg(kind_of(p)))
            .collect::<Vec<_>>(),
        "makeConcatWithConstants" => {
            let Some(first) = e.args.first() else {
                return Err("makeConcatWithConstants 没有 recipe".into());
            };
            let recipe = const_text(cf, *first)?;
            let consts = e
                .args
                .iter()
                .skip(1)
                .map(|i| const_text(cf, *i))
                .collect::<Result<Vec<_>, _>>()?;
            decode_recipe(&recipe, &consts, &params)?
        }
        other => return Err(format!("StringConcatFactory.{other} 不在 JIT 白名单里")),
    };
    Ok(Site::Concat {
        plan: rt::register_concat(pieces),
        params,
    })
}

/// recipe → 拼接计划。`\u0001` 取下一个动态参数，`\u0002` 取下一个静态常量，其余是字面量。
fn decode_recipe(recipe: &str, consts: &[String], params: &[String]) -> Result<Vec<CPiece>, String> {
    let mut out = Vec::new();
    let mut lit = String::new();
    let mut argi = 0usize;
    let mut coni = 0usize;
    let flush = |out: &mut Vec<CPiece>, lit: &mut String| {
        if !lit.is_empty() {
            out.push(CPiece::Lit(strtab::register(lit)));
            lit.clear();
        }
    };
    for ch in recipe.chars() {
        match ch {
            '\u{1}' => {
                flush(&mut out, &mut lit);
                let p = params
                    .get(argi)
                    .ok_or_else(|| format!("recipe 要第 {} 个参数，站点只有 {} 个", argi + 1, params.len()))?;
                out.push(CPiece::Arg(kind_of(p)));
                argi += 1;
            }
            '\u{2}' => {
                flush(&mut out, &mut lit);
                let c = consts
                    .get(coni)
                    .ok_or_else(|| format!("recipe 要第 {} 个静态常量，只有 {} 个", coni + 1, consts.len()))?;
                out.push(CPiece::Lit(strtab::register(c)));
                coni += 1;
            }
            c => lit.push(c),
        }
    }
    flush(&mut out, &mut lit);
    if argi != params.len() {
        return Err(format!("recipe 用了 {argi} 个参数，站点有 {} 个", params.len()));
    }
    Ok(out)
}

/// 参数描述符 → 运行时解码用的 kind
fn kind_of(desc: &str) -> u8 {
    match desc.as_bytes().first().copied() {
        Some(b'Z') => rt::K_BOOL,
        Some(b'C') => rt::K_CHAR,
        Some(b'I') | Some(b'B') | Some(b'S') => rt::K_INT,
        Some(b'J') => rt::K_LONG,
        Some(b'F') => rt::K_FLOAT,
        Some(b'D') => rt::K_DOUBLE,
        _ => rt::K_REF,
    }
}

/// 调用点的 `(名字, 描述符, 引导方法索引)`
pub(super) fn site_of(cf: &ClassFile, cp_index: u16) -> Result<(String, String, u16), String> {
    match cf.constant_pool.get(cp_index as usize).and_then(|e| e.as_ref()) {
        Some(CpEntry::InvokeDynamic(bsm, nat)) => {
            let (n, d) = nat_of(cf, *nat)?;
            Ok((n, d, *bsm))
        }
        other => Err(format!("常量池 #{cp_index} 不是 invokedynamic: {other:?}")),
    }
}

fn nat_of(cf: &ClassFile, nat: u16) -> Result<(String, String), String> {
    match cf.constant_pool.get(nat as usize).and_then(|e| e.as_ref()) {
        Some(CpEntry::NameAndType(n, d)) => Ok((
            String::from_utf8_lossy(cf.utf8(*n).unwrap_or_default()).into_owned(),
            String::from_utf8_lossy(cf.utf8(*d).unwrap_or_default()).into_owned(),
        )),
        other => Err(format!("NameAndType 解析失败: {other:?}")),
    }
}

/// 引导方法的 `(类, 方法名)`（引导方法一定是 MethodHandle）
fn bsm_target(cf: &ClassFile, method_ref: u16) -> Result<(String, String), String> {
    let refi = match cf.constant_pool.get(method_ref as usize).and_then(|e| e.as_ref()) {
        Some(CpEntry::MethodHandle(_, i)) => *i,
        other => return Err(format!("引导方法不是 MethodHandle: {other:?}")),
    };
    let (c, n, _) = crate::jit::call::method_ref_of(cf, refi).map_err(|e| e.0)?;
    Ok((c, n))
}

/// 引导方法的静态常量 → 文本（`\u0002` 占位符的值）
fn const_text(cf: &ClassFile, index: u16) -> Result<String, String> {
    match cf.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
        Some(CpEntry::String(u)) => Ok(String::from_utf8_lossy(cf.utf8(*u).unwrap_or_default()).into_owned()),
        Some(CpEntry::Integer(v)) => Ok(v.to_string()),
        Some(CpEntry::Long(v)) => Ok(v.to_string()),
        Some(CpEntry::Float(bits)) => Ok(crate::java_float_to_string(f32::from_bits(*bits))),
        Some(CpEntry::Double(bits)) => Ok(crate::java_double_to_string(f64::from_bits(*bits))),
        other => Err(format!("recipe 的静态常量类型不支持: {other:?}")),
    }
}
