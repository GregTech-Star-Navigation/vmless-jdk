//! 小方法内联 —— 在**字节码层**把被调方法体拼进调用方。
//!
//! **为什么在字节码层做**：主翻译器（`translate.rs`）是一条 1500 行的单趟
//! match，想复用它就得把它的全部状态（`fb` / `stack` / `lvars` / 几十个运行时
//! 函数 id / 异常派发 / 根区）抽成上下文再重入 —— 那是大面积重构，而
//! 「内联错了会出现结果对、语义错的隐蔽 bug」正是最不该大面积重构的时候。
//! 字节码拼接把内联变成一次**纯改写**：改完的 body 交给原来的翻译器，
//! 后面的块划分、栈形状分析（`refmap`）、根区一概不动。
//!
//! **三条硬约束**（缺一条就会静默出错）：
//!
//! 1. **拼接进来的代码里不能留下任何常量池引用**。`ldc` / `getstatic` /
//!    `invoke*` 的索引是**被调方**常量池的下标，拼进调用方之后会被按**调用方**
//!    的池解释 —— 那是「跑得通、结果错」。所以：`ldc*` 一律不内联；
//!    `invokestatic`/`invokevirtual` 在还有内联深度时递归内联掉（它的池引用
//!    随原指令一起消失），深度用尽就**整块放弃**，绝不留一条待解释的池引用。
//! 2. **不许有异常表**。带 `catch` 的方法直接不内联：handler 位置与
//!    `max_locals` 绑定，合并两张表是另一个量级的改动。
//! 3. **签名与字节码都不碰引用**。引用型局部变量住根区（GC 会就地改写它们），
//!    拼接过来的槽位没有对应的根区空间。只内联纯基本类型的方法就绕开了整件事，
//!    也顺带保证内联体里没有分配点（不需要根区登记）。
//!
//! **拼接方式：就地替换调用指令**，然后按「旧 pc → 新位置」的地址表把调用方的
//! 所有跳转、`switch` 填充、异常表一起重定位。
//!
//! 一开始试过的是「调用点换一条等长 `goto`，被调方搬到方法尾部」—— 好处是调用方
//! 已有的偏移一个都不用动。**那是错的**：`translate` 与 `refmap` 都建立在
//! 「跳转点上操作数栈必空」这条不变式上（`translate.rs` 到块首时直接
//! `if !stack.is_empty() { return Err(...) }`），而「跳到尾部」与「带着返回值
//! 跳回来」两条跳转的栈都不空 —— 实测 `Perf2.main` 因此直接编译失败。
//! 就地替换不引入任何新跳转，栈形状与原方法一致，那条不变式自然成立。
//!
//! 支持 `invokestatic` 与「接收者是 final 类」的 `invokevirtual`（单态调用点）。

use std::collections::{HashMap, HashSet};

use crate::ClassFile;

use super::call::{code_of, method_ref_of, param_descs, param_slots};
use super::classes::ClassSet;
use super::insn::insn_len_at;

/// 被内联方法体的指令数上限（「小方法」）
const MAX_INSNS: usize = 32;
/// 内联深度上限（1 = 只内联被调方本身，2 = 被调方体内还能再内联一层）
const MAX_DEPTH: u32 = 2;
/// 同一个方法里最多内联多少个调用点（防代码膨胀）
const MAX_SITES: usize = 8;
/// 拼接后的字节码上限（`goto` 的偏移是 16 位有符号）
const MAX_BODY: usize = 30_000;

type Key = (String, String, String);

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Int,
    Long,
    Float,
    Double,
}

impl Kind {
    fn of(desc: &str) -> Option<Kind> {
        match desc.as_bytes().first()? {
            b'I' | b'S' | b'B' | b'C' | b'Z' => Some(Kind::Int),
            b'J' => Some(Kind::Long),
            b'F' => Some(Kind::Float),
            b'D' => Some(Kind::Double),
            _ => None, // 引用：不内联
        }
    }

    fn store_op(self) -> u8 {
        match self {
            Kind::Int => 0x36,
            Kind::Long => 0x37,
            Kind::Float => 0x38,
            Kind::Double => 0x39,
        }
    }
}

/// 一个候选的被内联方法
struct Callee {
    cf: std::rc::Rc<ClassFile>,
    kinds: Vec<Kind>,
    max_locals: usize,
    max_stack: usize,
    instance: bool,
    body: Vec<u8>,
}

