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
    /// `LambdaMetafactory`：编译期就展开成「合成类实例 + 蹦床」（见 `jit::lambda`）
    Lambda(LambdaSite),
}

/// 一个 lambda 调用点在编译期解析出来的全部材料（运行期不再解析）
pub struct LambdaSite {
    /// 站点编号（`类名 + 常量池下标` 唯一，**跨编译单元共享**）
    pub id: u32,
    /// 合成类名（`<实现类>$$lambda$<id>`）
    pub class_name: String,
    /// SAM 方法名（= invokedynamic 自己的名字）
    pub sam_name: String,
    /// SAM 方法描述符（引导方法第 1 个参数 `samMethodType`，已擦除）
    pub sam_desc: String,
    /// `instantiatedMethodType`（引导方法第 3 个参数：实际参数类型）
    pub inst_desc: String,
    /// 实现目标的 MethodHandle：种类 + `(类, 方法名, 描述符)`
    pub impl_kind: u8,
    pub impl_class: String,
    pub impl_name: String,
    pub impl_desc: String,
    /// 站点自己的描述符：它的参数就是**捕获变量**（顺序 = 合成类字段顺序）
    pub captured: Vec<String>,
}

/// 分类一个调用点。`Err` 里是**方法级回退原因**（JIT 不接这个引导方法）。
pub fn classify(cf: &ClassFile, cp_index: u16) -> Result<Site, String> {
    classify_in(cf, "", cp_index)
}

/// 同上，但带上**声明类名**（站点编号要按「类 + 常量池下标」唯一）
pub fn classify_in(cf: &ClassFile, owner: &str, cp_index: u16) -> Result<Site, String> {
    let (name, desc, bsm_index) = site_of(cf, cp_index)?;
    let table = crate::classfile::attr::bootstrap_methods(cf)
        .map_err(|e| format!("BootstrapMethods 属性解析失败: {e}"))?;
    let e = table
        .get(bsm_index as usize)
        .ok_or_else(|| format!("BootstrapMethods[{bsm_index}] 越界（共 {} 项）", table.len()))?;
    let (bcls, bname) = bsm_target(cf, e.method_ref)?;
    if bcls == "java/lang/invoke/LambdaMetafactory" {
        return lambda_site(cf, owner, cp_index, &name, &desc, &bname, e);
    }
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

/// `LambdaMetafactory.{metafactory,altMetafactory}` 的静态材料。
///
/// 引导参数固定前三个：`(samMethodType, implMethod, instantiatedMethodType)`，
/// `altMetafactory` 后面再跟 `flags` / 标记接口 / 桥方法 —— 那些 v1 不看
/// （桥方法只是同一份实现在别的签名下再登记一次，没有也不影响正确性）。
fn lambda_site(
    cf: &ClassFile,
    owner: &str,
    cp_index: u16,
    sam_name: &str,
    site_desc: &str,
    bname: &str,
    e: &crate::classfile::attr::BootstrapMethod,
) -> Result<Site, String> {
    if bname != "metafactory" && bname != "altMetafactory" {
        return Err(format!("LambdaMetafactory.{bname} 不在 JIT 白名单里"));
    }
    let sam_desc = method_type(cf, e.args.first().copied())?;
    let (impl_kind, impl_class, impl_name, impl_desc) = impl_of(cf, e.args.get(1).copied())?;
    let inst_desc = method_type(cf, e.args.get(2).copied())?;
    let id = site_id(owner, cp_index);
    Ok(Site::Lambda(LambdaSite {
        id,
        class_name: format!("{}$$lambda${id}", impl_class.replace('/', "_")),
        sam_name: sam_name.to_string(),
        sam_desc,
        inst_desc,
        impl_kind,
        impl_class,
        impl_name,
        impl_desc,
        captured: param_descs(site_desc),
    }))
}

/// 「同一个类的同一个调用点只解析一次」——编号表是**进程级**的，
/// 所以同一份代码被编译多次（多个 `Jit`、重编同一个方法）不会重复织类。
static SITES: std::sync::Mutex<Vec<(String, u16)>> = std::sync::Mutex::new(Vec::new());

fn site_id(owner: &str, cp_index: u16) -> u32 {
    let mut g = SITES.lock().unwrap();
    if let Some(i) = g.iter().position(|(c, p)| c == owner && *p == cp_index) {
        return i as u32;
    }
    g.push((owner.to_string(), cp_index));
    (g.len() - 1) as u32
}

/// 引导参数里的 `CONSTANT_MethodType` → 描述符字符串
fn method_type(cf: &ClassFile, index: Option<u16>) -> Result<String, String> {
    let index = index.ok_or_else(|| "lambda 引导方法参数不足（至少要 samMethodType/implMethod/instantiatedMethodType）".to_string())?;
    match cf.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
        Some(CpEntry::MethodType(d)) => Ok(
            String::from_utf8_lossy(cf.utf8(*d).unwrap_or_default()).into_owned(),
        ),
        other => Err(format!("引导参数 #{index} 不是 MethodType: {other:?}")),
    }
}

/// 引导参数里的实现目标 `CONSTANT_MethodHandle` → `(种类, 类, 名, 描述符)`
fn impl_of(cf: &ClassFile, index: Option<u16>) -> Result<(u8, String, String, String), String> {
    let index = index.ok_or_else(|| "lambda 引导方法没有 implMethod".to_string())?;
    let (kind, refi) = match cf.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
        Some(CpEntry::MethodHandle(k, i)) => (*k, *i),
        other => return Err(format!("引导参数 #{index} 不是 MethodHandle: {other:?}")),
    };
    let (c, n, d) = crate::jit::call::method_ref_of(cf, refi).map_err(|e| e.0)?;
    Ok((kind, c, n, d))
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
