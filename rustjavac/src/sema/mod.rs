//! 语义分析的起点：符号表。
//!
//! 这一层把 AST 变成"可以直接喂给代码生成和 Rust 运行时"的结构：
//! 每个类型有哪些字段、字段是不是引用（决定 GC 追踪位图）、方法签名是什么、
//! 父类型与接口是谁。
//!
//! 符号表本体在 `symbols`，本模块只做导出。

pub mod enums;
pub mod erase;
pub mod generic;
pub mod nested;
pub mod qualify;
pub mod resolve;
pub mod symbols;

pub use enums::desugar_unit;
pub use erase::erase_unit;
pub use nested::flatten_unit;
pub use qualify::qualify_unit;
pub use resolve::Resolver;
pub use symbols::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_compilation_unit;

    #[test]
    fn builds_symbol_table_with_field_kinds() {
        let src = r#"
            class Point {
                int x;
                int y;
                String label;
                int[] data;
                long flags;
                void move(int dx, int dy) {}
                static Point origin() { return null; }
            }
        "#;
        let cu = parse_compilation_unit(src).expect("parse");
        let st = SymbolTable::build(&cu);
        let t = st.get("Point").expect("Point");
        assert_eq!(t.fields.len(), 5);
        assert_eq!(t.instance_fields(), 5);
        // x、y、flags 是原始；label、data 是引用
        assert_eq!(t.ref_fields(), 2);
        assert_eq!(t.ref_map, vec![FieldKind::Prim, FieldKind::Prim, FieldKind::Ref, FieldKind::Ref, FieldKind::Prim]);
        assert!(st.find_method(t, "move", 2).is_some());
        assert!(st.find_method(t, "move", 1).is_none());
        assert!(st.find_method(t, "origin", 0).map(|m| m.is_static).unwrap_or(false));
    }

    #[test]
    fn nested_types_are_collected() {
        let src = r#"
            class Outer {
                static class Inner { int v; }
                interface Callback { void call(); }
            }
        "#;
        let cu = parse_compilation_unit(src).expect("parse");
        let st = SymbolTable::build(&cu);
        assert!(st.get("Outer").is_some());
        assert!(st.get("Inner").is_some());
        assert!(st.get("Callback").is_some());
    }

    #[test]
    fn record_components_become_fields() {
        let src = r#"
            record Pair(int a, String b) {}
        "#;
        let cu = parse_compilation_unit(src).expect("parse");
        let st = SymbolTable::build(&cu);
        let t = st.get("Pair").expect("Pair");
        assert_eq!(t.fields.len(), 2);
        assert_eq!(t.ref_map, vec![FieldKind::Prim, FieldKind::Ref]);
    }

    #[test]
    fn constructors_are_not_methods() {
        let src = r#"
            class A {
                A(int x) {}
                void f() {}
            }
        "#;
        let cu = parse_compilation_unit(src).expect("parse");
        let st = SymbolTable::build(&cu);
        let t = st.get("A").expect("A");
        assert_eq!(t.methods.len(), 1);
        assert_eq!(t.methods[0].name, "f");
    }
}