/// 重写的结果
struct Rebuilt {
    bytes: Vec<u8>,
    /// 旧 pc → 新位置（异常表重定位要用）
    map: HashMap<usize, usize>,
    /// 这段代码往后用掉的局部变量槽数
    slots: usize,
    /// 这段代码需要的最深操作数栈
    stack: usize,
}

/// 把一个方法体里能内联的调用点就地展开。
/// 返回 (新字节码, 新 max_locals, 额外需要的 max_stack, 新异常表, 内联了几个点)。
#[allow(clippy::type_complexity)]
pub(super) fn inline_all(
    classes: &ClassSet,
    cf: &ClassFile,
    class: &str,
    name: &str,
    desc: &str,
    body: &[u8],
    max_locals: usize,
    max_stack: usize,
    exc_table: &[(u16, u16, u16, u16)],
) -> (Vec<u8>, usize, usize, Vec<(u16, u16, u16, u16)>, usize) {
    // 逃生阀：内联是最容易「算错但不崩」的变换，出问题时要能一键回到内联前的行为
    if std::env::var_os("VMLESS_JIT_NO_INLINE").is_some() {
        return (body.to_vec(), max_locals, 0, exc_table.to_vec(), 0);
    }
    let mut visited: HashSet<Key> = HashSet::new();
    visited.insert((class.to_string(), name.to_string(), desc.to_string()));
    match rebuild(classes, cf, body, 0, max_locals, MAX_DEPTH, &mut visited, MAX_SITES, false) {
        Some((r, sites)) if sites > 0 => {
            // 异常表按下标映射平移；映射不到（理论上不会，比如 end == body.len()）就整块放弃
            let mut table = Vec::with_capacity(exc_table.len());
            for &(s, e, h, t) in exc_table {
                let (Some(&ns), Some(&ne), Some(&nh)) = (
                    r.map.get(&(s as usize)),
                    r.map.get(&(e as usize)),
                    r.map.get(&(h as usize)),
                ) else {
                    return (body.to_vec(), max_locals, 0, exc_table.to_vec(), 0);
                };
                table.push((ns as u16, ne as u16, nh as u16, t));
            }
            (r.bytes, max_locals + r.slots, max_stack + r.stack, table, sites)
        }
        _ => (body.to_vec(), max_locals, 0, exc_table.to_vec(), 0),
    }
}

