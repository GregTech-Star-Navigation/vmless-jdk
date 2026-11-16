//! 栈帧：局部槽（按 JVM 槽编号，宽值占两槽）与操作数栈。

use super::slot::Slot;
use super::VmError;

pub(super) struct Frame {
    /// 局部变量表：按**JVM 槽**编号。long/double 占两槽，第 2 槽写 `I(0)` 当无效占位
    /// （与 max_locals、与 JIT 的 locals 布局保持一致）。
    pub(super) locals: Vec<Slot>,
    /// 操作数栈：一个宽值只占**一个**条目（解释器按值而不是按槽建模）。
    pub(super) stack: Vec<Slot>,
}

impl Frame {
    pub(super) fn new(max_locals: usize) -> Self {
        Frame { locals: vec![Slot::I(0); max_locals.max(1)], stack: Vec::new() }
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
}
