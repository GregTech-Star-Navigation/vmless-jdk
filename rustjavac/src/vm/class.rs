//! 类元数据、继承链与类型判定。

use crate::CpEntry;

use super::err;
use super::heap::RefKind;
use super::Vm;
use super::VmError;

/// java.* 异常类的继承链（我们只加载一个 class 文件，这些层级只能写死）
pub(crate) fn builtin_super(class: &str) -> Option<&'static str> {
    match class {
        "java/lang/RuntimeException" => Some("java/lang/Exception"),
        "java/lang/Exception" => Some("java/lang/Throwable"),
        "java/lang/Error" => Some("java/lang/Throwable"),
        "java/lang/Throwable" => Some("java/lang/Object"),
        "java/lang/ArithmeticException"
        | "java/lang/ArrayStoreException"
        | "java/lang/ClassCastException"
        | "java/lang/IllegalArgumentException"
        | "java/lang/IllegalStateException"
        | "java/lang/IndexOutOfBoundsException"
        | "java/lang/NegativeArraySizeException"
        | "java/lang/NullPointerException"
        | "java/lang/NumberFormatException"
        | "java/lang/UnsupportedOperationException"
        | "java/lang/SecurityException"
        | "java/lang/ArrayIndexOutOfBoundsException" => Some("java/lang/RuntimeException"),
        "java/lang/StringIndexOutOfBoundsException" => Some("java/lang/IndexOutOfBoundsException"),
        "java/lang/StackOverflowError" | "java/lang/OutOfMemoryError" | "java/lang/AssertionError" => {
            Some("java/lang/Error")
        }
        _ => None,
    }
}

impl<'a> Vm<'a> {
    /// 本类常量池里某个 Class 项的类名（instanceof / checkcast / anewarray 用）
    pub(super) fn class_name_of(&self, index: u16) -> Result<String, VmError> {
        match self.class.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
            Some(CpEntry::Class(n)) => Ok(String::from_utf8_lossy(self.class.utf8(*n).unwrap_or_default()).into_owned()),
            other => err(format!("索引 {index} 不是 Class 项: {other:?}")),
        }
    }

    /// 一个类名的直接父类。加载的只有本类，java.* 的层级写死在这张表里 ——
    /// 没有它 `catch (RuntimeException e)` 接不住 ArithmeticException。
    pub(super) fn super_of(&self, class: &str) -> Option<String> {
        if let Some(s) = builtin_super(class) {
            return Some(s.to_string());
        }
        let this = self.class.this_class_name()?;
        if class == this {
            let sup = self.class_name_of(self.class.super_class).ok()?;
            if sup.is_empty() {
                return None;
            }
            return Some(sup);
        }
        None
    }

    /// actual 是不是 target 或它的子类
    pub(super) fn is_subclass(&self, actual: &str, target: &str) -> bool {
        if actual == target {
            return true;
        }
        let mut cur = actual.to_string();
        for _ in 0..32 {
            match self.super_of(&cur) {
                Some(s) => {
                    if s == target {
                        return true;
                    }
                    cur = s;
                }
                None => return false,
            }
        }
        false
    }

    /// `instanceof` 的判定：null 对任何类型都是 false（含 Object）
    pub(super) fn is_instance(&self, handle: i32, target: &str) -> bool {
        match self.ref_kind(handle) {
            RefKind::Null => false,
            _ if target == "java/lang/Object" => true,
            RefKind::Str => target == "java/lang/String",
            RefKind::Obj(c) => self.is_subclass(&c, target),
            RefKind::Arr(d) => d == target,
        }
    }

    /// 出错信息里用的可读类型名
    pub(super) fn type_name(&self, handle: i32) -> String {
        match self.ref_kind(handle) {
            RefKind::Null => "null".into(),
            RefKind::Str => "java.lang.String".into(),
            RefKind::Obj(c) => c.replace('/', "."),
            RefKind::Arr(d) => d.replace('/', "."),
        }
    }
}
