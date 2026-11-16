//! 解释器的值模型：int / long / double 共用一种表示，用完再按字节码语义取用。

/// 解释器里的一个值。int/long/double 共用一种表示，用完再按字节码的语义取用。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Slot {
    I(i32),
    L(i64),
    F(f64),
}

impl Slot {
    pub(super) fn is_wide(self) -> bool {
        matches!(self, Slot::L(_) | Slot::F(_))
    }
    pub(super) fn as_i32(self) -> i32 {
        match self {
            Slot::I(v) => v,
            Slot::L(v) => v as i32,
            Slot::F(v) => v as i32,
        }
    }
    pub(super) fn as_i64(self) -> i64 {
        match self {
            Slot::I(v) => v as i64,
            Slot::L(v) => v,
            Slot::F(v) => v as i64,
        }
    }
    pub(super) fn as_f64(self) -> f64 {
        match self {
            Slot::I(v) => v as f64,
            Slot::L(v) => v as f64,
            Slot::F(v) => v,
        }
    }
}
