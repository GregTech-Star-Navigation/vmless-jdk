//! 解释器侧的 `java/lang/StringBuilder`：字符串拼接的内建实现。
//!
//! 编译器把 `"a" + b` 编成标准调用链（`new java/lang/StringBuilder` +
//! `append(…)` + `toString()`，见 `codegen::expr::emit_concat`），但没有它的
//! 字节码 —— 这里按对象句柄维护一块文本缓冲。
//!
//! 语义要与 javac/JVM 一致：`null` 拼成 `"null"`，`double` 用 Java 的格式化，
//! `append` 返回 this（编译器在调用点 `pop` 掉它）。

use super::slot::Slot;
use super::Vm;
use super::VmError;

impl Vm {
    /// `StringBuilder.append(X)…`：把文本接到句柄对应的缓冲上，返回 this（编译器会 pop）
    pub(super) fn sb_append(&mut self, handle: i32, desc: &str, arg: Slot) -> Result<Slot, VmError> {
        let text = match desc {
            "(J)Ljava/lang/StringBuilder;" => arg.as_i64().to_string(),
            "(D)Ljava/lang/StringBuilder;" => crate::java_double_to_string(arg.as_f64()),
            "(F)Ljava/lang/StringBuilder;" => crate::java_float_to_string(arg.as_f32()),
            "(Z)Ljava/lang/StringBuilder;" => (if arg.as_i32() != 0 { "true" } else { "false" }).to_string(),
            "(C)Ljava/lang/StringBuilder;" => {
                char::from_u32(arg.as_i32() as u32).unwrap_or(0 as char).to_string()
            }
            // 引用：null → "null"，String → 内容，包装类 → 值的文本，其余 → `Klass@hash`
            d if d.starts_with("(L") => self.obj_to_string(arg.as_i32()),
            _ => arg.as_i32().to_string(),
        };
        let buf = self.sb_bufs.entry(handle).or_default();
        buf.push_str(&text);
        Ok(Slot::I(handle))
    }

    /// `StringBuilder.toString()`：缓冲 → 字符串堆里的一个新句柄
    pub(super) fn sb_finish(&mut self, handle: i32) -> i32 {
        let text = self.sb_bufs.remove(&handle).unwrap_or_default();
        self.strings.push(text);
        super::heap::STR_BASE + self.strings.len() as i32 - 1
    }
}
