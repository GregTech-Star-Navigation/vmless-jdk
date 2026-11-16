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
use super::insn::insn_len;
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
    let mut stack: Vec<bool> = Vec::new();
    let mut pc = 0usize;
    while pc < body.len() {
        // translate 在块首要求栈空（否则直接报错），这里跟着清，状态与它一致
        if blocks.contains_key(&pc) && pc != 0 {
            stack.clear();
        }
        if handler_pcs.contains(&pc) {
            stack.push(true); // handler 入口栈上有一个异常对象
            uses_ref = true;
        }
        let before = stack.clone();
        out.insert(pc, before);
        let op = body[pc];
        let (pops, pushes) = effect(cf, body, pc, op, &mut stack)?;
        for _ in 0..pops {
            if stack.pop().is_none() {
                return Err(JitError(format!(
                    "pc={pc} 栈下溢（引用分析）：op=0x{op:02x} 需要弹出 {pops} 个，实际只有 {}",
                    stack.len() + 1
                )));
            }
        }
        for r in pushes {
            uses_ref |= r;
            stack.push(r);
        }
        // 终结指令之后是死代码：translate 也会清栈
        if matches!(op, 0xac..=0xb1 | 0xbf | 0xa7) {
            stack.clear();
        }
        pc += insn_len(op);
    }
    Ok(Refs { at: out, uses_ref })
}

/// 一条指令的 (弹出数, 压入的引用位图)。`stack` 是当前栈（`dup` 之类要看栈顶）。
fn effect(
    cf: &ClassFile,
    body: &[u8],
    pc: usize,
    op: u8,
    stack: &mut Vec<bool>,
) -> Result<(usize, Vec<bool>), JitError> {
    let u1 = || body.get(pc + 1).copied().unwrap_or(0);
    let u2 = || u16::from_be_bytes([body.get(pc + 1).copied().unwrap_or(0), body.get(pc + 2).copied().unwrap_or(0)]);
    Ok(match op {
        0x00 => (0, vec![]),                      // nop
        0x01 => (0, vec![true]),                  // aconst_null
        0x02..=0x11 => (0, vec![false]),          // iconst/lconst/fconst/dconst/bipush/sipush
        0x12 => (0, vec![ldc_is_ref(cf, u1() as u16)]), // ldc
        0x13 => (0, vec![ldc_is_ref(cf, u2())]),        // ldc_w
        0x14 => (0, vec![false]),                 // ldc2_w
        0x15 | 0x16 | 0x18 => (0, vec![false]),   // iload/lload/dload
        0x19 => (0, vec![true]),                  // aload
        0x1a..=0x1d => (0, vec![false]),          // iload_0..3
        0x1e..=0x21 => (0, vec![false]),          // lload_0..3
        0x26..=0x29 => (0, vec![false]),          // dload_0..3
        0x2a..=0x2d => (0, vec![true]),           // aload_0..3
        0x2e | 0x2f | 0x30 | 0x31 | 0x33 | 0x34 | 0x35 => (2, vec![false]), // i/l/f/d/ba/ca/saload
        0x32 => (2, vec![true]),                  // aaload
        0x36 | 0x37 | 0x38 | 0x39 | 0x3a => (1, vec![]), // i/l/f/d/astore
        0x3b..=0x3e => (1, vec![]),               // istore_0..3
        0x3f..=0x42 => (1, vec![]),               // lstore_0..3
        0x47..=0x4a => (1, vec![]),               // dstore_0..3
        0x4b..=0x4e => (1, vec![]),               // astore_0..3
        0x4f..=0x56 => (3, vec![]),               // i/l/f/d/a/ba/ca/sastore
        0x57 | 0x58 => (1, vec![]),               // pop / pop2
        0x59 => (0, vec![stack.last().copied().unwrap_or(false)]), // dup：复制栈顶，不弹出
        0x60 | 0x61 | 0x63 | 0x64 | 0x65 | 0x67 | 0x68 | 0x69 | 0x6b | 0x6c | 0x6d | 0x6f | 0x70
        | 0x71 | 0x73 => (2, vec![false]),        // 二元算术（i/l/d 的 + - * / %）
        0x74 | 0x75 | 0x77 => (1, vec![false]),   // ineg/lneg/dneg
        0x78 | 0x7a | 0x7c | 0x79 | 0x7b | 0x7d => (2, vec![false]), // 移位
        0x7e | 0x80 | 0x82 | 0x7f | 0x81 | 0x83 => (2, vec![false]), // 位运算
        0x84 => (0, vec![]),                      // iinc
        0x85 | 0x87 | 0x88 | 0x8a | 0x8e | 0x8f => (1, vec![false]), // 类型转换
        0x94 | 0x97 | 0x98 => (2, vec![false]),   // lcmp / fcmp / dcmp
        0x99..=0x9e => (1, vec![]),               // if<cond>
        0x9f..=0xa6 => (2, vec![]),               // if_icmp<cond> / if_acmp<cond>
        0xa7 => (0, vec![]),                      // goto
        0xac..=0xb0 => (1, vec![]),               // <t>return
        0xb1 => (0, vec![]),                      // return
        0xb2 => {
            // getstatic
            let (_, _, desc) = field_ref_of(cf, u2())?;
            (0, vec![is_ref_desc(&desc)])
        }
        0xb3 => (1, vec![]),                      // putstatic
        0xb4 => {
            let (_, _, desc) = field_ref_of(cf, u2())?;
            (1, vec![is_ref_desc(&desc)])
        }
        0xb5 => (2, vec![]),                      // putfield
        0xb6 | 0xb7 | 0xb8 | 0xb9 => {
            let (_, _, desc) = method_ref_of(cf, u2())?;
            let argc = argc_of(&desc);
            let pops = (if op == 0xb8 { argc } else { argc + 1 }) as usize;
            // 返回 void 的调用不往栈上压东西
            let ret = desc.split_once(')').map(|(_, r)| r).unwrap_or("V");
            let pushes = if ret.starts_with('V') { vec![] } else { vec![is_ref_desc(ret)] };
            (pops, pushes)
        }
        0xbb => (0, vec![true]),                  // new
        0xbc | 0xbd => (1, vec![true]),           // newarray / anewarray
        0xbe => (1, vec![false]),                 // arraylength
        0xbf => (1, vec![]),                      // athrow
        0xc0 => (1, vec![true]),                  // checkcast
        0xc1 => (1, vec![false]),                 // instanceof
        0xc5 => (body.get(pc + 3).copied().unwrap_or(0) as usize, vec![true]), // multianewarray
        0xc6 | 0xc7 => (1, vec![]),               // ifnull / ifnonnull
        other => return Err(JitError(format!("JIT 暂不支持字节码 0x{other:02x}"))),
    })
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
