//! 栈帧：局部槽（按 JVM 槽编号，宽值占两槽）与操作数栈。

use std::sync::Arc;

use super::gc::MethodRefs;
use super::slot::Slot;
use super::VmError;

/// 内联槽数：`max_locals` / 操作数栈在 8 以内时**一次堆分配都不需要**。
///
/// 取 8 的依据：JDK 热路径上的小方法（`Integer.toString` 的 `getChars`、`StringBuilder.append`
/// 的那些重载、`String.<init>`）`max_locals` 与 `max_stack` 都在 5~7；用户代码里的 getter /
/// 算术方法更小。超过 8 的按老样子分配到堆上（一次，容量按需增长）。
const INLINE: usize = 8;

/// 一列槽位：定长内联区 + 溢出到堆。
///
/// 为什么要它：`Frame` 每次调用要分配 locals + stack 两块、返回时再 `free` 两块 ——
/// rdtsc 实测这段是每次调用固定开销里最贵的一格（`Frame::new` 135 ns + 析构回收 ~120 ns）。
/// 内联之后小方法这两块都在帧自己身上，**零分配零释放**。
///
/// 为什么不是「线程本地缓冲池」：池化会让不同调用的帧复用同一块内存，GC 的保守扫描
/// 看到的根集随复用历史变化（实测 `Str1` 20 次里偶发 1 次丢根）。内联区跟着 Rust 栈帧走，
/// 每帧构造时自己清零，根集与「每帧新分配」逐字节一致。
///
/// `ptr` 不能存绝对地址：`Frame` 是按值返回的，内联区会跟着搬家。所以堆/内联由
/// `spilled` 区分，内联时每次现算 `inline.as_ptr()`（一次可预测分支）。
pub(super) struct Slots {
    /// true = 数据在 `heap`（容量超过内联区）
    spilled: bool,
    len: usize,
    /// 总容量（含内联部分）：`cap > INLINE` 意味着有堆块
    cap: usize,
    heap: *mut Slot,
    inline: [Slot; INLINE],
}

impl Slots {
    /// 长度 `n`、内容全 `I(0)` 的一列（局部变量表用：GC 会把整段当潜在引用扫）
    pub(super) fn zeros(n: usize) -> Slots {
        let mut s = Slots::empty();
        s.resize(n, Slot::I(0));
        s
    }

    /// 空的一列（操作数栈用）
    pub(super) fn empty() -> Slots {
        Slots {
            spilled: false,
            len: 0,
            cap: INLINE,
            heap: std::ptr::null_mut(),
            inline: [Slot::I(0); INLINE],
        }
    }

    #[inline(always)]
    fn base(&self) -> *const Slot {
        if self.spilled {
            self.heap
        } else {
            self.inline.as_ptr()
        }
    }

    #[inline(always)]
    pub(super) fn as_slice(&self) -> &[Slot] {
        // SAFETY: `base()` 指向至少 `len` 个已初始化槽；内联区随 self 存活，堆块由 Drop 释放
        unsafe { std::slice::from_raw_parts(self.base(), self.len) }
    }

    #[inline(always)]
    fn as_mut_slice(&mut self) -> &mut [Slot] {
        // SAFETY: 同上，且 &mut self 保证独占
        unsafe { std::slice::from_raw_parts_mut(self.base() as *mut Slot, self.len) }
    }

    #[inline(always)]
    pub(super) fn push(&mut self, v: Slot) {
        if self.len == self.cap {
            self.grow(1);
        }
        // SAFETY: 容量够（上面刚保证），写入后立刻把 len 提到 n+1
        unsafe {
            (self.base() as *mut Slot).add(self.len).write(v);
        }
        self.len += 1;
    }

    #[inline(always)]
    pub(super) fn pop(&mut self) -> Option<Slot> {
        if self.len == 0 {
            return None;
        }
        // SAFETY: len > 0，读已初始化区间的最后一个槽
        let v = unsafe { *(self.base().add(self.len - 1)) };
        self.len -= 1;
        Some(v)
    }

    pub(super) fn insert(&mut self, at: usize, v: Slot) {
        if self.len == self.cap {
            self.grow(1);
        }
        // SAFETY: 容量够；`at <= len` 由调用方（字节码语义）保证，越界按 len 截断
        unsafe {
            let p = self.base() as *mut Slot;
            let at = at.min(self.len);
            std::ptr::copy(p.add(at), p.add(at + 1), self.len - at);
            p.add(at).write(v);
        }
        self.len += 1;
    }

    pub(super) fn clear(&mut self) {
        self.len = 0;
    }

    pub(super) fn resize(&mut self, n: usize, v: Slot) {
        if n > self.cap {
            self.grow(n - self.len);
        }
        // SAFETY: 容量 >= n，写入后长度设为 n
        unsafe {
            let p = self.base() as *mut Slot;
            for i in self.len..n {
                p.add(i).write(v);
            }
        }
        self.len = n;
    }

    #[cold]
    fn grow(&mut self, need: usize) {
        let newcap = (self.cap * 2).max(self.len + need).max(INLINE + 1);
        let mut v: Vec<Slot> = Vec::with_capacity(newcap);
        v.extend_from_slice(self.as_slice());
        let len = v.len();
        let (p, c) = (v.as_mut_ptr(), v.capacity());
        std::mem::forget(v);
        if self.spilled {
            // SAFETY: 这块是上次 `grow` 里 forget 掉的 Vec，长度/容量一致
            unsafe {
                drop(Vec::from_raw_parts(self.heap, self.len, self.cap));
            }
        }
        self.spilled = true;
        self.heap = p;
        self.cap = c;
        self.len = len;
    }
}

