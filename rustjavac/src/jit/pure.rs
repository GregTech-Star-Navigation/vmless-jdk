//! 「纯数值方法」判据 —— 跨解释器/JIT 边界调用的前置条件。
//!
//! 两套运行时各有自己的堆与静态字段表，所以能被**对方**调用的方法必须满足：
//!
//! 1. 参数与返回值里没有引用（只有基本类型跨越边界才有意义）；
//! 2. 是 `static` 方法（实例方法第 0 槽是 `this`，那本身就是引用）；
//! 3. 字节码里不碰任何对象/数组/字段/监视器/异常，也不调 `java.*`；
//! 4. 它调用的方法也满足 1–3（调用图上**传递闭包**都得干净）。
//!
//! 这是**语义安全**判据而不是「能编译」判据：JIT 能编一个 `new Object()` 的方法，
//! 但那个对象在 JIT 的堆里，解释器看不见它。

use std::collections::HashSet;

use crate::jit::call::code_of;
use crate::jit::insn::insn_len_at;
use crate::jit::Jit;
use crate::{ClassFile, CpEntry};

type Key = (String, String, String);

/// 这个方法自己的字节码干净吗？干净就给出一条条调用边，否则 `None`
fn scan(jit: &Jit, cf: &ClassFile, key: &Key) -> Option<Vec<Key>> {
    let desc = &key.2;
    // 1) 签名里不许有引用
    if crate::vm::has_ref(desc) {
        return None;
    }
    let m = cf.methods.iter().find(|m| {
        let n = String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default());
        let d = String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default());
        n == key.1 && d == *desc
    })?;
    // 2) 必须是静态、非 synchronized
    if m.access_flags & 0x0008 == 0 || m.access_flags & 0x0020 != 0 {
        return None;
    }
    let (_, _, body, _) = code_of(cf, m).ok()?;
    let mut edges = Vec::new();
    let mut pc = 0usize;
    while pc < body.len() {
        let op = body[pc];
        let len = insn_len_at(&body, pc).max(1);
        match op {
            // 常量：int/long/float/double 的字面量都安全（`ldc` 要看常量池类型）
            0x00 | 0x02..=0x11 | 0x14 => {}
            0x12 | 0x13 => {
                let at = pc + 1;
                let idx = if op == 0x12 {
                    body.get(at).copied().unwrap_or(0) as u16
                } else {
                    u16::from_be_bytes([body.get(at).copied()? as u8, body.get(at + 1).copied()?])
                };
                match cf.constant_pool.get(idx as usize).and_then(|e| e.as_ref()) {
                    Some(CpEntry::Integer(_)) | Some(CpEntry::Float(_)) => {}
                    _ => return None,
                }
            }
            // 局部变量：aload/astore 之外都安全
            0x15..=0x18 | 0x1a..=0x29 | 0x36..=0x39 | 0x3b..=0x4a => {}
            0x57..=0x5f => {}                 // pop / dup / swap
            0x60..=0x84 => {}                 // 算术、位运算、iinc
            0x85..=0x93 => {}                 // 数值转换（i2l / l2d …）
            0x94..=0x98 => {}                 // lcmp / fcmp / dcmp
            0x99..=0x9e => {}                 // if<cond>
            0x9f..=0xa4 => {}                 // if_icmp<cond>（if_acmp 见下）
            0xa7 | 0xaa | 0xab | 0xc8 => {}   // goto / switch / goto_w
            0xac..=0xaf | 0xb1 => {}          // ireturn/lreturn/freturn/dreturn/return
            0xb7 | 0xb8 => {
                // 静态调用：目标必须解析得出来，并且（传递地）也是纯数值方法
                let idx = u16::from_be_bytes([body.get(pc + 1).copied()?, body.get(pc + 2).copied()?]);
                let (c, n, d) = crate::jit::call::method_ref_of(cf, idx).ok()?;
                let (owner, _) = jit.classes.resolve_method(&c, &n, &d)?;
                edges.push((owner, n, d));
            }
            _ => return None,
        }
        pc += len;
    }
    Some(edges)
}

/// 全部编译过的方法里，哪些可以跨运行时调用
pub(super) fn pure_set(jit: &Jit) -> HashSet<Key> {
    let mut alive: HashSet<Key> = HashSet::new();
    let mut edges: Vec<(Key, Vec<Key>)> = Vec::new();
    for key in jit.compiled.keys() {
        let Some(cf) = jit.classes.get(&key.0) else { continue };
        if let Some(e) = scan(jit, &cf, key) {
            alive.insert(key.clone());
            edges.push((key.clone(), e));
        }
    }
    // 不动点：调用边上有脏的就把源头也剔掉（自递归不算脏）
    loop {
        let mut changed = false;
        for (key, out) in &edges {
            if !alive.contains(key) {
                continue;
            }
            if out.iter().any(|t| !alive.contains(t)) {
                alive.remove(key);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    alive
}

/// 造解释器能回调的 JIT 方法表
pub(super) fn pure_table(jit: &Jit) -> crate::vm::JitTable {
    let pure = pure_set(jit);
    let mut out = Vec::new();
    for key in pure {
        let Some((fid, max_locals, _)) = jit.compiled.get(&key) else { continue };
        let ptr = jit.module.get_finalized_function(*fid) as usize;
        let ret = key.2.rsplit(')').next().and_then(|r| r.bytes().next()).unwrap_or(b'V');
        out.push((
            key.0,
            key.1,
            key.2,
            crate::vm::JitEntry { code: ptr, max_locals: *max_locals, ret },
        ));
    }
    out.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
    out
}
