//! 内建原语：println 的格式化。

use crate::java_double_to_string;

use super::heap::STR_BASE;
use super::slot::Slot;
use super::Vm;

impl Vm {
    /// `System.out.println(...)` 的文本：按重载（参数类型）决定怎么格式化
    pub(super) fn println_text(&self, desc: &str, v: Slot) -> String {
        match desc {
            "(J)V" => v.as_i64().to_string(),
            "(D)V" | "(F)V" => java_double_to_string(v.as_f64()),
            "(Z)V" => (if v.as_i32() != 0 { "true" } else { "false" }).to_string(),
            "(Ljava/lang/String;)V" | "(Ljava/lang/Object;)V" => {
                let h = v.as_i32();
                if h == 0 {
                    "null".to_string() // Java 打 println((String) null) 也是 "null"
                } else if h >= STR_BASE {
                    self.string_text(h)
                } else {
                    self.type_name(h) // 对象/数组：给个类型名，别静默打成空
                }
            }
            _ => v.as_i32().to_string(),
        }
    }
}
