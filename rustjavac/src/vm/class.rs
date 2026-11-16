//! 类元数据、继承链与类型判定。

use crate::{ClassFile, CpEntry};

use super::err;
use super::heap::RefKind;
use super::Vm;
use super::VmError;

/// java.* 异常类的继承链（我们没有它们的字节码，这些层级只能写死）
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

/// 常量池里某个 Class 项的类名（instanceof / checkcast / anewarray / 异常表用）
pub(super) fn class_name_of(cf: &ClassFile, index: u16) -> Result<String, VmError> {
    match cf.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
        Some(CpEntry::Class(n)) => {
            Ok(String::from_utf8_lossy(cf.utf8(*n).unwrap_or_default()).into_owned())
        }
        other => err(format!("索引 {index} 不是 Class 项: {other:?}")),
    }
}

impl Vm {
    /// 静态初始化：先父类、再自己，只跑一次（JVM 的 `<clinit>` 语义）
    pub(super) fn ensure_init(&mut self, class: &str) -> Result<(), VmError> {
        if self.initialized.contains(class) {
            return Ok(());
        }
        // 先标记：`<clinit>` 里引用自己的静态字段时不能无限递归
        self.initialized.insert(class.to_string());
        if let Some(s) = self.store.super_of(class) {
            self.ensure_init(&s)?;
        }
        if let Some((ci, mi)) = self.store.method_in_class(class, "<clinit>", "()V") {
            self.depth += 1;
            let r = self.execute(ci, mi, "()V", &[], false);
            self.depth -= 1;
            r?;
        }
        Ok(())
    }

    /// actual 是不是 target 或它的子类（**接口也算**：实际类 implements 它即成立）
    pub(super) fn is_subclass(&self, actual: &str, target: &str) -> bool {
        if actual == target {
            return true;
        }
        self.store.chain(actual).iter().any(|c| c == target)
            || self.store.iface_closure(actual).iter().any(|c| c == target)
            || {
                // 链上没加载的 java.* 祖先再按内置表走一遍
                let mut cur = actual.to_string();
                for _ in 0..32 {
                    match builtin_super(&cur) {
                        Some(s) => {
                            if s == target {
                                return true;
                            }
                            cur = s.to_string();
                        }
                        None => break,
                    }
                }
                false
            }
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
