//! 编译错误。

#[derive(Debug)]
pub struct CompileError {
    pub msg: String,
}

pub(crate) fn err<T>(msg: impl Into<String>) -> Result<T, CompileError> {
    Err(CompileError { msg: msg.into() })
}