/// 重写一段字节码，把能内联的调用点就地展开，并把所有跳转/switch 偏移重定位。
///
/// `shift` = 这段代码自己的局部变量槽号要平移多少（调用方自己传 0，被调方传它的
/// 帧起点）；`next_free` = 往后第一个空闲槽（嵌套内联从这里开始分配）。
/// `callee_mode` = true 时把 `Xreturn` 换成「跳到本段末尾」（它拼进调用方之后，
/// 本段末尾就是调用之后要继续执行的位置）。
/// 返回 (重写结果, 内联了几个点)。
fn rebuild(
    classes: &ClassSet,
    cf: &ClassFile,
    body: &[u8],
    shift: usize,
    next_free: usize,
    depth: u32,
    visited: &mut HashSet<Key>,
    max_sites: usize,
    callee_mode: bool,
) -> Option<(Rebuilt, usize)> {
    let mut bytes: Vec<u8> = Vec::with_capacity(body.len() + 32);
    let mut map: HashMap<usize, usize> = HashMap::new();
    // (偏移字段位置, 基准位置, 目标旧 pc)：2 字节相对 / 4 字节相对都用它
    let mut fix16: Vec<(usize, usize)> = Vec::new();
    let mut fix32: Vec<(usize, usize, usize)> = Vec::new();
    let mut slots = 0usize;
    let mut stack = 0usize;
    let mut sites = 0usize;
    let mut pc = 0usize;
    while pc < body.len() {
        let op = body[pc];
        let len = insn_len_at(body, pc).max(1);
        if len < 1 || pc + len > body.len() {
            return None;
        }
        map.insert(pc, bytes.len());
        match op {
            // 局部变量短形式（槽号编在操作码里）：平移之后可能装不进短形式，改成长形式
            0x1a..=0x1d | 0x1e..=0x21 | 0x22..=0x25 | 0x26..=0x29 => {
                let slot = (op - 0x1a) as usize % 4 + shift;
                let base_op = match op {
                    0x1a..=0x1d => 0x15,
                    0x1e..=0x21 => 0x16,
                    0x22..=0x25 => 0x17,
                    _ => 0x18,
                };
                emit_local(&mut bytes, base_op, slot)?;
            }
            0x3b..=0x3e | 0x3f..=0x42 | 0x43..=0x46 | 0x47..=0x4a => {
                let slot = (op - 0x3b) as usize % 4 + shift;
                let base_op = match op {
                    0x3b..=0x3e => 0x36,
                    0x3f..=0x42 => 0x37,
                    0x43..=0x46 => 0x38,
                    _ => 0x39,
                };
                emit_local(&mut bytes, base_op, slot)?;
            }
            // 局部变量长形式：槽号整体平移 shift
            0x15..=0x18 | 0x36..=0x39 => {
                let slot = body[pc + 1] as usize + shift;
                if slot > 255 {
                    return None;
                }
                bytes.extend_from_slice(&[op, slot as u8]);
            }
            // iinc：槽号 + 增量
            0x84 => {
                let slot = body[pc + 1] as usize + shift;
                if slot > 255 {
                    return None;
                }
                bytes.extend_from_slice(&[0x84, slot as u8, body[pc + 2]]);
            }
            // 条件跳转 / goto / ifnull / ifnonnull：16 位相对偏移，目标要重定位
            0x99..=0xa7 | 0xc6 | 0xc7 => {
                let off = i16::from_be_bytes([body[pc + 1], body[pc + 2]]) as isize;
                let at = bytes.len();
                bytes.extend_from_slice(&[op, 0, 0]);
                fix16.push((at, (pc as isize + off) as usize));
            }
            // tableswitch / lookupswitch：4 字节对齐填充随位置变化，必须重排
            0xaa | 0xab => {
                let sw = bytes.len(); // 偏移的基准：switch 指令自身
                bytes.push(op);
                while bytes.len() % 4 != 0 {
                    bytes.push(0);
                }
                let old_pad = (4 - (pc + 1) % 4) % 4;
                let p = pc + 1 + old_pad;
                if p + 12 > body.len() {
                    return None;
                }
                let rd = |q: usize| -> Option<i32> {
                    if q + 4 > body.len() {
                        return None;
                    }
                    Some(i32::from_be_bytes([body[q], body[q + 1], body[q + 2], body[q + 3]]))
                };
                let default = rd(p)?;
                let pos = bytes.len();
                bytes.extend_from_slice(&[0, 0, 0, 0]);
                fix32.push((pos, sw, (pc as isize + default as isize) as usize));
                if op == 0xaa {
                    let lo = rd(p + 4)?;
                    let hi = rd(p + 8)?;
                    if hi < lo {
                        return None;
                    }
                    let n = (hi - lo + 1) as usize;
                    if n > 65535 || p + 12 + n * 4 > body.len() {
                        return None;
                    }
                    bytes.extend_from_slice(&body[p + 4..p + 12]); // lo / hi 原样抄
                    for i in 0..n {
                        let off = rd(p + 12 + i * 4)?;
                        let pos = bytes.len();
                        bytes.extend_from_slice(&[0, 0, 0, 0]);
                        fix32.push((pos, sw, (pc as isize + off as isize) as usize));
                    }
                } else {
                    let cnt = rd(p + 4)?;
                    if cnt < 0 || cnt > 65535 {
                        return None;
                    }
                    let n = cnt as usize;
                    if p + 8 + n * 8 > body.len() {
                        return None;
                    }
                    bytes.extend_from_slice(&body[p + 4..p + 8]); // npairs 原样抄
                    for i in 0..n {
                        let q = p + 8 + i * 8;
                        bytes.extend_from_slice(&body[q..q + 4]); // 键原样抄
                        let off = rd(q + 4)?;
                        let pos = bytes.len();
                        bytes.extend_from_slice(&[0, 0, 0, 0]);
                        fix32.push((pos, sw, (pc as isize + off as isize) as usize));
                    }
                }
            }
            // 返回：**直接丢掉**。被调方是直筒子，唯一的 return 必定是最后一条指令，
            // 丢掉之后调用方的下一条指令就接着执行，返回值仍留在操作数栈上 —— 与调用
            // 指令的栈效果完全一致。换成 `goto` 就错了：那会新增一个跳转点，而
            // `refmap` 在跳转点会清栈（见 body_allowed 的说明）。
            0xac..=0xaf | 0xb1 if callee_mode => {
                if pc + len != body.len() {
                    return None; // 不是末尾的 return 说明方法体不是直筒子
                }
            }
            // invoke*：能内联就地展开（不留池引用），否则原样抄
            0xb8 | 0xb6 => {
                let mut done = false;
                if sites < max_sites && depth > 0 && len == 3 {
                    let idx = u16::from_be_bytes([body[pc + 1], body[pc + 2]]);
                    let mut deeper: HashSet<Key> = visited.clone();
                    if let Some((code, used, deep)) =
                        emit_call(classes, cf, idx, op == 0xb6, next_free + slots, depth, &mut deeper)
                    {
                        bytes.extend_from_slice(&code);
                        slots += used;
                        stack = stack.max(deep);
                        sites += 1;
                        done = true;
                    }
                }
                if !done {
                    bytes.extend_from_slice(&body[pc..pc + len]);
                }
            }
            // goto_w / jsr_w / jsr / ret / 其它：位置会变就不能原样抄
            0xa8 | 0xa9 | 0xc4 | 0xc8 | 0xc9 => return None,
            _ => bytes.extend_from_slice(&body[pc..pc + len]),
        }
        pc += len;
    }
    map.insert(body.len(), bytes.len());
    if bytes.len() > MAX_BODY {
        return None;
    }
    // 填 16 位相对偏移
    for (at, target) in fix16 {
        let to = if target == usize::MAX { bytes.len() } else { *map.get(&target)? };
        let off = to as isize - at as isize;
        if off > i16::MAX as isize || off < i16::MIN as isize {
            return None;
        }
        bytes[at + 1] = ((off as i16) >> 8) as u8;
        bytes[at + 2] = (off as i16) as u8;
    }
    // 填 4 位相对偏移（switch：相对 switch 指令自身）
    for (at, sw, target) in fix32 {
        let to = *map.get(&target)?;
        let off = to as isize - sw as isize;
        if off > i32::MAX as isize || off < i32::MIN as isize {
            return None;
        }
        bytes[at..at + 4].copy_from_slice(&(off as i32).to_be_bytes());
    }
    Some((Rebuilt { bytes, map, slots, stack }, sites))
}

