//! 对象 / 数组 / 字符串三套堆与引用句柄编码。

use crate::CpEntry;

use super::Vm;

/// 字符串句柄的基址：`handle >= STR_BASE` 就是字符串堆里的下标。
/// 对象句柄是正数（1 起）、数组句柄是负数、0 是 null，四者不重叠。
pub(super) const STR_BASE: i32 = 1 << 24;

/// 一个引用句柄的运行时类型
pub(super) enum RefKind {
    Null,
    Str,
    Obj(String),
    Arr(String),
}

impl<'a> Vm<'a> {
    /// 字符串常量 → 句柄（同一常量池项复用同一句柄）
    pub(super) fn string_handle(&mut self, cp_index: u16) -> i32 {
        if let Some(h) = self.string_handles.get(&cp_index) {
            return *h;
        }
        let text = self.string_at(cp_index).unwrap_or_default();
        self.strings.push(text);
        let h = STR_BASE + self.strings.len() as i32 - 1;
        self.string_handles.insert(cp_index, h);
        h
    }

    pub(super) fn string_text(&self, handle: i32) -> String {
        self.strings.get((handle - STR_BASE) as usize).cloned().unwrap_or_default()
    }

    pub(super) fn ref_kind(&self, handle: i32) -> RefKind {
        if handle == 0 {
            RefKind::Null
        } else if handle >= STR_BASE {
            RefKind::Str
        } else if handle > 0 {
            RefKind::Obj(self.object_class.get(handle as usize - 1).cloned().unwrap_or_default())
        } else {
            RefKind::Arr(self.array_class.get((-handle - 1) as usize).cloned().unwrap_or_default())
        }
    }

    pub(super) fn string_at(&self, index: u16) -> Option<String> {
        match self.class.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
            Some(CpEntry::String(u)) => {
                self.class.utf8(*u).map(|b| String::from_utf8_lossy(b).into_owned())
            }
            _ => None,
        }
    }
}
