//! 读写屏障。
//!
//! 分代回收的正确性全靠它：老年代对象里存一个指向年轻代的引用时，
//! minor 只看根集是不会发现这条边的 —— 必须在 store 的时候把老对象所在的卡标脏。
//!
//! 支持两种：
//! - `IncrementalUpdate`（默认）：store 时若「老 → 年轻」，标脏老对象所在卡。
//! - `Satb`：在上面基础上再记下被覆盖掉的旧值（删除屏障），保守保留本轮活对象。

use crate::cards::CardTable;
use crate::heap::Spaces;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BarrierKind {
    /// 不设屏障（**只在完全没有老年代引用年轻代的前提下正确**，测试用）
    None,
    #[default]
    IncrementalUpdate,
    Satb,
}

/// SATB 缓冲上限；满了之后新值不再入缓冲（保守策略退化为仅卡表）
pub const SATB_LIMIT: usize = 1 << 16;

/// 一次引用写入的屏障处理。返回是否标脏了卡。
#[inline]
pub fn on_store(
    kind: BarrierKind,
    spaces: &Spaces,
    cards: &mut CardTable,
    satb: &mut Vec<u32>,
    obj: u32,
    old_val: u32,
    new_val: u32,
) -> bool {
    match kind {
        BarrierKind::None => false,
        BarrierKind::IncrementalUpdate => {
            if spaces.is_old(obj) && spaces.is_young(new_val) {
                cards.mark(obj);
                true
            } else {
                false
            }
        }
        BarrierKind::Satb => {
            let mut marked = false;
            if spaces.is_old(obj) && spaces.is_young(new_val) {
                cards.mark(obj);
                marked = true;
            }
            // 删除屏障：旧值可能是本轮的唯一引用，保守地把它也当根
            if old_val != 0 && spaces.is_young(old_val) && satb.len() < SATB_LIMIT {
                satb.push(old_val);
            }
            marked
        }
    }
}