/// 压一条局部变量访问：槽号 <= 3 且是短形式那一档时仍用短形式，否则长形式（2 字节）。
/// 返回 None = 槽号超过 255。
fn emit_local(bytes: &mut Vec<u8>, base_op: u8, slot: usize) -> Option<()> {
    if slot > 255 {
        return None;
    }
    // 短形式表：iload/lload/fload/dload 是 0x1a/0x1e/0x22/0x26，
    // store 是 0x3b/0x3f/0x43/0x47；两者都「基址 + 槽号」，且同基址组内槽号 0..3。
    let short = match base_op {
        0x15 => Some(0x1a),
        0x16 => Some(0x1e),
        0x17 => Some(0x22),
        0x18 => Some(0x26),
        0x36 => Some(0x3b),
        0x37 => Some(0x3f),
        0x38 => Some(0x43),
        0x39 => Some(0x47),
        _ => None,
    };
    if let (Some(b), true) = (short, slot <= 3) {
        bytes.push(b + slot as u8);
    } else {
        bytes.extend_from_slice(&[base_op, slot as u8]);
    }
    Some(())
}

/// 展开一条调用：实参落槽 + 被调方方法体。返回 (字节, 用掉的槽数, 用掉的栈深)。
fn emit_call(
    classes: &ClassSet,
    cf: &ClassFile,
    idx: u16,
    virtual_call: bool,
    base: usize,
    depth: u32,
    visited: &mut HashSet<Key>,
) -> Option<(Vec<u8>, usize, usize)> {
    if depth == 0 {
        return None;
    }
    let (cls, name, cdesc) = method_ref_of(cf, idx).ok()?;
    let (owner, _) = classes.resolve_method(&cls, &name, &cdesc)?;
    let key: Key = (owner, name, cdesc);
    if visited.contains(&key) {
        return None; // 递归 / 环
    }
    let callee = load(classes, &key, virtual_call)?;
    let mut bytes = Vec::with_capacity(callee.body.len() + 16);
    // 实参落槽：栈上顺序是 [arg0 .. argN-1]，从后往前 store
    let dslots = param_slots(&key.2);
    let shift = if callee.instance { 1 } else { 0 };
    for i in (0..callee.kinds.len()).rev() {
        let slot = base + dslots.get(i).copied().unwrap_or(i) + shift;
        if slot > 255 {
            return None;
        }
        bytes.extend_from_slice(&[callee.kinds[i].store_op(), slot as u8]);
    }
    if virtual_call {
        bytes.push(0x57); // pop 接收者（引用只占 1 槽）
    }
    let mut deeper: HashSet<Key> = visited.clone();
    deeper.insert(key.clone());
    // 被调方的跳转全是**相对**偏移，整段搬过来偏移不变；它的 `Xreturn` 已经换成
    // 「跳到本段末尾」，而本段末尾就是调用之后继续执行的位置。
    let (region, _) = rebuild(
        classes,
        &callee.cf,
        &callee.body,
        base,
        base + callee.max_locals,
        depth - 1,
        &mut deeper,
        MAX_SITES,
        true,
    )?;
    bytes.extend_from_slice(&region.bytes);
    Some((bytes, callee.max_locals + region.slots, callee.max_stack + region.stack))
}

