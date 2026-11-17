//! 停靠点（safepoint）+ **精确根**的标记-清扫 GC + 每帧引用位图。
//!
//! ## 为什么要引用位图
//!
//! 解释器的 `Frame.locals` 是 `Vec<Slot>`，引用（句柄）和 int/long 混在一起，没有类型
//! 标签。追溯扫描无法区分「句柄 5」和「整数 5」：把整数当引用 → 把随机下标当对象；
//! 漏掉真引用 → 活对象被回收。所以每个方法在**第一次执行时**用字节码数据流算出一张
//! `pc → (局部槽引用位, 操作数栈引用位)` 的表（[`MethodRefs`]），运行期帧带着它。
//!
//! 这张表就是 JVM 校验器（verifier）做的事，只是格（lattice）简化成「引用 / 非引用 +
//! 槽宽」：`aload` 压引用、`iload` 压 int、`invoke*` 看返回描述符、`getfield` 看字段
//! 描述符…… 所有跳转点用**并（OR）**合并（并是往「多标」的方向走，安全）；一旦发现
//! 结构上对不上（栈深不一致、宽窄不一致、不认识的指令），整个方法退化成**保守位图**
//! （每个槽都当潜在引用，运行时逐个用页表验证）—— 多留垃圾，但绝不漏根。
//!
//! ## 回收策略
//!
//! - 标记-清扫、**不搬动**（页表里的对象地址是句柄的全部含义，搬动会让所有句柄失效）。
//! - 停在**解释器主循环的指令边界**上回收：`pc` 精确、栈形状精确 —— 这是本设计里
//!   最重要的一条约束。分配函数自己**不**触发回收，它只累加计数。
//! - 清扫在暂停之外并发做（`VMLESS_GC_STATS=1` 里 pause 与 sweep 分开报）。
//! - 索引**不复用**：句柄是页表下标，复用会让陈旧句柄指向另一台对象。
//!
//! ## 根集
//!
//! 1. 每个登记线程的**帧链**（精确位图；对不上就保守）；
//! 2. 每线程的**Rust 栈**（保守扫描）—— 解释器/native/反射代码里临时拿着句柄的
//!    情况（异常对象在展开途中、indy 里的临时值）靠它兜住；
//! 3. 共享冷表（静态字段、类对象表、字符串常量表、indy 状态、监视器表、线程表…），
//!    一律按「表里出现的 i32 都可能是句柄」处理并逐个用页表验证（表都很小，多标有界）。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::{ClassFile, CpEntry};

use super::frame::Frame;
use super::shared::{Shared, STR_BASE};
use super::slot::Slot;
use super::Vm;

// ---------------------------------------------------------------- 引用位图

fn bits_get(b: &[u64], i: usize) -> bool {
    b.get(i / 64).map(|w| (w >> (i % 64)) & 1 != 0).unwrap_or(false)
}

fn bits_set(b: &mut [u64], i: usize) {
    if let Some(w) = b.get_mut(i / 64) {
        *w |= 1 << (i % 64);
    }
}

/// 一个字节码位置上「哪些槽是引用」。
pub(crate) struct Shape {
    lbits: Box<[u64]>,
    sbits: Box<[u64]>,
    depth: u16,
    /// 保守形状：所有槽都是潜在引用
    all: bool,
}

impl Shape {
    #[inline]
    pub(crate) fn local_is_ref(&self, i: usize) -> bool {
        self.all || bits_get(&self.lbits, i)
    }
    #[inline]
    pub(crate) fn stack_is_ref(&self, i: usize) -> bool {
        self.all || bits_get(&self.sbits, i)
    }
    #[inline]
    pub(crate) fn depth(&self) -> usize {
        self.depth as usize
    }
    #[inline]
    pub(crate) fn is_all(&self) -> bool {
        self.all
    }
}

/// 一个方法的引用位图：`pc → Shape`。
pub(crate) struct MethodRefs {
    /// `body.len()` 项；下标 = 字节码偏移，值 = `shapes` 下标（0 = 保守形状）
    at: Vec<u16>,
    shapes: Vec<Shape>,
}

impl MethodRefs {
    #[inline]
    pub(crate) fn shape_at(&self, pc: usize) -> Option<&Shape> {
        if pc == usize::MAX {
            return None;
        }
        self.at.get(pc).map(|&i| &self.shapes[i as usize])
    }

    /// 整张表都保守（诊断/统计用）
    pub(crate) fn is_conservative(&self) -> bool {
        self.shapes.len() == 1
    }

    fn all_conservative(n: usize) -> MethodRefs {
        MethodRefs {
            at: vec![0; n.max(1)],
            shapes: vec![Shape { lbits: Box::new([0; 4]), sbits: Box::new([0; 4]), depth: 0, all: true }],
        }
    }
}

/// 栈上一个值的类别（分析期的抽象值）
type Val = u8;
const V: Val = 0; // 基本类型，1 槽
const R: Val = 1; // 引用，1 槽
const W: Val = 2; // long/double，2 槽（bit0 = 0 非引用，bit1 = 1 宽）

#[inline]
fn is_ref_val(v: Val) -> bool {
    v & 1 == 1
}

#[derive(Clone, PartialEq)]
struct St {
    /// 局部槽：true = 引用（按 JVM 槽编号）
    locals: Vec<bool>,
    stack: Vec<Val>,
}

impl St {
    fn pop(&mut self) -> Result<Val, ()> {
        self.stack.pop().ok_or(())
    }
    fn popn(&mut self, n: usize) -> Result<(), ()> {
        for _ in 0..n {
            self.pop()?;
        }
        Ok(())
    }
    fn push(&mut self, v: Val) {
        self.stack.push(v);
    }
    fn top(&self, n: usize) -> Result<Val, ()> {
        self.stack.iter().rev().nth(n - 1).copied().ok_or(())
    }
    fn set_local(&mut self, i: usize, v: Val) {
        let wide = v & 2 != 0;
        if i < self.locals.len() {
            self.locals[i] = is_ref_val(v);
        }
        if wide && i + 1 < self.locals.len() {
            self.locals[i + 1] = false;
        }
    }
}

/// 一条指令的执行结果
struct Step {
    /// 会不会落到下一条指令
    fall: bool,
    /// 跳转目标
    targets: Vec<usize>,
}

impl Step {
    fn fall() -> Step {
        Step { fall: true, targets: Vec::new() }
    }
}

// ---------------------------------------------------------------- 描述符小工具