impl std::fmt::Debug for Slots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_slice().fmt(f)
    }
}

impl std::ops::Deref for Slots {
    type Target = [Slot];
    #[inline(always)]
    fn deref(&self) -> &[Slot] {
        self.as_slice()
    }
}

impl std::ops::DerefMut for Slots {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut [Slot] {
        self.as_mut_slice()
    }
}

impl Drop for Slots {
    fn drop(&mut self) {
        if self.spilled {
            // SAFETY: 堆块来自 `grow` 里 forget 的 Vec，长度/容量与当时一致
            unsafe {
                drop(Vec::from_raw_parts(self.heap, self.len, self.cap));
            }
        }
    }
}

pub(super) struct Frame {
    /// 局部变量表：按**JVM 槽**编号。long/double 占两槽，第 2 槽写 `I(0)` 当无效占位
    /// （与 max_locals、与 JIT 的 locals 布局保持一致）。
    pub(super) locals: Slots,
    /// 操作数栈：一个宽值只占**一个**条目（解释器按值而不是按槽建模）。
    pub(super) stack: Slots,
    /// 参数槽位表（`crate::jit::param_slots(desc)`）：第 i 个**声明的参数**起始槽
    pub(super) arg_slots: Vec<usize>,
    /// 参数从哪个槽开始（实例方法 1 = this 占槽 0；静态方法 0）
    pub(super) arg_base: usize,
    /// 当前字节码偏移（**GC 找根用**）：`gc_tick` / 调用点会把它写成「下一条要执行的
    /// 指令偏移」，GC 拿 `refs[pc]` 得到这一点的引用位图。停在别的值时保守扫描。
    pub(super) pc: usize,
    /// 本方法的引用位图（`gc::MethodRefs`）。None = 没有（native 帧 / 未算出来）→
    /// GC 对这个帧退化成「所有槽都当潜在引用，逐个用页表验证」。
    pub(super) refs: Option<Arc<MethodRefs>>,
    /// native 帧的引用位图（按描述符算，locals 里只有参数）
    pub(super) nrefs: Option<Arc<[u64]>>,
}

impl Frame {
    pub(super) fn new(max_locals: usize) -> Self {
        Frame {
            // locals 必须**全长都是 `I(0)`**：GC 把 locals 里所有槽当潜在引用扫，
            // 残留的旧句柄会被当成根。
            locals: Slots::zeros(max_locals),
            stack: Slots::empty(),
            arg_slots: Vec::new(),
            arg_base: 0,
            pc: usize::MAX,
            refs: None,
            nrefs: None,
        }
    }
    #[inline(always)]
    pub(super) fn push(&mut self, v: Slot) {
        self.stack.push(v);
    }
    #[inline(always)]
    pub(super) fn pop(&mut self) -> Result<Slot, VmError> {
        match self.stack.pop() {
            Some(v) => Ok(v),
            None => Err(VmError { msg: "操作数栈下溢".into(), exc: None }),
        }
    }
    #[inline(always)]
    pub(super) fn pop_i(&mut self) -> Result<i32, VmError> {
        Ok(self.pop()?.as_i32())
    }
    #[inline(always)]
    pub(super) fn load(&self, slot: usize) -> Slot {
        match self.locals.get(slot) {
            Some(v) => *v,
            None => Slot::I(0),
        }
    }
    #[inline(always)]
    pub(super) fn store(&mut self, slot: usize, v: Slot) {
        if slot + 1 >= self.locals.len() {
            self.locals.resize(slot + 2, Slot::I(0));
        }
        self.locals[slot] = v;
        if v.is_wide() {
            self.locals[slot + 1] = Slot::I(0); // 宽值的第 2 槽无效
        }
    }

    /// 参数落位（规则本体，解释器与 native 共用一份）。
    /// `slots` 是 `crate::jit::param_slots(desc)` 的结果 —— 调用方给现成的，
    /// 所以缓存过槽位表的路径（native 派发的快路、解释器帧）不必每帧重算一遍。
    pub(super) fn place_args_slots(&mut self, slots: &[usize], args: &[Slot], receiver: bool) {
        let off = usize::from(receiver);
        self.arg_base = off;
        self.arg_slots = slots.to_vec();
        for (i, a) in args.iter().enumerate() {
            let slot = if i < off { 0 } else { slots.get(i - off).copied().unwrap_or(i - off) + off };
            self.store(slot, *a);
        }
    }
}

impl Frame {
    /// 第 `i` 个**声明的参数**（0 起，不含 `this`）所在的槽。
    ///
    /// native 实现一律走这里 —— 手写槽位下标必然算错（`Unsafe.getX(Object,long)`
    /// 是**实例**方法，`this` 占槽 0，`o` 在槽 1、`offset` 在槽 2，不是 0/1）。
    pub(super) fn arg_slot(&self, i: usize) -> usize {
        self.arg_base + self.arg_slots.get(i).copied().unwrap_or(i)
    }
    pub(super) fn arg(&self, i: usize) -> Slot {
        self.load(self.arg_slot(i))
    }
    pub(super) fn arg_i(&self, i: usize) -> i32 {
        self.arg(i).as_i32()
    }
    pub(super) fn arg_l(&self, i: usize) -> i64 {
        self.arg(i).as_i64()
    }
    pub(super) fn arg_f(&self, i: usize) -> f32 {
        self.arg(i).as_f32()
    }
    pub(super) fn arg_d(&self, i: usize) -> f64 {
        self.arg(i).as_f64()
    }
    /// `this`（实例方法）
    pub(super) fn this(&self) -> i32 {
        self.load(0).as_i32()
    }
}
