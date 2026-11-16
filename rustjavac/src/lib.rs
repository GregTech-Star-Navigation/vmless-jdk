//! JVM class 文件读写（Rust 版 javac 的地基）。
//!
//! 目标：读入任意 `.class`，原样写回，**逐字节一致**。
//! 有了它，符号表、代码生成、Rust 运行时（librustjrt）才有共同的数据结构。
//!
//! 本文件只剩模块声明与顶层 re-export —— 具体实现见各模块目录。

pub mod classfile;
pub mod front;
pub mod sema;
pub mod lower;
pub mod codegen;
pub use crate::codegen as compiler;
pub mod vm;
pub mod jit;

// 旧路径保持可用：`classfile::parse` / `classfile::write` / `classfile::Error` / `classfile::Result`
pub use crate::classfile::{parse, write, Error, Result};
pub use crate::classfile::{java_double_to_string, ret_of};
pub use crate::classfile::{Attribute, ClassFile, CpEntry, Member};
// 旧路径保持可用：`classfile::ast::*` / `classfile::lexer::*` / `classfile::parser::*`
pub use crate::front::{ast, lexer, parser};
