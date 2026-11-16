//! AST → 字节码（模块树入口）。
//!
//! 覆盖：静态/实例方法、int/long/double/引用类型的参数与局部变量、算术表达式、
//! `return`、`System.out.println` 各重载、分支与 try/catch/finally（需要 StackMapTable）。
//!
//! 结构：
//! - `error`     编译错误
//! - `emit`      方法体字节码发射器（字节缓冲/栈深/标签/栈图帧/异常表）
//! - `constpool` 常量池构建器
//! - `types`     类型种类、描述符、验证类型、表达式类型推断
//! - `ctx`       编译上下文（符号表/局部槽/finally 栈）
//! - `expr`      表达式编译
//! - `stmt`      语句编译
//! - `method`    单个方法装配（返回/临时槽）
//! - `class`     类级装配（字段表/方法表/常量池收尾/写出）
//! - `simple`    简单直线代码生成（`simple_main` 等）
//!
//! 旧路径 `classfile::compiler::*` 经 lib.rs 的模块别名继续可用。

use crate::ast::*;

pub(crate) mod class;
pub(crate) mod constpool;
pub(crate) mod ctx;
pub(crate) mod emit;
pub(crate) mod error;
pub(crate) mod expr;
pub(crate) mod method;
pub(crate) mod simple;
pub(crate) mod stmt;
pub(crate) mod types;

// 对外 API（`classfile::codegen::*` / 经别名的 `classfile::compiler::*`）
pub use self::class::{ACC_PUBLIC, ACC_STATIC, ACC_SUPER};
pub use self::constpool::CpBuilder;
pub use self::error::CompileError;
pub use self::simple::*;
pub use self::types::descriptor;

use self::class::compile_type;

/// 编译一个编译单元，返回 (类名, class 字节)
pub fn compile(cu: &CompilationUnit) -> Result<Vec<(String, Vec<u8>)>, CompileError> {
    compile_with_path(cu, &[])
}

/// 编译一个编译单元，父类不在本单元时从 `classpath` 目录里读 `.class`
pub fn compile_with_path(
    cu: &CompilationUnit,
    classpath: &[std::path::PathBuf],
) -> Result<Vec<(String, Vec<u8>)>, CompileError> {
    // 嵌套类型先拍平成顶层类型（`Outer.Inner` → `Outer$Inner`）：必须在枚举降级之前，
    // 这样嵌套枚举走的是同一条「静态常量 + values/valueOf」的路径
    let cu = &crate::sema::flatten_unit(cu);
    // 泛型擦除：`Box<T>` 的字段/参数/返回值描述符用上界（默认 Object），
    // 之后的阶段（符号表、codegen、解释器、JIT）都不必认识类型参数
    let cu = &crate::sema::erase_unit(cu);
    // 枚举先降级成普通类（静态 final 常量 + 构造器 + values/valueOf/ordinal/name），
    // 这样 codegen / 解释器 / JIT 都不必认识「枚举」这个概念
    let cu = &crate::sema::desugar_unit(cu).map_err(|msg| CompileError { msg })?;
    // 同包的简单类型名先补成 `p/Name`（含 import 指到的）：后面所有描述符/继承链都按内部名走
    let cu = &crate::sema::qualify_unit(cu, classpath);
    let res = crate::sema::Resolver::new(cu, classpath);
    // classpath 上的类先读进来：跨编译单元的字段/方法表要在建 type_fields 之前就绪
    res.preload();
    let mut out = Vec::new();
    for t in &cu.types {
        // 返回的类名是**内部名**（带包前缀），写盘时按它建目录
        let name = match &cu.package {
            Some(p) if !p.is_empty() => format!("{}/{}", p.replace('.', "/"), t.name),
            _ => t.name.clone(),
        };
        out.push((name, compile_type(&cu.package, t, &res)?));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_compilation_unit;

    #[test]
    fn compiles_addition_method() {
        let src = r#"
            class Calc {
                static int add(int a, int b) { return a + b; }
            }
        "#;
        let cu = parse_compilation_unit(src).expect("parse");
        let out = compile(&cu).expect("compile");
        assert_eq!(out.len(), 1);
        let (name, bytes) = &out[0];
        assert_eq!(name, "Calc");
        let cf = crate::parse(bytes).expect("生成的字节能被解析");
        assert_eq!(cf.major_version, 69);
        // 构造器 + add
        assert_eq!(cf.methods.len(), 2);
    }

    #[test]
    fn method_descriptors_are_correct() {
        let src = r#"
            class M {
                static int add(int a, int b) { return a + b; }
                static long wide(long x) { return x; }
            }
        "#;
        let cu = parse_compilation_unit(src).expect("parse");
        let name = descriptor(&Type::Named { path: vec!["java".into(), "lang".into(), "String".into()], args: vec![] });
        assert_eq!(name, "Ljava/lang/String;");
        assert_eq!(descriptor(&Type::Array(Box::new(Type::Prim("int")))), "[I");
        let _ = cu;
    }

    #[test]
    fn compiles_main_with_println() {
        let src = r#"
            class T {
                static int add(int a, int b) { return a + b; }
                public static void main(String[] args) {
                    System.out.println(add(2, 3));
                }
            }
        "#;
        let cu = parse_compilation_unit(src).expect("parse");
        let out = compile(&cu).expect("compile");
        assert_eq!(out[0].0, "T");
    }
}
