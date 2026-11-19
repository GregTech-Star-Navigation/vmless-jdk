//! 方法引用解析、参数个数与参数槽位。

use crate::CpEntry;

use super::err;
use super::Vm;
use super::VmError;

pub(super) struct MethodRefInfo {
    pub(super) class: String,
    pub(super) name: String,
    pub(super) desc: String,
    /// 方法解析结果缓存（JVMS 5.4.3.3：解析成功后缓存，`invokespecial`/`invokestatic` 用）。
    /// 0 = 未解析；否则 `(class_idx << 32 | method_idx) + 1`。`Cell` 是因为 site 缓存
    /// 是线程本地的 `Rc`，写入只发生在本线程。
    pub(super) target: std::cell::Cell<u64>,
    /// 引用类的 `<clinit>` 已经跑过（初始化单调、永不回退）。
    pub(super) inited: std::cell::Cell<bool>,
    /// 字段偏移缓存：`(类表代数, 偏移 + 1)`。代数 = `store.classes.len()`，
    /// 任何类加载都会让它变，从而让全部旧的偏移缓存失效 —— 详见 `field_off_cached`。
    pub(super) field: std::cell::Cell<(u32, u32)>,
    /// 字符串句柄的槽下标缓存（`str_field_slot` 的结果 + 1，0 = 未解析）
    pub(super) str_slot: std::cell::Cell<u32>,
    /// 虚分派单态缓存：`(接收者类名 Arc 指针, (class_idx<<32|method_idx)+1, 类名 Arc)`。
    /// 第三个字段是**握住**那个 `Arc`：不攥住的话，旧 `Vm` 被回收后类名 `Arc` 可能被
    /// 释放、地址被新类名复用 —— 缓存就会把一个类的目标错给另一个类。
    pub(super) mono: std::cell::RefCell<(usize, u64, Option<std::sync::Arc<str>>)>,
    /// 参数个数缓存（`usize::MAX` = 未算），描述符不变、答案不变
    pub(super) argc: std::cell::Cell<usize>,
}

impl Vm {
    pub(super) fn method_ref(&self, cf: &crate::ClassFile, index: u16) -> Result<MethodRefInfo, VmError> {
        let e = cf
            .constant_pool
            .get(index as usize)
            .and_then(|e| e.as_ref())
            .ok_or_else(|| VmError { msg: format!("常量池索引 {index} 为空"), exc: None })?;
        let (class_idx, nat_idx) = match e {
            CpEntry::MethodRef(c, n) | CpEntry::InterfaceMethodRef(c, n) => (*c, *n),
            CpEntry::FieldRef(c, n) => (*c, *n),
            other => return err(format!("索引 {index} 不是方法/字段引用: {other:?}")),
        };
        let class_name = match cf.constant_pool.get(class_idx as usize).and_then(|e| e.as_ref()) {
            Some(CpEntry::Class(n)) => {
                String::from_utf8_lossy(cf.utf8(*n).unwrap_or_default()).into_owned()
            }
            _ => return err("方法引用的类名解析失败"),
        };
        let (name, desc) = match cf.constant_pool.get(nat_idx as usize).and_then(|e| e.as_ref()) {
            Some(CpEntry::NameAndType(n, d)) => (
                String::from_utf8_lossy(cf.utf8(*n).unwrap_or_default()).into_owned(),
                String::from_utf8_lossy(cf.utf8(*d).unwrap_or_default()).into_owned(),
            ),
            _ => return err("NameAndType 解析失败"),
        };
        Ok(MethodRefInfo {
            class: class_name,
            name,
            desc,
            target: std::cell::Cell::new(0),
            inited: std::cell::Cell::new(false),
            field: std::cell::Cell::new((0, 0)),
            str_slot: std::cell::Cell::new(0),
            mono: std::cell::RefCell::new((0, 0, None)),
            argc: std::cell::Cell::new(usize::MAX),
        })
    }
}

/// `(II)I` → 2（数的是**参数个数**，long/double 各算一个）
    pub(super) fn count_args(desc: &str) -> Result<usize, VmError> {
    let bytes = desc.as_bytes();
    let mut i = 1;
    let mut n = 0;
    while i < bytes.len() && bytes[i] != b')' {
        match bytes[i] {
            b'[' => {
                i += 1;
                while i < bytes.len() && bytes[i] == b'[' {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'L' {
                    while i < bytes.len() && bytes[i] != b';' {
                        i += 1;
                    }
                }
                i += 1;
                n += 1;
            }
            b'L' => {
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1;
                n += 1;
            }
            b'J' | b'D' => {
                i += 1;
                n += 1;
            }
            _ => {
                i += 1;
                n += 1;
            }
        }
    }
    Ok(n)
}

impl MethodRefInfo {
    /// 参数个数（按调用点缓存：描述符是常量池里的常量，答案永远不变）
    #[inline]
    pub(super) fn argc(&self) -> Result<usize, VmError> {
        let c = self.argc.get();
        if c != usize::MAX {
            return Ok(c);
        }
        let n = count_args(&self.desc)?;
        self.argc.set(n);
        Ok(n)
    }
}