/// 取出一个可以内联的方法（不满足条件就 `None`）
fn load(classes: &ClassSet, key: &Key, want_virtual: bool) -> Option<Callee> {
    let (class, name, desc) = (&key.0, &key.1, &key.2);
    let cf = classes.get(class)?;
    let m = cf.methods.iter().find(|m| {
        let n = String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default());
        let d = String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default());
        n == *name && d == *desc
    })?;
    let is_static = m.access_flags & 0x0008 != 0;
    if m.access_flags & 0x0020 != 0 {
        return None; // synchronized
    }
    if want_virtual == is_static {
        return None; // 调用形式与方法种类对不上
    }
    // 虚调用只有「目标类 final」才算单态；接口/父类都可能被覆盖
    if want_virtual && cf.access_flags & 0x0010 == 0 {
        return None;
    }
    let descs = param_descs(desc);
    let mut kinds = Vec::with_capacity(descs.len());
    for d in &descs {
        kinds.push(Kind::of(d)?);
    }
    let (max_locals, max_stack, body, table) = code_of(&cf, m).ok()?;
    if !table.is_empty() {
        return None; // 有异常表：不内联
    }
    if !body_allowed(&body) {
        return None;
    }
    Some(Callee { cf, kinds, max_locals, max_stack, instance: !is_static, body })
}

/// 白名单校验：指令数、**无常量池引用**、无引用操作、无异常、**无分支**。
///
/// 为什么要禁分支：`translate` 与 `refmap` 都建立在这条不变式上 ——「跳转点上
/// 操作数栈必空」（`translate.rs` 到块首时 `if !stack.is_empty() { return Err }`，
/// `refmap` 线性走一遍时到块首直接 `stack.clear()`）。被内联方法体里的跳转在
/// **它自己那一帧**里栈是空的，可拼进调用方之后，调用点下面若还压着活值（最典型
/// 的就是 `System.out.println(f(x))` 里的 `System.out`），那些跳转点的绝对栈就
/// 不空了 —— 实测 `Perf2.main` 直接编译失败。
///
/// 直筒子方法体没有跳转，于是**一个块首都不会新增**，调用点下面压着什么活值都无所谓，
/// 栈形状与原来逐 pc 一致。要放开这条限制，就得先给调用点做栈深分析并在必要时把
/// 活值溢出到局部变量槽（见本文件头部注释）。
fn body_allowed(body: &[u8]) -> bool {
    let mut pc = 0usize;
    let mut n = 0usize;
    while pc < body.len() {
        let op = body[pc];
        let len = insn_len_at(body, pc).max(1);
        if pc + len > body.len() {
            return false;
        }
        n += 1;
        if n > MAX_INSNS {
            return false;
        }
        match op {
            // 常量 + 局部变量 + 栈操作 + 算术 + 转换 + 比较
            0x00 | 0x02..=0x11 | 0x15..=0x18 | 0x1a..=0x29 | 0x36..=0x39 | 0x3b..=0x4a
            | 0x57..=0x5f | 0x60..=0x84 | 0x85..=0x98 => {}
            // 返回（分支一律不收：见上面 body_allowed 的说明）
            0xac..=0xaf | 0xb1 => {}
            // 嵌套调用：池引用在拼接时会被整体替换掉，深度由 rebuild 控制
            0xb6 | 0xb8 => {}
            _ => return false,
        }
        pc += len;
    }
    true
}
