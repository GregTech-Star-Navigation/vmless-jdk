//! 栈帧：局部槽（按 JVM 槽编号，宽值占两槽）与操作数栈。

use super::slot::Slot;
use super::VmError;

pub(super) struct Frame {
    /// 局部变量表：按**JVM 槽**编号。long/double 占两槽，第 2 槽写 `I(0)` 当无效占位
    /// （与 max_locals、与 JIT 的 locals 布局保持一致）。
    pub(super) locals: Vec<Slot>,
    /// 操作数栈：一个宽值只占**一个**条目（解释器按值而不是按槽建模）。
    pub(super) stack: Vec<Slot>,
    /// 参数槽位表（`crate::jit::param_slots(desc)`）：第 i 个**声明的参数**起始槽
    pub(super) arg_slots: Vec<usize>,
    /// 参数从哪个槽开始（实例方法 1 = this 占槽 0；静态方法 0）
    pub(super) arg_base: usize,
}

impl Frame {
    pub(super) fn new(max_locals: usize) -> Self {
        Frame {
            locals: vec![Slot::I(0); max_locals.max(1)],
            stack: Vec::new(),
            arg_slots: Vec::new(),
            arg_base: 0,
        }
    }
    pub(super) fn push(&mut self, v: Slot) {
        self.stack.push(v);
    }
    pub(super) fn pop(&mut self) -> Result<Slot, VmError> {
        self.stack.pop().ok_or_else(|| VmError { msg: "操作数栈下溢".into(), exc: None })
    }
    pub(super) fn pop_i(&mut self) -> Result<i32, VmError> {
        Ok(self.pop()?.as_i32())
    }
    pub(super) fn load(&self, slot: usize) -> Slot {
        *self.locals.get(slot).unwrap_or(&Slot::I(0))
    }
    pub(super) fn store(&mut self, slot: usize, v: Slot) {
        if slot + 1 >= self.locals.len() {
            self.locals.resize(slot + 2, Slot::I(0));
        }
        self.locals[slot] = v;
        if v.is_wide() {
            self.locals[slot + 1] = Slot::I(0); // 宽值的第 2 槽无效
        }
    }

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
