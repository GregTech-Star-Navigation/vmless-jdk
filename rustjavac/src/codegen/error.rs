//! 编译错误。

#[derive(Debug)]
pub struct CompileError {
    pub msg: String,
}

pub(crate) fn err<T>(msg: impl Into<String>) -> Result<T, CompileError> {
    Err(CompileError { msg: msg.into() })
}


impl From<String> for CompileError {
    fn from(msg: String) -> Self {
        CompileError { msg }
    }
}

impl From<&str> for CompileError {
    fn from(msg: &str) -> Self {
        CompileError { msg: msg.to_string() }
    }
}
