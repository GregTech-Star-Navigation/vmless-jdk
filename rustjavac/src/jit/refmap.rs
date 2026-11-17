//! 操作数栈「哪个栈位是引用」分析。
//!
//! JIT 在 GC 点要把操作数栈上的引用写进影子栈（每帧的根区），GC 搬过对象之后
//! 还要把新偏移读回来 —— 这两件事都必须知道**精确**的引用位置：多标一个，GC
//! 会把一个 int 当偏移改写掉；少标一个，活对象会被回收。
//!
//! 好消息是不需要数据流：每条压栈指令自己就决定了压上去的是不是引用
//! （`aload` 压引用、`iload` 压 int、`invoke*` 看返回描述符…），而 `translate`
//! 要求**每个跳转点的操作数栈为空**，所以照着字节码线性走一遍得到的状态在每条
//! 路径上都成立。
//!
//! 这个模块只负责算，不负责生成代码；遇到 `translate` 不支持的字节码就报同样的错，
//! 保证两边对「支持哪些指令」的判断不会悄悄分叉。

use std::collections::{HashMap, HashSet};

use cranelift_codegen::ir::Block;

use super::call::{argc_of, method_ref_of};
use super::indy;
use super::insn::insn_len_at;
use super::layout::field_ref_of;
use super::JitError;
use crate::{ClassFile, CpEntry};

/// 每个 pc → 执行**这条指令之前**操作数栈上各槽是否为引用
pub(super) type RefMap = HashMap<usize, Vec<bool>>;

/// 分析结果：每个 pc 的栈引用位图 + 「这个方法用不用引用」
pub(super) struct Refs {
    pub(super) at: RefMap,
    /// 字节码里出现过引用（压栈 / 局部变量 / handler）—— 没有就不必维护影子栈
    pub(super) uses_ref: bool,
}

/// 算一份 `pc → 栈上引用位图`
pub(super) fn analyze(
    cf: &ClassFile,
    body: &[u8],
    blocks: &HashMap<usize, Block>,
    handler_pcs: &HashSet<usize>,
) -> Result<Refs, JitError> {
    let mut out: RefMap = HashMap::new();
    let mut uses_ref = false;
    let mut stack: Vec<Val> = Vec::new();
    let mut pc = 0usize;
    while pc < body.len() {
        // translate 在块首要求栈空（否则直接报错），这里跟着清，状态与它一致
        if blocks.contains_key(&pc) && pc != 0 {
            stack.clear();
        }
        if handler_pcs.contains(&pc) {
            stack.push(R); // handler 入口栈上有一个异常对象
            uses_ref = true;
        }
        let before = ref_bits(&stack);
        out.insert(pc, before);
        let op = body[pc];
        let (pops, pushes) = effect(cf, body, pc, op, &mut stack)?;
        for _ in 0..pops {
            if stack.pop().is_none() {
                return Err(JitError(format!(
                    "pc={pc} 栈下溢（引用分析）：op=0x{op:02x} 需要弹出 {pops} 个，实际只有 {}",
                    stack.len()
                )));
            }
        }
        for v in pushes {
            uses_ref |= v.0;
            stack.push(v);
        }
        // 终结指令之后是死代码：translate 也会清栈
        if matches!(op, 0xac..=0xb1 | 0xbf | 0xa7) {
            stack.clear();
        }
        pc += insn_len_at(body, pc);
    }
    Ok(Refs { at: out, uses_ref })
}

/// 栈上一个值的类别：`(是否引用, 是否 cat-2 宽值)`。
///
/// dup/dup_x*/dup2 系列的**形式**取决于栈顶值的槽宽（cat-2 = long/double），
/// 只看引用位会算错弹出数量，于是后面所有 pc 的引用位置整体错位 —— 错标的引用会让
/// GC 把一个 int 当偏移改写掉。所以这里两种信息都要带走。
type Val = (bool, bool);
/// 引用（cat-1）
const R: Val = (true, false);
/// 普通 1 槽基本类型（int/float/…）
const V: Val = (false, false);
/// long / double（cat-2）
const W: Val = (false, true);

fn ref_bits(vs: &[Val]) -> Vec<bool> {
    vs.iter().map(|(r, _)| *r).collect()
}

