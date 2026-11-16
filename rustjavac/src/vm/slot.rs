//! 解释器的值模型：int / long / float / double 共用一种表示，用完再按字节码语义取用。

/// 解释器里的一个值。int/long/float/double 共用一种表示，用完再按字节码的语义取用。
///
/// **float 必须与 double 分开**（`Fl` / `F`）：两者在 JVM 里的槽宽不同（float 占 1 槽、
/// double 占 2 槽），`pop2`/`dup2` 与局部变量落位都要按槽宽区分；混成一个 `F(f64)`
/// 会让「float 存进第 n 槽」顺手把第 n+1 槽的变量清成 0，也会让 `pop2` 少弹一个值。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Slot {
    I(i32),
    L(i64),
    /// double
    F(f64),
    /// float（值按 f32 收，存成 f32 以便还原时保持 f32 语义）
    Fl(f32),
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
            Slot::Fl(v) => v as i32,
        }
    }
    pub(super) fn as_i64(self) -> i64 {
        match self {
            Slot::I(v) => v as i64,
            Slot::L(v) => v,
            Slot::F(v) => v as i64,
            Slot::Fl(v) => v as i64,
        }
    }
    pub(super) fn as_f64(self) -> f64 {
        match self {
            Slot::I(v) => v as f64,
            Slot::L(v) => v as f64,
            Slot::F(v) => v,
            Slot::Fl(v) => v as f64,
        }
    }
    /// f32 语义：fadd/fsub/…/fcmpl 一律按这个取，收回头再按 f32 存
    pub(super) fn as_f32(self) -> f32 {
        match self {
            Slot::I(v) => v as f32,
            Slot::L(v) => v as f32,
            Slot::F(v) => v as f32,
            Slot::Fl(v) => v,
        }
    }
}
