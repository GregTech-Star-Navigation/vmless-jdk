//! 编译前端：AST、词法分析、语法分析。
//!
//! 原来的 `src/ast.rs` / `src/lexer.rs` / `src/parser.rs` 原样迁进本目录，
//! 旧路径由 `lib.rs` 的 re-export 继续提供（`crate::ast` / `crate::lexer` / `crate::parser`）。

pub mod ast;
pub mod lexer;
pub mod parser;