/// 一条指令的 (弹出数, 压入的值)。`stack` 是当前栈（`dup` 之类要看栈顶）。
fn effect(
    cf: &ClassFile,
    body: &[u8],
    pc: usize,
    op: u8,
    stack: &mut Vec<Val>,
) -> Result<(usize, Vec<Val>), JitError> {
    let u1 = || body.get(pc + 1).copied().unwrap_or(0);
    let u2 = || u16::from_be_bytes([body.get(pc + 1).copied().unwrap_or(0), body.get(pc + 2).copied().unwrap_or(0)]);
    // 栈顶第 n 个值（1 = 栈顶）
    let nth = |n: usize| stack.iter().rev().nth(n - 1).copied().unwrap_or(V);
    Ok(match op {
        0x00 => (0, vec![]),                      // nop
        0x01 => (0, vec![R]),                     // aconst_null
        0x02..=0x0d | 0x10 | 0x11 => (0, vec![V]), // iconst/fconst/bipush/sipush
        0x0e | 0x0f => (0, vec![W]),              // dconst_0/1
        0x12 => (0, vec![ldc_val(cf, u1() as u16)]), // ldc
        0x13 => (0, vec![ldc_val(cf, u2())]),          // ldc_w
        0x14 => (0, vec![W]),                     // ldc2_w（long/double）
        0x15 | 0x17 => (0, vec![V]),              // iload / fload
        0x16 | 0x18 => (0, vec![W]),              // lload / dload
        0x19 => (0, vec![R]),                     // aload
        0x1a..=0x1d => (0, vec![V]),              // iload_0..3
        0x1e..=0x21 => (0, vec![W]),              // lload_0..3
        0x22..=0x25 => (0, vec![V]),              // fload_0..3
        0x26..=0x29 => (0, vec![W]),              // dload_0..3
        0x2a..=0x2d => (0, vec![R]),              // aload_0..3
        0x2e | 0x30 | 0x33 | 0x34 | 0x35 => (2, vec![V]), // i/f/ba/ca/saload
        0x2f | 0x31 => (2, vec![W]),              // laload / daload
        0x32 => (2, vec![R]),                     // aaload
        0x36 | 0x38 | 0x3a => (1, vec![]),        // istore/fstore/astore
        0x37 | 0x39 => (1, vec![]),               // lstore/dstore
        0x3b..=0x3e => (1, vec![]),               // istore_0..3
        0x3f..=0x42 => (1, vec![]),               // lstore_0..3
        0x43..=0x46 => (1, vec![]),               // fstore_0..3
        0x47..=0x4a => (1, vec![]),               // dstore_0..3
        0x4b..=0x4e => (1, vec![]),               // astore_0..3
        0x4f..=0x56 => (3, vec![]),               // i/l/f/d/a/ba/ca/sastore
        0x57 => (1, vec![]),                      // pop
        0x58 => (if nth(1).1 { 1 } else { 2 }, vec![]), // pop2：宽值一个，否则两个
        0x59 => (0, vec![nth(1)]),                // dup
        0x5a => (2, vec![nth(1), nth(2), nth(1)]), // dup_x1
        0x5b => {
            // dup_x2：下面那个是宽值时只跨一个
            if nth(2).1 {
                (2, vec![nth(1), nth(2), nth(1)])
            } else {
                (3, vec![nth(1), nth(3), nth(2), nth(1)])
            }
        }
        0x5c => {
            // dup2：栈顶是宽值就复制一个，否则复制两个
            if nth(1).1 {
                (1, vec![nth(1), nth(1)])
            } else {
                (2, vec![nth(2), nth(1), nth(2), nth(1)])
            }
        }
        0x5d => {
            // dup2_x1
            if nth(1).1 {
                (2, vec![nth(1), nth(2), nth(1)])
            } else {
                (3, vec![nth(2), nth(1), nth(3), nth(2), nth(1)])
            }
        }
        0x5e => {
            // dup2_x2：形式 4 = 栈顶两项都宽；形式 3 = 只有栈顶宽；
            // 形式 2 = 第三项宽；形式 1 = 四个都是 1 槽
            if nth(1).1 && nth(2).1 {
                (2, vec![nth(1), nth(2), nth(1)]) // 形式 4
            } else if nth(1).1 {
                (3, vec![nth(1), nth(3), nth(2), nth(1)]) // 形式 3
            } else if nth(3).1 {
                (3, vec![nth(2), nth(1), nth(3), nth(2), nth(1)]) // 形式 2
            } else {
                (4, vec![nth(2), nth(1), nth(4), nth(3), nth(2), nth(1)]) // 形式 1
            }
        }
        0x5f => (2, vec![nth(1), nth(2)]),         // swap
        0x60 | 0x62 | 0x64 | 0x66 | 0x68 | 0x6a | 0x6c | 0x6e | 0x70 | 0x72 | 0x7e | 0x80 | 0x82
        | 0x78 | 0x7a | 0x7c => (2, vec![V]),     // int/float 的二元运算（+ - * / % 位运算 移位）
        0x61 | 0x63 | 0x65 | 0x67 | 0x69 | 0x6b | 0x6d | 0x6f | 0x71 | 0x73 | 0x7f | 0x81 | 0x83
        | 0x79 | 0x7b | 0x7d => (2, vec![W]),     // long/double 的二元运算
        0x74 | 0x76 => (1, vec![V]),              // ineg / fneg
        0x75 | 0x77 => (1, vec![W]),              // lneg / dneg
        0x84 => (0, vec![]),                      // iinc
        0x85 => (1, vec![W]),                     // i2l
        0x86 => (1, vec![V]),                     // i2f
        0x87 => (1, vec![W]),                     // i2d
        0x88 => (1, vec![V]),                     // l2i
        0x89 => (1, vec![V]),                     // l2f
        0x8a => (1, vec![W]),                     // l2d
        0x8b => (1, vec![V]),                     // f2i
        0x8c => (1, vec![W]),                     // f2l
        0x8d => (1, vec![W]),                     // f2d
        0x8e => (1, vec![V]),                     // d2i
        0x8f => (1, vec![W]),                     // d2l
        0x90 => (1, vec![V]),                     // d2f
        0x91 | 0x92 | 0x93 => (1, vec![V]),       // i2b / i2c / i2s
        0x94 => (2, vec![V]),                     // lcmp
        0x95 | 0x96 => (2, vec![V]),              // fcmpl / fcmpg
        0x97 | 0x98 => (2, vec![V]),              // dcmpl / dcmpg
        0x99..=0x9e => (1, vec![]),               // if<cond>
        0x9f..=0xa6 => (2, vec![]),               // if_icmp<cond> / if_acmp<cond>
        0xa7 => (0, vec![]),                      // goto
        0xaa | 0xab => (1, vec![]),               // tableswitch / lookupswitch：弹出选择子
        0xac => (1, vec![]),                      // ireturn
        0xad | 0xaf => (1, vec![]),               // lreturn / dreturn
        0xae => (1, vec![]),                      // freturn
        0xb0 => (1, vec![]),                      // areturn
        0xb1 => (0, vec![]),                      // return
        0xb2 => {
            // getstatic
            let (_, _, desc) = field_ref_of(cf, u2())?;
            (0, vec![desc_val(&desc)])
        }
        0xb3 => (1, vec![]),                      // putstatic
        0xb4 => {
            let (_, _, desc) = field_ref_of(cf, u2())?;
            (1, vec![desc_val(&desc)])
        }
        0xb5 => (2, vec![]),                      // putfield
        0xb6 | 0xb7 | 0xb8 | 0xb9 => {
            let (_, _, desc) = method_ref_of(cf, u2())?;
            let argc = argc_of(&desc);
            let pops = (if op == 0xb8 { argc } else { argc + 1 }) as usize;
            // 返回 void 的调用不往栈上压东西
            let ret = desc.split_once(')').map(|(_, r)| r).unwrap_or("V");
            let pushes = if ret.starts_with('V') { vec![] } else { vec![desc_val(ret)] };
            (pops, pushes)
        }
        0xbb => (0, vec![R]),                     // new
        0xbc | 0xbd => (1, vec![R]),              // newarray / anewarray
        0xbe => (1, vec![V]),                     // arraylength
        0xbf => (1, vec![]),                      // athrow
        0xc0 => (1, vec![R]),                     // checkcast
        0xc1 => (1, vec![V]),                     // instanceof
        0xc5 => (body.get(pc + 3).copied().unwrap_or(0) as usize, vec![R]), // multianewarray
        0xc6 | 0xc7 => (1, vec![]),               // ifnull / ifnonnull
        // invokedynamic：弹掉站点的参数、压回返回值（`StringConcatFactory` 压的是字符串引用）。
        // 能不能编译不在这里判 —— 这里只管**操作数栈的形状**，接不了的那个调用点
        // 由翻译器在 0xba 处报错（方法级回退）。
        0xba => {
            let idx = u2();
            let (_, desc, _) = indy_site(cf, idx)?;
            let argc = argc_of(&desc);
            let ret = desc.split_once(')').map(|(_, r)| r).unwrap_or("V");
            let pushes = if ret.starts_with('V') { vec![] } else { vec![desc_val(ret)] };
            (argc as usize, pushes)
        }
        other => return Err(JitError(format!("JIT 暂不支持字节码 0x{other:02x}"))),
    })
}

/// invokedynamic 调用点的 `(名字, 描述符, 引导方法索引)`（栈形状分析只要描述符）
fn indy_site(cf: &ClassFile, idx: u16) -> Result<(String, String, u16), JitError> {
    indy::site_of(cf, idx).map_err(JitError)
}

/// 描述符 → 栈上值类别
fn desc_val(desc: &str) -> Val {
    match desc.as_bytes().first().copied() {
        Some(b'L') | Some(b'[') => R,
        Some(b'J') | Some(b'D') => W,
        _ => V,
    }
}

fn ldc_val(cf: &ClassFile, idx: u16) -> Val {
    if ldc_is_ref(cf, idx) { R } else { V }
}

fn is_ref_desc(d: &str) -> bool {
    matches!(d.as_bytes().first().copied(), Some(b'L') | Some(b'['))
}

/// `ldc` 的常量是不是引用（只有 String 常量是）
fn ldc_is_ref(cf: &ClassFile, idx: u16) -> bool {
    matches!(
        cf.constant_pool.get(idx as usize).and_then(|e| e.as_ref()),
        Some(CpEntry::String(_))
    )
}