/// 参数类型列表（`(IJLjava/lang/String;)V` → `["I","J","Ljava/lang/String;"]`）
fn param_types(desc: &str) -> Vec<&str> {
    let b = desc.as_bytes();
    let mut i = 1;
    let mut out = Vec::new();
    let mut start;
    while i < b.len() && b[i] != b')' {
        start = i;
        while i < b.len() && b[i] == b'[' {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        match b[i] {
            b'L' => {
                while i < b.len() && b[i] != b';' {
                    i += 1;
                }
                i += 1;
            }
            _ => i += 1,
        }
        out.push(&desc[start..i.min(desc.len())]);
    }
    out
}

/// 返回类型（`(I)Ljava/lang/String;` → `Ljava/lang/String;`）
fn return_desc(desc: &str) -> &str {
    match desc.find(')') {
        Some(i) => &desc[i + 1..],
        None => "V",
    }
}

/// 类型描述符 → 它在操作数栈上占的抽象值
fn val_of(ty: &str) -> Val {
    match ty.as_bytes().first().copied() {
        Some(b'L') | Some(b'[') => R,
        Some(b'J') | Some(b'D') => W,
        _ => V,
    }
}

fn is_ref_ty(ty: &str) -> bool {
    matches!(ty.as_bytes().first().copied(), Some(b'L') | Some(b'['))
}

/// 指令长度（不认识的指令 → None，调用方退化成保守位图）
fn insn_len(body: &[u8], pc: usize) -> Option<usize> {
    let op = *body.get(pc)?;
    let n = match op {
        // 无操作数
        0x00..=0x0f | 0x1a..=0x35 | 0x3b..=0x83 | 0x85..=0x98 | 0xac..=0xb1
        | 0xbe | 0xbf | 0xc2 | 0xc3 => 1,
        0x10 | 0x12 | 0x15..=0x19 | 0x36..=0x3a | 0xa9 | 0xbc => 2,
        0x11 | 0x13 | 0x14 | 0x84 | 0x99..=0xa8 | 0xb2..=0xb8 | 0xbb | 0xbd
        | 0xc0 | 0xc1 | 0xc6 | 0xc7 => 3,
        0xb9 | 0xba | 0xc8 | 0xc9 => 5,
        0xc5 => 4,
        0xc4 => return None, // wide：罕见，直接保守
        0xaa => {
            // tableswitch：0-3 字节 padding + default + low + high + (high-low+1) 个 offset
            let mut q = pc + 1;
            while q % 4 != 0 {
                q += 1;
            }
            let low = i32::from_be_bytes([*body.get(q + 4)?, *body.get(q + 5)?, *body.get(q + 6)?, *body.get(q + 7)?]);
            let high = i32::from_be_bytes([*body.get(q + 8)?, *body.get(q + 9)?, *body.get(q + 10)?, *body.get(q + 11)?]);
            let n = (high - low + 1).max(0) as usize;
            q + 12 + n * 4 - pc
        }
        0xab => {
            let mut q = pc + 1;
            while q % 4 != 0 {
                q += 1;
            }
            let n = i32::from_be_bytes([*body.get(q + 4)?, *body.get(q + 5)?, *body.get(q + 6)?, *body.get(q + 7)?]);
            if n < 0 {
                return None;
            }
            q + 8 + n as usize * 8 - pc
        }
        _ => return None,
    };
    if pc + n <= body.len() {
        Some(n)
    } else {
        None
    }
}

/// 常量池项 → 描述符（字段引用 / 方法引用）
fn ref_desc(cf: &ClassFile, index: u16) -> Option<String> {
    let e = cf.constant_pool.get(index as usize).and_then(|e| e.as_ref())?;
    let nat = match e {
        CpEntry::FieldRef(_, n) | CpEntry::MethodRef(_, n) | CpEntry::InterfaceMethodRef(_, n) => *n,
        CpEntry::InvokeDynamic(_, n) => *n,
        _ => return None,
    };
    match cf.constant_pool.get(nat as usize).and_then(|e| e.as_ref())? {
        CpEntry::NameAndType(_, d) => Some(String::from_utf8_lossy(cf.utf8(*d)?).into_owned()),
        _ => None,
    }
}

fn ldc_val(cf: &ClassFile, index: u16) -> Option<Val> {
    match cf.constant_pool.get(index as usize).and_then(|e| e.as_ref())? {
        CpEntry::Integer(_) | CpEntry::Float(_) => Some(V),
        // 字符串 / Class / 方法句柄 / 方法类型 / 动态常量：一律当引用看（多标安全）
        _ => Some(R),
    }
}

// ---------------------------------------------------------------- 数据流分析

/// 算一个方法的引用位图。
///
/// 从方法入口出发做不动点迭代：每条指令按自己的语义弹出/压入抽象值，跳转目标处
/// 用「并」合并状态（引用并引用 = 引用，非引用并非引用 = 非引用，混了 → 冲突）。
/// 任何不理解、对不上的情形都退化成 [`MethodRefs::all_conservative`]。
pub(crate) fn analyze(
    cf: &ClassFile,
    body: &[u8],
    desc: &str,
    max_locals: usize,
    is_static: bool,
    exc: &[(u16, u16, u16, u16)],
) -> MethodRefs {
    let n = body.len();
    if n == 0 {
        return MethodRefs::all_conservative(1);
    }
    // 1) 指令起点（线性扫一遍：长度表不认识就整体保守）
    let mut starts: Vec<usize> = Vec::new();
    let mut p = 0usize;
    while p < n {
        starts.push(p);
        match insn_len(body, p) {
            Some(l) if l > 0 => p += l,
            _ => return MethodRefs::all_conservative(n),
        }
    }
    let mut next: HashMap<usize, usize> = HashMap::with_capacity(starts.len());
    for w in starts.windows(2) {
        next.insert(w[0], w[1]);
    }

    // 2) 入口状态：this + 参数
    let locals_n = max_locals.max(1);
    let mut init = St { locals: vec![false; locals_n], stack: Vec::new() };
    let mut slot = 0usize;
    if !is_static {
        init.locals[0] = true;
        slot = 1;
    }
    for ty in param_types(desc) {
        init.set_local(slot, val_of(ty));
        slot += if ty.as_bytes().first() == Some(&b'J') || ty.as_bytes().first() == Some(&b'D') { 2 } else { 1 };
    }

    let mut states: Vec<Option<St>> = vec![None; n];
    states[0] = Some(init);
    let mut work: VecDeque<usize> = VecDeque::new();
    work.push_back(0);
    // 冲突就整体保守（安全方向：多标）。`VMLESS_REFMAP_DEBUG=1` 打印退化原因
    let dbg = std::env::var("VMLESS_REFMAP_DEBUG").map(|v| v != "0").unwrap_or(false);
    let conflict = |n: usize, why: &str| {
        if dbg {
            eprintln!("[refmap] 退化（{why}）：{desc}");
        }
        MethodRefs::all_conservative(n)
    };

    while let Some(pc) = work.pop_front() {
        let mut st = match states[pc].take() {
            Some(s) => s,
            None => continue,
        };
        // 处理过（states[pc] 被置回）之后再用同一份状态合并会重复，所以先还原
        states[pc] = Some(st.clone());

        let op = body[pc];
        let before = st.clone();
        let step = match step(cf, body, pc, op, &mut st) {
            Some(s) => s,
            None => {
                if dbg {
                    eprintln!("[refmap] pc={pc} op=0x{op:02x} 不支持/栈不匹配：{desc}");
                }
                return MethodRefs::all_conservative(n);
            }
        };

        // 3) 异常 handler：本指令抛异常时，handler 入口的局部变量 = 「本指令之前」的
        //    局部变量（见模块注释：取并，安全），栈上只有一个异常对象引用。
        for &(s, e, h, _ct) in exc {
            let (s, e, h) = (s as usize, e as usize, h as usize);
            if pc >= s && pc < e {
                if h >= n {
                    return conflict(n, "分析冲突");
                }
                let mut hs = St { locals: before.locals.clone(), stack: vec![R] };
                if pc >= 0 {
                    // 存储类指令也可能抛（aastore），把改过的局部变量也算进去
                    for (i, v) in st.locals.iter().enumerate() {
                        if *v {
                            hs.locals[i] = true;
                        }
                    }
                }
                match merge_into(&mut states, h, hs) {
                    Ok(true) => work.push_back(h),
                    Ok(false) => {}
                    Err(()) => return conflict(n, "合并冲突（栈深/宽窄不一致）"),
                }
            }
        }

        // 4) 后继
        let mut succ: Vec<usize> = step.targets;
        if step.fall {
            if let Some(&nx) = next.get(&pc) {
                succ.push(nx);
            }
        }
        for s in succ {
            if s >= n {
                return conflict(n, "分析冲突");
            }
            match merge_into(&mut states, s, st.clone()) {
                Ok(true) => work.push_back(s),
                Ok(false) => {}
                Err(()) => return conflict(n, "合并冲突（栈深/宽窄不一致）"),
            }
        }
    }

    // 5) 收尾：每个指令起点 → Shape，相同的状态共用一个 Shape
    let mut shapes: Vec<Shape> = vec![Shape {
        lbits: Box::new([0; 4]),
        sbits: Box::new([0; 4]),
        depth: 0,
        all: true,
    }];
    let mut seen: HashMap<(Vec<u64>, Vec<u64>, u16), u16> = HashMap::new();
    let mut at = vec![0u16; n];
    for &pc in &starts {
        let Some(st) = states[pc].as_ref() else { continue };
        if st.stack.len() > u16::MAX as usize {
            return conflict(n, "分析冲突");
        }
        let lb = to_bits(locals_n, |i| st.locals.get(i).copied().unwrap_or(false));
        let sb = to_bits(st.stack.len(), |i| st.stack.get(i).map(|v| is_ref_val(*v)).unwrap_or(false));
        let key = (lb.clone(), sb.clone(), st.stack.len() as u16);
        let idx = match seen.get(&key) {
            Some(&i) => i,
            None => {
                let i = shapes.len() as u16;
                if i == u16::MAX {
                    return conflict(n, "分析冲突");
                }
                shapes.push(Shape { lbits: lb.into_boxed_slice(), sbits: sb.into_boxed_slice(), depth: key.2, all: false });
                seen.insert(key, i);
                i
            }
        };
        at[pc] = idx;
    }
    MethodRefs { at, shapes }
}

fn to_bits(n: usize, f: impl Fn(usize) -> bool) -> Vec<u64> {
    let mut v = vec![0u64; (n + 63) / 64];
    for i in 0..n {
        if f(i) {
            bits_set(&mut v, i);
        }
    }
    v
}

fn merge_into(states: &mut [Option<St>], pc: usize, inc: St) -> Result<bool, ()> {
    match states.get_mut(pc).and_then(|s| s.as_mut()) {
        None => {
            states[pc] = Some(inc);
            Ok(true)
        }
        Some(cur) => {
            if cur.stack.len() != inc.stack.len() {
                return Err(());
            }
            let mut changed = false;
            for (a, b) in cur.stack.iter_mut().zip(inc.stack.iter()) {
                if (*a & 2) != (*b & 2) {
                    return Err(()); // 宽窄不一致：分析不可靠
                }
                let merged = *a | (*b & 1);
                if merged != *a {
                    *a = merged;
                    changed = true;
                }
            }
            for (i, b) in inc.locals.iter().enumerate() {
                if *b && !cur.locals.get(i).copied().unwrap_or(false) {
                    if let Some(x) = cur.locals.get_mut(i) {
                        *x = true;
                        changed = true;
                    }
                }
            }
            Ok(changed)
        }
    }
}

/// 一条指令的语义。`None` = 不认识的形态（整体保守）。
fn step(cf: &ClassFile, body: &[u8], pc: usize, op: u8, st: &mut St) -> Option<Step> {
    let g1 = |o: usize| -> Option<usize> { body.get(pc + 1).map(|b| *b as usize + o) };
    let u1 = || -> Option<usize> { body.get(pc + 1).map(|b| *b as usize) };
    let u2 = || -> Option<usize> {
        Some(u16::from_be_bytes([*body.get(pc + 1)?, *body.get(pc + 2)?]) as usize)
    };
    let s2 = || -> Option<isize> {
        Some(i16::from_be_bytes([*body.get(pc + 1)?, *body.get(pc + 2)?]) as isize)
    };
    let _ = g1;
    let mut out = Step::fall();
    match op {
        0x00 => {}
        0x01 => st.push(R),                                // aconst_null
        0x02..=0x08 => st.push(V),                         // iconst_*
        0x09 | 0x0a => st.push(W),                         // lconst_*
        0x0b..=0x0d => st.push(V),                         // fconst_*
        0x0e | 0x0f => st.push(W),                         // dconst_*
        0x10 | 0x11 => st.push(V),                         // bipush / sipush
        0x12 => st.push(ldc_val(cf, u1()? as u16)?),       // ldc
        0x13 => st.push(ldc_val(cf, u2()? as u16)?),       // ldc_w
        0x14 => st.push(W),                                // ldc2_w
        0x15 => st.push(V),                                // iload
        0x16 => st.push(W),                                // lload
        0x17 => st.push(V),                                // fload
        0x18 => st.push(W),                                // dload
        0x19 => st.push(R),                                // aload
        0x1a..=0x1d => st.push(V),                         // iload_0..3
        0x1e..=0x21 => st.push(W),                         // lload_0..3
        0x22..=0x25 => st.push(V),                         // fload_0..3
        0x26..=0x29 => st.push(W),                         // dload_0..3
        0x2a..=0x2d => st.push(R),                         // aload_0..3
        0x2e => { st.popn(2).ok()?; st.push(V); }          // iaload
        0x2f => { st.popn(2).ok()?; st.push(W); }          // laload
        0x30 => { st.popn(2).ok()?; st.push(V); }          // faload
        0x31 => { st.popn(2).ok()?; st.push(W); }          // daload
        0x32 => { st.popn(2).ok()?; st.push(R); }          // aaload
        0x33..=0x35 => { st.popn(2).ok()?; st.push(V); }   // baload/caload/saload
        0x36 => { let i = u1()?; st.pop().ok()?; st.set_local(i, V); } // istore
        0x37 => { let i = u1()?; st.pop().ok()?; st.set_local(i, W); } // lstore
        0x38 => { let i = u1()?; st.pop().ok()?; st.set_local(i, V); } // fstore
        0x39 => { let i = u1()?; st.pop().ok()?; st.set_local(i, W); } // dstore
        0x3a => { let i = u1()?; st.pop().ok()?; st.set_local(i, R); } // astore
        0x3b..=0x3e => { st.pop().ok()?; st.set_local((op - 0x3b) as usize, V); } // istore_0..3
        0x3f..=0x42 => { st.pop().ok()?; st.set_local((op - 0x3f) as usize, W); } // lstore_0..3
        0x43..=0x46 => { st.pop().ok()?; st.set_local((op - 0x43) as usize, V); } // fstore_0..3
        0x47..=0x4a => { st.pop().ok()?; st.set_local((op - 0x47) as usize, W); } // dstore_0..3
        0x4b..=0x4e => { st.pop().ok()?; st.set_local((op - 0x4b) as usize, R); } // astore_0..3
        0x4f..=0x56 => { st.popn(3).ok()?; }               // *astore
        0x57 => { st.pop().ok()?; }                        // pop
        0x58 => {
            // pop2：一个宽值或两个窄值
            let t = st.top(1).ok()?;
            st.pop().ok()?;
            if t & 2 == 0 {
                st.pop().ok()?;
            }
        }
        0x59 => { let t = st.top(1).ok()?; st.push(t); }   // dup
        0x5a => {                                          // dup_x1
            let a = st.top(1).ok()?;
            let b = st.top(2).ok()?;
            if a & 2 != 0 || b & 2 != 0 { return None; }
            st.popn(2).ok()?;
            st.push(a); st.push(b); st.push(a);
        }
        0x5b => {                                          // dup_x2
            let a = st.top(1).ok()?;
            let b = st.top(2).ok()?;
            if a & 2 != 0 { return None; }
            if b & 2 != 0 {
                // 形式 2：… value2(宽), value1 → value1, value2, value1
                st.popn(2).ok()?;
                st.push(a); st.push(b); st.push(a);
            } else {
                let c = st.top(3).ok()?;
                st.popn(3).ok()?;
                st.push(a); st.push(c); st.push(b); st.push(a);
            }
        }
        0x5c => {                                          // dup2
            let a = st.top(1).ok()?;
            if a & 2 != 0 {
                st.push(a);
            } else {
                let b = st.top(2).ok()?;
                if b & 2 != 0 { return None; }
                st.push(b); st.push(a);
            }
        }
        0x5d => {                                          // dup2_x1
            let a = st.top(1).ok()?;
            if a & 2 != 0 {
                let b = st.top(2).ok()?;
                st.popn(2).ok()?;
                st.push(a); st.push(b); st.push(a);
            } else {
                let b = st.top(2).ok()?;
                let c = st.top(3).ok()?;
                if b & 2 != 0 || c & 2 != 0 { return None; }
                st.popn(3).ok()?;
                st.push(b); st.push(a); st.push(c); st.push(b); st.push(a);
            }
        }
        0x5e => {                                          // dup2_x2
            let a = st.top(1).ok()?;
            if a & 2 != 0 {
                let b = st.top(2).ok()?;
                if b & 2 != 0 {
                    st.popn(2).ok()?;
                    st.push(a); st.push(b); st.push(a);
                } else {
                    let c = st.top(3).ok()?;
                    st.popn(3).ok()?;
                    st.push(a); st.push(c); st.push(b); st.push(a);
                }
            } else {
                let b = st.top(2).ok()?;
                let c = st.top(3).ok()?;
                if b & 2 != 0 || c & 2 != 0 { return None; }
                if let Ok(d) = st.top(4) {
                    if d & 2 != 0 {
                        st.popn(4).ok()?;
                        st.push(b); st.push(a); st.push(d); st.push(c); st.push(b); st.push(a);
                    } else {
                        return None;
                    }
                } else {
                    return None;
                }
            }
        }
        0x5f => {                                          // swap
            let a = st.top(1).ok()?;
            let b = st.top(2).ok()?;
            if a & 2 != 0 || b & 2 != 0 { return None; }
            st.popn(2).ok()?;
            st.push(a); st.push(b);
        }
        // 算术/位运算：结果类型由指令决定（0x60 起四族各 6 条，逐族判断）
        0x60..=0x63 | 0x64..=0x67 | 0x68..=0x6b | 0x6c..=0x6f | 0x70..=0x73 | 0x74..=0x77
        | 0x78..=0x7b | 0x7c..=0x7f => {
            // iadd..ixor (V) / ladd..lxor (W)：每 4 条一族
            let ty = match (op - 0x60) / 8 {
                0 => V, // i*
                1 => W, // l*
                2 => V, // f*
                _ => W, // d*
            };
            st.popn(2).ok()?;
            st.push(ty);
        }
        0x80..=0x83 => { st.popn(2).ok()?; st.push(V); }   // ior ixor iand (含 0x7f/0x81/0x83)
        0x84 => { let i = u1()?; st.set_local(i, V); }      // iinc
        0x85 => { st.pop().ok()?; st.push(W); }            // i2l
        0x86 => { st.pop().ok()?; st.push(V); }            // i2f
        0x87 => { st.pop().ok()?; st.push(W); }            // i2d
        0x88 => { st.pop().ok()?; st.push(V); }            // l2i
        0x89 => { st.pop().ok()?; st.push(V); }            // l2f
        0x8a => { st.pop().ok()?; st.push(W); }            // l2d
        0x8b => { st.pop().ok()?; st.push(V); }            // f2i
        0x8c => { st.pop().ok()?; st.push(W); }            // f2l
        0x8d => { st.pop().ok()?; st.push(W); }            // f2d
        0x8e => { st.pop().ok()?; st.push(V); }            // d2i
        0x8f => { st.pop().ok()?; st.push(W); }            // d2l
        0x90 => { st.pop().ok()?; st.push(V); }            // d2f
        0x91 => { st.pop().ok()?; st.push(V); }            // i2b
        0x92 => { st.pop().ok()?; st.push(V); }            // i2c
        0x93 => { st.pop().ok()?; st.push(V); }            // i2s
        0x94..=0x98 => { st.popn(2).ok()?; st.push(if op == 0x97 { V } else { W }); } // lcmp/fcmpl…
        0x99..=0x9e => {                                   // if<cond>
            st.pop().ok()?;
            out.targets.push((pc as isize + s2()?) as usize);
        }
        0x9f..=0xa4 => {                                   // if_icmp<cond>
            st.popn(2).ok()?;
            out.targets.push((pc as isize + s2()?) as usize);
        }
        0xa5 | 0xa6 => {                                   // if_acmpeq/ne
            st.popn(2).ok()?;
            out.targets.push((pc as isize + s2()?) as usize);
        }
        0xa7 => {                                          // goto
            out.fall = false;
            out.targets.push((pc as isize + s2()?) as usize);
        }
        0xa8 | 0xa9 | 0xc9 => return None,                 // jsr/ret/jsr_w：子程序，不支持
        0xaa => {                                          // tableswitch
            st.pop().ok()?;
            out.fall = false;
            let mut q = pc + 1;
            while q % 4 != 0 {
                q += 1;
            }
            let rd = |i: usize| -> Option<i32> {
                Some(i32::from_be_bytes([*body.get(i)?, *body.get(i + 1)?, *body.get(i + 2)?, *body.get(i + 3)?]))
            };
            let default = rd(q)?;
            let low = rd(q + 4)?;
            let high = rd(q + 8)?;
            out.targets.push((pc as isize + default as isize) as usize);
            let n = (high - low + 1).max(0) as usize;
            for i in 0..n {
                let off = rd(q + 12 + i * 4)?;
                out.targets.push((pc as isize + off as isize) as usize);
            }
        }
        0xab => {                                          // lookupswitch
            st.pop().ok()?;
            out.fall = false;
            let mut q = pc + 1;
            while q % 4 != 0 {
                q += 1;
            }
            let rd = |i: usize| -> Option<i32> {
                Some(i32::from_be_bytes([*body.get(i)?, *body.get(i + 1)?, *body.get(i + 2)?, *body.get(i + 3)?]))
            };
            let default = rd(q)?;
            let n = rd(q + 4)?;
            if n < 0 {
                return None;
            }
            out.targets.push((pc as isize + default as isize) as usize);
            for i in 0..n as usize {
                let off = rd(q + 8 + i * 8 + 4)?;
                out.targets.push((pc as isize + off as isize) as usize);
            }
        }
        0xac => { st.pop().ok()?; out.fall = false; }      // ireturn
        0xad => { st.pop().ok()?; out.fall = false; }      // lreturn
        0xae => { st.pop().ok()?; out.fall = false; }      // freturn
        0xaf => { st.pop().ok()?; out.fall = false; }      // dreturn
        0xb0 => { st.pop().ok()?; out.fall = false; }      // areturn
        0xb1 => { out.fall = false; }                      // return
        0xb2 => {                                          // getstatic
            let d = ref_desc(cf, u2()? as u16)?;
            st.push(val_of(&d));
        }
        0xb3 => {                                          // putstatic
            st.pop().ok()?;
        }
        0xb4 => {                                          // getfield
            let d = ref_desc(cf, u2()? as u16)?;
            st.pop().ok()?;
            st.push(val_of(&d));
        }
        0xb5 => {                                          // putfield
            st.popn(2).ok()?;
        }
        0xb6 | 0xb7 | 0xb9 => {                            // invokevirtual/special/interface
            let d = ref_desc(cf, u2()? as u16)?;
            let argc = param_types(&d).len();
            st.popn(argc + 1).ok()?;
            push_ret(st, &d);
        }
        0xb8 => {                                          // invokestatic
            let d = ref_desc(cf, u2()? as u16)?;
            let argc = param_types(&d).len();
            st.popn(argc).ok()?;
            push_ret(st, &d);
        }
        0xba => {                                          // invokedynamic
            let d = ref_desc(cf, u2()? as u16)?;
            let argc = param_types(&d).len();
            st.popn(argc).ok()?;
            push_ret(st, &d);
        }
        0xbb => { let _ = u2()?; st.push(R); }               // new（不弹东西！）
        0xbc | 0xbd => { let _ = u2()?; st.popn(1).ok()?; st.push(R); } // newarray/anewarray（弹长度）
        0xbe => { st.pop().ok()?; st.push(V); }            // arraylength
        0xbf => {                                          // athrow
            st.pop().ok()?;
            out.fall = false;
        }
        0xc0 => { st.pop().ok()?; st.push(R); }            // checkcast
        0xc1 => { st.pop().ok()?; st.push(V); }            // instanceof
        0xc2 | 0xc3 => { st.pop().ok()?; }                 // monitorenter / monitorexit
        0xc5 => {                                          // multianewarray
            let nd = *body.get(pc + 3)? as usize;
            st.popn(nd).ok()?;
            st.push(R);
        }
        0xc6 | 0xc7 => {                                   // ifnull / ifnonnull
            st.pop().ok()?;
            out.targets.push((pc as isize + s2()?) as usize);
        }
        0xc8 => {                                          // goto_w
            out.fall = false;
            let off = i32::from_be_bytes([
                *body.get(pc + 1)?,
                *body.get(pc + 2)?,
                *body.get(pc + 3)?,
                *body.get(pc + 4)?,
            ]);
            out.targets.push((pc as isize + off as isize) as usize);
        }
        _ => return None,
    }
    Some(out)
}

fn push_ret(st: &mut St, desc: &str) {
    let r = return_desc(desc);
    if r != "V" {
        st.push(val_of(r));
    }
}

// ---------------------------------------------------------------- 帧登记（根集）

/// 每个线程的根：帧指针链 + 临时根。
///
/// 帧只在**自己的线程**里 push/pop（GC 是在世界停住时读的），所以这里是
/// 「一个线程写、GC 停住时读」—— `UnsafeCell` + 停靠点协议已经保证了互斥。
pub(crate) struct ThreadRoots {
    frames: std::cell::UnsafeCell<Vec<*const Frame>>,
    temps: std::cell::UnsafeCell<Vec<i32>>,
    /// 栈顶（最高地址）：保守扫描的下界用不到，上界用
    stack_base: usize,
    /// 最近一次 park / 采集时的栈指针（保守扫描从这里往上扫）
    sp: AtomicUsize,
    /// 本线程 `Vm` 的地址（GC 要读它的 `held`：线程持有的监视器句柄）
    vm: std::cell::UnsafeCell<*const Vm>,
    /// 本线程已经停靠到哪一轮（`>= 当前轮次` = 在停靠点里停着）
    parked: AtomicUsize,
}

unsafe impl Send for ThreadRoots {}
unsafe impl Sync for ThreadRoots {}

impl ThreadRoots {
    fn new(stack_base: usize) -> ThreadRoots {
        ThreadRoots {
            frames: std::cell::UnsafeCell::new(Vec::with_capacity(256)),
            temps: std::cell::UnsafeCell::new(Vec::new()),
            stack_base,
            sp: AtomicUsize::new(0),
            vm: std::cell::UnsafeCell::new(std::ptr::null()),
            parked: AtomicUsize::new(0),
        }
    }
}

thread_local! {
    static MY_ROOTS: std::cell::Cell<*const ThreadRoots> = const { std::cell::Cell::new(std::ptr::null()) };
}

/// 本线程的临时根（异常对象在展开途中之类）
pub(crate) fn temp_root(h: i32) -> usize {
    if h == 0 {
        return usize::MAX;
    }
    MY_ROOTS.with(|r| {
        let p = r.get();
        if p.is_null() {
            return usize::MAX;
        }
        unsafe {
            let t = &mut *(*p).temps.get();
            t.push(h);
            t.len() - 1
        }
    })
}

/// 撤销一个临时根（按 `temp_root` 返回的下标）
pub(crate) fn temp_unroot(idx: usize) {
    if idx == usize::MAX {
        return;
    }
    MY_ROOTS.with(|r| {
        let p = r.get();
        if p.is_null() {
            return;
        }
        unsafe {
            let t = &mut *(*p).temps.get();
            if idx + 1 == t.len() {
                t.pop();
            } else if idx < t.len() {
                t[idx] = 0;
            }
        }
    });
}

/// 帧登记守卫：`execute_body` 里创建它，函数返回（含 `?` 提前返回）时自动摘掉。
pub(crate) struct FrameGuard {
    roots: *const ThreadRoots,
    /// 登记前的帧数（Drop 时截回这个长度 —— 比 pop 更抗异常路径）
    depth: usize,
}

impl FrameGuard {
    #[inline]
    pub(super) fn push(f: &Frame, vm: *const Vm) -> FrameGuard {
        let p = MY_ROOTS.with(|r| r.get());
        if p.is_null() {
            return FrameGuard { roots: std::ptr::null(), depth: 0 };
        }
        unsafe {
            *(*p).vm.get() = vm;
            let v = &mut *(*p).frames.get();
            v.push(f as *const Frame);
            FrameGuard { roots: p, depth: v.len() }
        }
    }
}

impl Drop for FrameGuard {
    fn drop(&mut self) {
        if self.roots.is_null() {
            return;
        }
        unsafe {
            let v = &mut *(*self.roots).frames.get();
            v.truncate(self.depth.saturating_sub(1));
        }
    }
}

/// 已登记线程的根表（GC 从这张表拿别人的帧）
struct RootsTable(Mutex<Vec<*const ThreadRoots>>);
// SAFETY: 表里存的是别的线程 `Box` 出来的 `ThreadRoots` —— 它们在 `unregister_roots` 之前
// 一直有效，而那时线程自己也已经退出了字节码执行。
unsafe impl Send for RootsTable {}
unsafe impl Sync for RootsTable {}
static ALL_ROOTS: RootsTable = RootsTable(Mutex::new(Vec::new()));

/// 本线程登记的根（`Safepoint::register` 里调）
fn register_roots() {
    // 栈底：取本帧里一个局部变量的地址当上界（栈往下长，往上是调用者）
    let anchor = 0u8;
    let base = &anchor as *const u8 as usize;
    let boxed = Box::into_raw(Box::new(ThreadRoots::new(base)));
    MY_ROOTS.with(|r| r.set(boxed));
    ALL_ROOTS.0.lock().unwrap_or_else(|e| e.into_inner()).push(boxed);
}

fn unregister_roots() {
    let p = MY_ROOTS.with(|r| r.replace(std::ptr::null()));
    if p.is_null() {
        return;
    }
    let mut g = ALL_ROOTS.0.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(i) = g.iter().position(|x| *x == p) {
        g.swap_remove(i);
    }
}

// ---------------------------------------------------------------- 停靠点

/// 卡表：8 字节粒度标记「这块地址上有引用被写过」。
///
/// 当前 GC 是**全量标记**，用不到增量标记的脏卡；留着它是为了下一步的增量/分代
/// 回收（写屏障已经在 `Heap::set_ref` 接好了），现在是**关着**的（开着只是白写内存）。
static CARDS_ENABLED: AtomicBool = AtomicBool::new(false);
const CARD_SHIFT: u32 = 9; // 512 字节/卡
const CARD_WORDS: usize = 1 << 16;
static CARDS: [AtomicU64; CARD_WORDS] = {
    #[allow(clippy::declare_interior_mutable_const)]
    const Z: AtomicU64 = AtomicU64::new(0);
    [Z; CARD_WORDS]
};

/// 引用字段写入的写屏障。
#[inline]
pub(crate) fn dirty_write(off: u32) {
    if !CARDS_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let card = (off as usize >> CARD_SHIFT) & (CARD_WORDS - 1);
    let bit = off as usize & 63;
    CARDS[card].fetch_or(1 << bit, Ordering::Relaxed);
}

/// 打开卡表（增量标记用；当前全量标记不需要）
pub(crate) fn cards_enable(on: bool) {
    CARDS_ENABLED.store(on, Ordering::Relaxed);
}

pub(crate) struct Safepoint {
    epoch: AtomicUsize,
    active: AtomicUsize,
    requested: AtomicBool,
    lock: Mutex<()>,
    cv: Condvar,
}

impl Safepoint {
    pub(crate) fn new() -> Safepoint {
        Safepoint {
            epoch: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            requested: AtomicBool::new(false),
            lock: Mutex::new(()),
            cv: Condvar::new(),
        }
    }

    pub(crate) fn register(&self) {
        // 正在 GC 时先等它结束：否则新线程的帧不在这一轮的根集里，它手上的对象会被扫掉
        {
            let mut g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
            while self.requested.load(Ordering::Acquire) {
                let (ng, _) = self
                    .cv
                    .wait_timeout(g, std::time::Duration::from_millis(50))
                    .unwrap_or_else(|e| e.into_inner());
                g = ng;
            }
        }
        register_roots();
        self.active.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn unregister(&self) {
        unregister_roots();
        self.active.fetch_sub(1, Ordering::AcqRel);
        self.cv.notify_all();
    }

    /// 字节码主循环里的检查：稳态成本是一条 relaxed 读。
    #[inline]
    pub(crate) fn poll(&self) {
        if self.requested.load(Ordering::Relaxed) {
            self.park();
        }
    }

    /// 到停靠点：报到 → 睡到本轮结束
    pub(crate) fn park(&self) {
        let anchor = 0u8;
        MY_ROOTS.with(|r| {
            let p = r.get();
            if !p.is_null() {
                unsafe { (*p).sp.store(&anchor as *const u8 as usize, Ordering::Release) };
            }
        });
        loop {
            if !self.requested.load(Ordering::Acquire) {
                return; // 这一轮已经结束了
            }
            let epoch = self.epoch.load(Ordering::Acquire);
            // 先宣告「我停在第 epoch 轮」，再睡 —— 发起方按**每个线程各自的轮次**判定是否停全，
            // 这样上一轮迟到的宣告不会被下一轮误当成「已到齐」。
            MY_ROOTS.with(|r| {
                let p = r.get();
                if !p.is_null() {
                    unsafe { (*p).parked.store(epoch, Ordering::Release) };
                }
            });
            let mut g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
            while self.epoch.load(Ordering::Acquire) == epoch {
                if !self.requested.load(Ordering::Acquire) {
                    break;
                }
                self.cv.notify_all(); // 叫醒发起方：我又到了一个
                g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
            }
            drop(g);
            if !self.requested.load(Ordering::Acquire) {
                return;
            }
            // 还在请求里（新一轮开始了）：继续停在下一个停靠点
        }
    }

    /// 发起一轮停靠：等所有登记线程到达（`timeout_ms` 内），返回「是否全部到齐」。
    ///
    /// 没到齐也不死等 —— 返回 false 让调用方放弃这一轮（**没停全就不回收**）。
    pub(crate) fn sync(&self, timeout_ms: u64) -> bool {
        let me = MY_ROOTS.with(|r| r.get()) as usize;
        let round = self.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        self.requested.store(true, Ordering::Release);

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        let mut ok = false;
        loop {
            // 每个登记线程各自宣告「我停在第几轮」：只要还有人的轮次 < 本轮，就还没停全
            let pending = {
                let g = ALL_ROOTS.0.lock().unwrap_or_else(|e| e.into_inner());
                g.iter()
                    .filter(|p| !p.is_null() && **p as usize != me)
                    .filter(|p| unsafe { (***p).parked.load(Ordering::Acquire) } < round)
                    .count()
            };
            if pending == 0 {
                ok = true;
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_micros(50));
        }
        // 结束这一轮（再 +1 让所有等着的线程醒来）
        self.requested.store(false, Ordering::Release);
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.cv.notify_all();
        ok
    }

    pub(crate) fn requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    /// 本线程登记过吗（没登记就不能发起 GC：停不住别人）
    pub(crate) fn is_registered(&self) -> bool {
        !MY_ROOTS.with(|r| r.get()).is_null()
    }
}

pub(crate) fn in_safepoint_thread() -> bool {
    !MY_ROOTS.with(|r| r.get()).is_null()
}

// ---------------------------------------------------------------- 采集根

/// 标记器：句柄工作栈 + 世代号
struct Marker<'a> {
    sh: &'a Shared,
    /// 只收集候选句柄（不追对象图）—— 根扫描阶段用，收集完再并行标记
    gathering: bool,
    epoch: u32,
    work: Vec<i32>,
    /// 本轮标记到的句柄数（统计）
    marked: u64,
    /// 只标记不扫槽（`VMLESS_GC_NOSCAN=1`，量时间构成用）
    noscan: bool,
    /// 校验/统计：精确帧 / 保守帧 / 位图失配帧
    verify: bool,
    f_precise: u64,
    f_conservative: u64,
    f_mismatch: u64,
}

impl<'a> Marker<'a> {
    fn new(sh: &'a Shared, epoch: u32) -> Marker<'a> {
        Marker {
            sh,
            gathering: false,
            epoch,
            work: Vec::with_capacity(4096),
            marked: 0,
            verify: std::env::var("VMLESS_GC_VERIFY").map(|v| v != "0").unwrap_or(false),
            noscan: std::env::var("VMLESS_GC_NOSCAN").map(|v| v != "0").unwrap_or(false),
            f_precise: 0,
            f_conservative: 0,
            f_mismatch: 0,
        }
    }

    /// 一个候选句柄：还在页表里就标记；本轮第一次标到就把它的槽排进工作栈
    #[inline]
    fn mark(&mut self, h: i32) {
        if h == 0 || h >= STR_BASE {
            return; // null / 字符串句柄（字符串堆不回收）
        }
        if self.gathering {
            self.work.push(h);
            return;
        }
        let heap = &self.sh.heap;
        if !heap.mark(h, self.epoch) {
            // 要么已经标过，要么句柄早就失效（已回收）
            return;
        }
        self.marked += 1;
        let Some(o) = heap.hdr(h) else { return };
        let n = o.len();
        for i in 0..n {
            if o.refs.is_ref(i) {
                if let Some(Slot::I(v)) = o.get(i) {
                    if v != 0 {
                        self.work.push(v);
                    }
                }
            }
        }
    }

    /// 句柄 → 对象头裸指针（只为让一批解析并发起来；null = 句柄已失效/不是引用）
    #[inline]
    fn resolve(&self, h: i32) -> *const super::shared::ObjHdr {
        if h == 0 || h >= STR_BASE {
            return std::ptr::null();
        }
        match self.sh.heap.hdr(h) {
            Some(o) => o as *const _,
            None => std::ptr::null(),
        }
    }

    /// 已经解析过对象头之后的标记（省掉一次页表查找）
    #[inline]
    fn mark_resolved(&mut self, h: i32, o: *const super::shared::ObjHdr) {
        if o.is_null() {
            return;
        }
        let heap = &self.sh.heap;
        if !heap.mark_ptr(o, self.epoch) {
            return;
        }
        self.marked += 1;
        let o = unsafe { &*o };
        if self.noscan {
            return; // 只做「标记」不扫槽（量时间构成用，结果不完整）
        }
        let n = o.len();
        for i in 0..n {
            if o.refs.is_ref(i) {
                if let Some(Slot::I(v)) = o.get(i) {
                    if v != 0 {
                        self.work.push(v);
                    }
                }
            }
        }
        let _ = h;
    }

    fn drain(&mut self) {
        while let Some(h) = self.work.pop() {
            self.mark(h);
        }
    }
}

/// 保守扫描一段内存：把每个 4 字节字都当潜在句柄试一次。
///
/// 这是「解释器/native/反射代码在 Rust 局部变量里临时拿着句柄」的兜底 —— 那些值
/// 不在任何帧里，页表验证保证假的候选只会多留一个对象（活着），不会误杀。
fn scan_stack_range(m: &mut Marker, lo: usize, hi: usize) {
    let lo = (lo + 3) & !3; // `read_volatile` 要求 4 字节对齐
    if lo >= hi || hi - lo > (64 << 20) {
        return;
    }
    let words = (hi - lo) / 4;
    for i in 0..words {
        let addr = lo + i * 4;
        // SAFETY: 这段是**本线程或已停住线程**的栈内存；读到的是脏值也没关系 ——
        // 后面每一个候选都要过 `Heap::mark` 的页表验证
        let v = unsafe { (addr as *const i32).read_volatile() };
        if v > 0 || v < 0 {
            m.mark(v);
        }
    }
}

/// 扫描一个线程的全部根
fn scan_thread(m: &mut Marker, r: &ThreadRoots) {
    // 0) 线程持有的监视器句柄（thin 锁不膨胀，句柄只在 `Vm::held` 里）
    unsafe {
        let vm = *(*r).vm.get();
        if !vm.is_null() {
            for &h in (*vm).held.iter() {
                m.mark(h);
            }
        }
    }
    // 1) 帧链（精确位图，对不上就保守）
    unsafe {
        let frames = &*(*r).frames.get();
        for &f in frames.iter() {
            if f.is_null() {
                continue;
            }
            let fr = &*f;
            scan_frame(m, fr);
        }
    }
    // 2) 临时根
    unsafe {
        for &h in (*(*r).temps.get()).iter() {
            m.mark(h);
        }
    }
    // 3) Rust 栈（保守）：从最近的栈指针扫到栈底
    let sp = r.sp.load(Ordering::Acquire);
    if sp != 0 {
        scan_stack_range(m, sp, r.stack_base);
    }
}

fn scan_frame(m: &mut Marker, fr: &Frame) {
    let shape = fr.refs.as_ref().and_then(|r| r.shape_at(fr.pc));
    if m.verify {
        match shape {
            Some(sh) if !sh.is_all() && sh.depth() >= fr.stack.len() => m.f_precise += 1,
            Some(sh) if !sh.is_all() => {
                m.f_mismatch += 1;
                if m.verify {
                    eprintln!(
                        "[gc] 位图失配：pc={} 位图栈深={} 实际栈深={}",
                        fr.pc,
                        sh.depth(),
                        fr.stack.len()
                    );
                }
            }
            _ => m.f_conservative += 1,
        }
    }
    let stk = &fr.stack;
    match shape {
        // 位图与当前栈深对不上：说明我们算错了/状态不是指令边界 —— 保守扫这一帧
        // 实际栈**比位图短**是合法的：停在调用点上时参数已经被弹掉了，剩下的正好是
        // 位图栈的前缀 —— 前缀上每一位的含义没变，所以仍然可以精确扫。
        // 反过来（实际比位图长）说明我们对不上这条指令的语义，只能保守。
        Some(sh) if sh.is_all() || sh.depth() < stk.len() => {
            conservative_frame(m, fr);
        }
        Some(sh) => {
            for (i, v) in fr.locals.iter().enumerate() {
                if sh.local_is_ref(i) {
                    if let Slot::I(h) = *v {
                        m.mark(h);
                    }
                }
            }
            for (i, v) in stk.iter().enumerate() {
                if sh.stack_is_ref(i) {
                    if let Slot::I(h) = *v {
                        m.mark(h);
                    }
                }
            }
        }
        None => conservative_frame(m, fr),
    }
}

fn conservative_frame(m: &mut Marker, fr: &Frame) {
    for v in fr.locals.iter().chain(fr.stack.iter()) {
        if let Slot::I(h) = *v {
            m.mark(h);
        }
    }
    // native 帧：locals 里只有参数，位图按描述符算好了
    if let Some(bits) = fr.nrefs.as_ref() {
        for (i, v) in fr.locals.iter().enumerate() {
            if bits.get(i / 64).map(|w| (w >> (i % 64)) & 1 != 0).unwrap_or(false) {
                if let Slot::I(h) = *v {
                    m.mark(h);
                }
            }
        }
    }
}

// ---------------------------------------------------------------- 采集共享根

/// 共享冷表里的候选句柄。表都很小（静态字段 / 类对象 / 字符串常量 / indy 状态…），
/// 一律「表里出现的 i32 都当候选」，靠页表验证兜底。
fn scan_shared_roots(m: &mut Marker) {
    let sh = m.sh;
    {
        let g = sh.statics.lock().unwrap_or_else(|e| e.into_inner());
        for v in g.values() {
            if let Slot::I(h) = *v {
                m.mark(h);
            }
        }
    }
    {
        let g = sh.class_objs.lock().unwrap_or_else(|e| e.into_inner());
        for h in g.keys() {
            m.mark(*h);
        }
    }
    {
        let g = sh.class_obj_cache.lock().unwrap_or_else(|e| e.into_inner());
        for h in g.values() {
            m.mark(*h);
        }
    }
    {
        let g = sh.exc_msgs.lock().unwrap_or_else(|e| e.into_inner());
        for (k, v) in g.iter() {
            m.mark(*k);
            m.mark(*v);
        }
    }
    {
        let g = sh.ann_objs.lock().unwrap_or_else(|e| e.into_inner());
        for h in g.keys() {
            m.mark(*h);
        }
    }
    {
        let heap = &sh.heap;
        let g = heap.str_handles.lock().unwrap_or_else(|e| e.into_inner());
        for h in g.values() {
            m.mark(*h);
        }
    }
    {
        let heap = &sh.heap;
        let g = heap.str_fields.lock().unwrap_or_else(|e| e.into_inner());
        for (h, vals) in g.iter() {
            m.mark(*h);
            for v in vals {
                if let Slot::I(x) = *v {
                    m.mark(x);
                }
            }
        }
    }
    {
        let heap = &sh.heap;
        let g = heap.sb_bufs.lock().unwrap_or_else(|e| e.into_inner());
        for h in g.keys() {
            m.mark(*h);
        }
    }
    {
        let heap = &sh.heap;
        let g = heap.wrapper_cache.lock().unwrap_or_else(|e| e.into_inner());
        for h in g.values() {
            m.mark(*h);
        }
    }
    {
        // 监视器表的键是对象句柄；膨胀中的对象不能被回收
        for (h, _, _, _, _) in sh.monitors.snapshot() {
            m.mark(h);
        }
    }
    {
        let g = sh.threads.lock().unwrap_or_else(|e| e.into_inner());
        for r in g.values() {
            m.mark(r.java);
            m.mark(r.waiting_on);
        }
    }
    {
        let g = sh.indy.lock().unwrap_or_else(|e| e.into_inner());
        g.each_handle(|h| m.mark(h));
    }
}


/// 并行标记：N 个线程从同一个工作队列领活，直到队列空且没人干活。
///
/// 标记是**幂等**的（同一对象只扫一次，靠 epoch 的原子读写；重复扫描只是白干，无害），
/// 所以不需要给堆分区，抢一个全局队列最简单也最不容易错。
fn mark_all(sh: &Arc<Shared>, epoch: u32, seeds: &mut Vec<i32>) -> u64 {
    let threads = gc_threads();
    // 活对象很少时（校验/小样例）单线程就地标完：起 7 个线程比标几百个对象还贵
    if threads <= 1 || seeds.is_empty() || sh.heap.live() < 16_384 {
        let mut m = Marker::new(sh, epoch);
        m.work.append(seeds);
        m.drain();
        return m.marked;
    }
    let shared = Arc::new(Work {
        m: Mutex::new(WorkQ { q: std::mem::take(seeds), busy: 0 }),
        cv: Condvar::new(),
    });
    let mut handles = Vec::with_capacity(threads - 1);
    for _ in 1..threads {
        let w = Arc::clone(&shared);
        let s2 = Arc::clone(sh);
        match std::thread::Builder::new().name("gc-mark".into()).spawn(move || {
            let mut m = Marker::new(&s2, epoch);
            mark_worker(&w, &mut m);
            m.marked
        }) {
            Ok(h) => handles.push(h),
            Err(_) => break,
        }
    }
    // 发起者自己也干活（否则白等一个核）
    let mut m = Marker::new(sh, epoch);
    mark_worker(&shared, &mut m);
    let mut total = m.marked;
    for h in handles {
        if let Ok(v) = h.join() {
            total += v;
        }
    }
    total
}

const MARK_BATCH: usize = 1024;
const MARK_FLUSH: usize = 4096;
const MARK_CHUNK: usize = 32;

struct WorkQ {
    q: Vec<i32>,
    /// 正在干活的线程数（队列空且为 0 → 全部结束）
    busy: usize,
}

struct Work {
    m: Mutex<WorkQ>,
    cv: Condvar,
}

fn mark_worker(w: &Work, m: &mut Marker) {
    loop {
        let batch = {
            let mut g = w.m.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if !g.q.is_empty() {
                    let cut = g.q.len() - g.q.len().min(MARK_BATCH);
                    let b: Vec<i32> = g.q.split_off(cut);
                    g.busy += 1;
                    break b;
                }
                if g.busy == 0 {
                    drop(g);
                    w.cv.notify_all();
                    return;
                }
                let (ng, _) = w
                    .cv
                    .wait_timeout(g, std::time::Duration::from_millis(1))
                    .unwrap_or_else(|e| e.into_inner());
                g = ng;
            }
        };
        // 分块：先把 32 个句柄都解析成对象头（32 条访存链同时在飞），再逐个扫槽。
        // 内层一个分配都不做 —— 并行标记里每次 malloc 都会在全局分配器上打架。
        let mut i = 0usize;
        while i < batch.len() {
            let end = (i + MARK_CHUNK).min(batch.len());
            let mut ptrs: [*const super::shared::ObjHdr; MARK_CHUNK] =
                [std::ptr::null(); MARK_CHUNK];
            for k in 0..(end - i) {
                ptrs[k] = m.resolve(batch[i + k]);
            }
            // 对象头已经到手（resolve 顺带把第一行拉进缓存），再把"身体"（槽位数组）
            // 预取进来 —— 标记每个对象都要多一次跳跃，不预取就是一次实打实的 miss。
            for k in 0..(end - i) {
                let o = ptrs[k];
                if !o.is_null() && unsafe { (*o).has_refs() } {
                    #[cfg(target_arch = "x86_64")]
                    unsafe {
                        use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
                        _mm_prefetch((*o).cells_ptr() as *const i8, _MM_HINT_T0);
                    }
                }
            }
            for k in 0..(end - i) {
                m.mark_resolved(batch[i + k], ptrs[k]);
            }
            i = end;
            if m.work.len() >= MARK_FLUSH {
                let mut g = w.m.lock().unwrap_or_else(|e| e.into_inner());
                g.q.append(&mut m.work);
                drop(g);
                w.cv.notify_one(); // 新活来了：叫一个就够
            }
        }
        let mut g = w.m.lock().unwrap_or_else(|e| e.into_inner());
        g.q.append(&mut m.work);
        g.busy -= 1;
        let wake = !g.q.is_empty() || g.busy == 0;
        drop(g);
        if wake {
            w.cv.notify_all(); // 有活 / 收工：让等的人自己判断
        }
    }
}

/// 标记线程数（`VMLESS_GC_THREADS` 可覆盖）
fn gc_threads() -> usize {
    if let Some(v) = std::env::var("VMLESS_GC_THREADS").ok().and_then(|v| v.parse::<usize>().ok()) {
        return v.max(1);
    }
    std::thread::available_parallelism().map(|n| n.get().min(8)).unwrap_or(1).max(1)
}

/// 进程启动时刻（退避计时用；懒初始化，只在真要发起回收时才读时钟）
fn now_ms() -> u64 {
    static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    T0.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

// ---------------------------------------------------------------- 回收

/// 一轮回收的统计
pub(crate) struct GcStats {
    pub pause_us: u64,
    pub freed: u32,
    pub freed_bytes: u64,
    pub live: u64,
    pub all_parked: bool,
}

impl Shared {
    /// 现在轮到 GC 了吗（分配量超过阈值）
    #[inline]
    pub(crate) fn gc_should_run(&self) -> bool {
        if self.gc_off {
            return false; // 量「改前」用：整个关掉回收，别的行为一模一样
        }
        let n = self.heap.allocs_since_gc.load(Ordering::Relaxed);
        if self.gc_stress {
            return n > 0;
        }
        n >= self.heap.gc_threshold()
    }

}

impl Vm {
    /// 解释器主循环的 GC 检查点。
    ///
    /// **只在这里回收**：`pc` 是精确的指令边界、`frame` 的栈形状与位图一致。分配函数
    /// 本身只累加计数，绝不自己触发回收（那样 `pc` 会停在半条指令上，位图对不上）。
    #[inline]
    pub(super) fn gc_poll(&mut self, frame: &mut Frame, pc: usize) {
        // 压力模式下 gc_should_run 就是「这一小段有没有分配过」—— 没分配就不必回收
        if !self.sh.gc_should_run() {
            return;
        }
        frame.pc = pc;
        // 没登记的线程停不住别人，不能当发起者（run_main 之外的入口不会走到这里）
        if !self.sh.safepoint.is_registered() {
            return;
        }
        // 停不住世界就别反复试：上一次失败后 50ms 内不再发起（否则每个线程都在 4ms 上堵着）
        if now_ms() < self.sh.gc_backoff.load(Ordering::Relaxed) {
            return;
        }
        // 同时只允许一个线程当发起者；清扫还没结束也不开始新的一轮
        let sh = Arc::clone(&self.sh);
        let Ok(_initiator) = sh.gc_lock.try_lock() else { return };
        if self.sh.sweeping.load(Ordering::Acquire) {
            return;
        }
        // 世界没停全就不回收（宁可不回收）
        if !self.sh.safepoint.sync(4) {
            self.sh.gc_backoff.store(now_ms() + 50, Ordering::Relaxed);
            return;
        }
        let st = self.collect();
        if self.sh.gc_stats {
            eprintln!(
                "[gc] pause={:.2}ms live={} 位图:精确/总={}/{}{}",
                st.pause_us as f64 / 1000.0,
                st.live,
                self.sh.refmap_total.load(Ordering::Relaxed)
                    - self.sh.refmap_cons.load(Ordering::Relaxed),
                self.sh.refmap_total.load(Ordering::Relaxed),
                if st.all_parked { "" } else { " (未停全：跳过)" }
            );
        }
    }

    /// 一轮完整回收（调用方必须已经把世界停住）
    fn collect(&mut self) -> GcStats {
        let t0 = std::time::Instant::now();
        let sh = Arc::clone(&self.sh);
        let epoch = sh.heap.gc_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        let mut m = Marker::new(&sh, epoch);
        // 第一阶段只**收候选**（不追对象图），第二阶段再并行标记
        m.gathering = true;

        // 根：所有登记线程的帧 + 临时根 + 保守栈
        let anchor = 0u8;
        let my_sp = &anchor as *const u8 as usize;
        MY_ROOTS.with(|r| {
            let p = r.get();
            if !p.is_null() {
                unsafe { (*p).sp.store(my_sp, Ordering::Release) };
            }
        });
        let roots: Vec<*const ThreadRoots> =
            ALL_ROOTS.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for p in roots {
            if p.is_null() {
                continue;
            }
            unsafe { scan_thread(&mut m, &*p) };
        }
        scan_shared_roots(&mut m);
        m.gathering = false;
        // 到这里 m.work 里是全部根候选句柄；下面并行标记
        let t_roots = t0.elapsed();
        let nroots = m.work.len();
        let mut seeds = std::mem::take(&mut m.work);
        let marked = mark_all(&sh, epoch, &mut seeds);
        if sh.gc_stats {
            eprintln!(
                "[gc]   根 {} 个（{:.2}ms）/ 标记 {} 个（{:.2}ms）/ 线程 {}",
                nroots,
                t_roots.as_secs_f64() * 1000.0,
                marked,
                (t0.elapsed() - t_roots).as_secs_f64() * 1000.0,
                gc_threads()
            );
        }

        if m.verify {
            eprintln!(
                "[gc] 帧：精确 {} / 保守 {} / 位图失配 {}",
                m.f_precise, m.f_conservative, m.f_mismatch
            );
        }
        let pause_us = t0.elapsed().as_micros() as u64;
        let live_before = sh.heap.live();
        sh.heap.allocs_since_gc.store(0, Ordering::Release);
        // 阈值材料是「回收**后**的存活数」，清扫完才更新（见下面两处 live_after_gc）
        let _ = live_before;

        // 清扫：小堆就地做完（起线程比扫一遍还贵），大堆丢给后台线程（不占停顿）
        let (o, a) = sh.heap.counts();
        if (o as u64 + a as u64) < 131_072 {
            let (fo, fa, fb) = sh.heap.sweep(epoch);
            sh.heap.live_after_gc.store(sh.heap.live(), Ordering::Release);
            if sh.gc_stats {
                eprintln!("[gc] sweep freed={} bytes={}KB live={}", fo + fa, fb / 1024, sh.heap.live());
            }
            return GcStats {
                pause_us,
                freed: fo + fa,
                freed_bytes: fb,
                live: sh.heap.live(),
                all_parked: true,
            };
        }
        sh.sweeping.store(true, Ordering::Release);
        let sh2 = Arc::clone(&sh);
        let e = epoch;
        let stats = sh.gc_stats;
        let spawn = std::thread::Builder::new().name("gc-sweep".into()).spawn(move || {
            let (o, a, b) = sh2.heap.sweep(e);
            sh2.heap.live_after_gc.store(sh2.heap.live(), Ordering::Release);
            if stats {
                eprintln!(
                    "[gc] sweep freed={} (obj {} arr {}) bytes={}MB live={}",
                    o + a,
                    o,
                    a,
                    b / (1024 * 1024),
                    sh2.heap.live()
                );
            }
            sh2.sweeping.store(false, Ordering::Release);
        });
        if spawn.is_err() {
            // 起不了线程就在当前线程做完（仍然正确，只是停顿长一点）
            let (_o, _a, _b) = sh.heap.sweep(epoch);
            sh.sweeping.store(false, Ordering::Release);
        }
        GcStats {
            pause_us,
            freed: 0,
            freed_bytes: 0,
            live: sh.heap.live(),
            all_parked: true,
        }
    }
}


// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CpEntry;

    fn cf_with(cp: Vec<CpEntry>) -> ClassFile {
        let mut constant_pool: Vec<Option<CpEntry>> = vec![None];
        for e in cp {
            constant_pool.push(Some(e));
        }
        ClassFile {
            minor_version: 0,
            major_version: 52,
            constant_pool,
            access_flags: 0,
            this_class: 0,
            super_class: 0,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            attributes: vec![],
        }
    }

    fn call(
        body: &[u8],
        desc: &str,
        is_static: bool,
        locals: usize,
        exc: &[(u16, u16, u16, u16)],
    ) -> MethodRefs {
        analyze(&cf_with(vec![]), body, desc, locals, is_static, exc)
    }

    /// 局部槽：`astore` 之后那个槽是引用，`istore` 之后不是；宽值占两槽、第二槽不是引用
    #[test]
    fn bitmap_locals_ref_and_wide() {
        // (Ljava/lang/Object;I)V 实例方法 → 槽0 = this(引用), 槽1 = int
        let body: Vec<u8> = vec![
            0x2a, // 0  aload_0        栈: [R]
            0x4c, // 1  astore_1       local1 = 引用
            0x1b, // 2  iload_1
            0x3d, // 3  istore_2       local2 = 非引用
            0x16, 0x04, // 4  lload 4    栈: [W]
            0x37, 0x06, // 6  lstore 6    local6/7 = 非引用
            0xb1, // 8  return
        ];
        let m = call(&body, "(I)V", false, 8, &[]);
        assert!(!m.is_conservative(), "简单方法不该退化");
        let s0 = m.shape_at(0).unwrap();
        assert!(s0.local_is_ref(0), "this 必须是引用");
        assert!(!s0.local_is_ref(1), "int 参数不是引用");
        assert_eq!(s0.depth(), 0);
        // pc=2：astore_1 之后 local1 变成引用，栈空
        let s2 = m.shape_at(2).unwrap();
        assert!(s2.local_is_ref(1));
        assert_eq!(s2.depth(), 0);
        // pc=3：iload_1 之后栈上 1 个值，且**不是**引用
        let s3 = m.shape_at(3).unwrap();
        assert_eq!(s3.depth(), 1);
        assert!(!s3.stack_is_ref(0), "iload 压的是 int，不能当引用");
        // pc=4：istore_2 之后 local2 不是引用
        assert!(!m.shape_at(4).unwrap().local_is_ref(2));
        // pc=6：lload 4 之后栈上是一个宽值（1 个条目，非引用）
        let s6 = m.shape_at(6).unwrap();
        assert_eq!(s6.depth(), 1);
        assert!(!s6.stack_is_ref(0));
        // pc=8：lstore 6 之后 6/7 都不是引用
        let s8 = m.shape_at(8).unwrap();
        assert!(!s8.local_is_ref(6));
        assert!(!s8.local_is_ref(7));
    }

    /// `getfield` 的引用字段 → 压引用；int 字段 → 压非引用
    #[test]
    fn bitmap_getfield_ref_and_prim() {
        // cp: 1=Utf8 "o", 2=Utf8 "Ljava/lang/Object;", 3=Utf8 "n", 4=Utf8 "I",
        //     5=NameAndType(1,2), 6=NameAndType(3,4), 7=FieldRef(0,5), 8=FieldRef(0,6)
        let cf = cf_with(vec![
            CpEntry::Utf8(b"o".to_vec()),
            CpEntry::Utf8(b"Ljava/lang/Object;".to_vec()),
            CpEntry::Utf8(b"n".to_vec()),
            CpEntry::Utf8(b"I".to_vec()),
            CpEntry::NameAndType(1, 2),
            CpEntry::NameAndType(3, 4),
            CpEntry::FieldRef(0, 5),
            CpEntry::FieldRef(0, 6),
        ]);
        let body: Vec<u8> = vec![
            0x2a, // 0  aload_0
            0xb4, 0x00, 0x07, // 1  getfield #7（引用字段）
            0x4b, // 4  astore_0 → local0 = 引用
            0x2b, // 5  aload_1
            0xb4, 0x00, 0x08, // 6  getfield #8（int 字段）
            0x3c, // 9  istore_1
            0xb1, // 10 return
        ];
        let m = analyze(&cf, &body, "(Ljava/lang/Object;I)V", 4, false, &[]);
        assert!(!m.is_conservative());
        let s = m.shape_at(4).unwrap();
        assert_eq!(s.depth(), 1);
        assert!(s.stack_is_ref(0), "引用字段读出来必须是引用");
        let s = m.shape_at(9).unwrap();
        assert_eq!(s.depth(), 1);
        assert!(!s.stack_is_ref(0), "int 字段读出来不是引用");
    }

    /// `invokevirtual` 按描述符弹参数、按返回类型压；`ldc` 的字符串常量是引用
    #[test]
    fn bitmap_invoke_and_ldc() {
        // cp: 1=Utf8 "m", 2=Utf8 "(I)Ljava/lang/String;", 3=Utf8 "hello",
        //     4=NameAndType(1,2), 5=MethodRef(0,4), 6=String(3)
        let cf = cf_with(vec![
            CpEntry::Utf8(b"m".to_vec()),
            CpEntry::Utf8(b"(I)Ljava/lang/String;".to_vec()),
            CpEntry::Utf8(b"hello".to_vec()),
            CpEntry::NameAndType(1, 2),
            CpEntry::MethodRef(0, 4),
            CpEntry::String(3),
        ]);
        let body: Vec<u8> = vec![
            0x2a, // 0 aload_0
            0x04, // 1 iconst_1
            0xb6, 0x00, 0x05, // 2 invokevirtual #5 → 弹 2（接收者+1 参）、压 1 个引用
            0x4c, // 5 astore_1
            0x12, 0x06, // 6 ldc #6（字符串常量 → 引用）
            0x4d, // 8 astore_2
            0xb1, // 9 return
        ];
        let m = analyze(&cf, &body, "(Ljava/lang/Object;)V", 4, false, &[]);
        assert!(!m.is_conservative());
        let s = m.shape_at(5).unwrap();
        assert_eq!(s.depth(), 1);
        assert!(s.stack_is_ref(0), "返回 String 的调用压出来必须是引用");
        assert!(m.shape_at(6).unwrap().local_is_ref(1));
        assert!(m.shape_at(8).unwrap().stack_is_ref(0), "ldc 字符串常量是引用");
        assert!(m.shape_at(9).unwrap().local_is_ref(2));
    }

    /// 分支合并：一条路径上 local2 是引用、另一条上不是 → 合并后**仍然算引用**（并）
    #[test]
    fn bitmap_branch_merge_is_union() {
        let body: Vec<u8> = vec![
            0x1a, // 0  iload_0
            0x99, 0x00, 0x08, // 1  ifeq → 9
            0x2a, // 4  aload_1
            0x4c, // 5  astore_2       引用路径
            0xa7, 0x00, 0x06, // 6  goto → 12
            0x03, // 9  iconst_0
            0x3d, // 10 istore_2       非引用路径
            0x00, // 11 nop
            0xb1, // 12 return
        ];
        let m = analyze(&cf_with(vec![]), &body, "(ILjava/lang/Object;)V", 4, false, &[]);
        assert!(!m.is_conservative());
        let s = m.shape_at(12).unwrap();
        assert!(s.local_is_ref(2), "合并后 local2 必须仍然算引用（并 = 不漏根）");
    }

    /// 异常 handler：入口栈上只有一个异常对象引用，局部变量取 try 块里的并
    #[test]
    fn bitmap_handler_entry() {
        // cp: 1=Utf8 "m", 2=Utf8 "()V", 3=NameAndType(1,2), 4=MethodRef(0,3)
        let cf = cf_with(vec![
            CpEntry::Utf8(b"m".to_vec()),
            CpEntry::Utf8(b"()V".to_vec()),
            CpEntry::NameAndType(1, 2),
            CpEntry::MethodRef(0, 3),
        ]);
        let body: Vec<u8> = vec![
            0x2a, // 0 aload_0
            0x4c, // 1 astore_1      local1 = 引用
            0x2b, // 2 aload_1
            0xb6, 0x00, 0x04, // 3 invokevirtual #4（可能抛）
            0xb1, // 6 return
            0x57, // 7 handler: pop（异常对象）
            0xb1, // 8 return
        ];
        let exc = vec![(2u16, 6u16, 7u16, 0u16)];
        let m = analyze(&cf, &body, "(Ljava/lang/Object;)V", 4, false, &exc);
        assert!(!m.is_conservative(), "handler 不该让整个方法退化");
        let h = m.shape_at(7).unwrap();
        assert_eq!(h.depth(), 1, "handler 入口栈上只有那个异常对象");
        assert!(h.stack_is_ref(0), "handler 入口的那个值必须是引用");
        assert!(
            h.local_is_ref(1),
            "try 块里写过的引用局部变量在 handler 入口仍然算引用"
        );
    }

    /// 不认识的指令（wide）→ 整个方法退化成保守位图（多标安全，绝不漏标）
    #[test]
    fn bitmap_conservative_fallback() {
        let body: Vec<u8> = vec![0xc4, 0x15, 0x01, 0x00, 0xb1];
        let m = call(&body, "(I)V", true, 4, &[]);
        assert!(m.is_conservative());
        let s = m.shape_at(0).unwrap();
        assert!(s.is_all(), "保守形状要把所有槽都当引用");
        assert!(s.local_is_ref(0) && s.local_is_ref(3));
    }

    /// 端到端：用真编译器产出的字节码检查常见形态都不退化（分支/循环/拼接/数组/宽值）
    #[test]
    fn bitmap_real_bytecode_is_precise() {
        let src = r#"
            class A { int v; A next; }
            class T {
                A head;
                int n;
                void put(A a) { a.next = head; head = a; n = n + 1; }
                int len() { int k = 0; A p = head; while (p != null) { k = k + p.v; p = p.next; } return k; }
                String txt(int i) { String s = i > 3 ? "big" : "small"; return s + i; }
                int[] arr(int n) { int[] a = new int[n]; for (int i = 0; i < n; i++) a[i] = i * i; return a; }
                long wide(long a, double b) { long c = a + (long) b; return c * 2; }
            }
        "#;
        let cu = crate::parser::parse_compilation_unit(src).expect("解析");
        let classes = crate::compiler::compile(&cu).expect("编译");
        let mut total = 0usize;
        let mut conservative = 0usize;
        for (_name, bytes) in classes.iter() {
            let cf = crate::parse(bytes).expect("读回 class");
            for m in &cf.methods {
                let desc =
                    String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default()).into_owned();
                let is_static = m.access_flags & 0x0008 != 0;
                let Ok((max_locals, body, exc)) = crate::vm::exec::code_of(&cf, m) else { continue };
                let refs = analyze(&cf, body, &desc, max_locals, is_static, &exc);
                total += 1;
                if refs.is_conservative() {
                    let nm = String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default()).into_owned();
                    eprintln!("保守: {nm}{desc} (body {} 字节)", body.len());
                    conservative += 1;
                    continue;
                }
                let mut pc = 0usize;
                while pc < body.len() {
                    let s = refs.shape_at(pc).unwrap_or_else(|| panic!("pc={pc} 没有形状"));
                    assert!(!s.is_all(), "pc={pc} 退化成保守形状");
                    pc += insn_len(body, pc).expect("长度");
                }
            }
        }
        assert!(total > 5, "至少要分析到几个方法，实际 {total}");
        assert_eq!(conservative, 0, "这些形态（含分支/循环/拼接/数组/宽值）不该退化");
    }
}
