//! 异常：抛出、异常表查找、未捕获异常的文本。

use super::Vm;
use super::VmError;

/// 抛一个 Java 异常（参数是异常对象的引用句柄）
    pub(super) fn thrown(h: i32) -> VmError {
    VmError { msg: format!("未捕获的 Java 异常（句柄 {h}）"), exc: Some(h) }
}

impl Vm {
    /// 异常对象 → 打印用的 `类名: message`
    pub(super) fn exc_text(&self, handle: i32) -> String {
        let class = self.type_name(handle);
        match self.exc_msgs.get(&handle) {
            Some(m) => format!("{class}: {}", self.string_text(*m)),
            None => class,
        }
    }

    /// 按顺序在异常表里找能接住这个异常的 handler（返回 handler 的字节码偏移）
    pub(super) fn find_handler(
        &self,
        cf: &crate::ClassFile,
        table: &[(u16, u16, u16, u16)],
        pc: usize,
        handle: i32,
    ) -> Result<Option<usize>, VmError> {
        for &(start, end, handler, catch_type) in table {
            if (start as usize) <= pc && pc < (end as usize) {
                if catch_type == 0 {
                    return Ok(Some(handler as usize)); // catch-all（finally）
                }
                let name = super::class::class_name_of(cf, catch_type)?;
                if self.is_instance(handle, &name) {
                    return Ok(Some(handler as usize));
                }
            }
        }
        Ok(None)
    }
}
