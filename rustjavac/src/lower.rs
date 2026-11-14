//! 把源码的符号表降到 Rust 运行时的类元数据。
//!
//! 这是两条线的交汇点：左边是 javac 侧的 `sema::TypeInfo`（源码里有哪些类、字段是不是引用），
//! 右边是 `librustjrt::ClassMeta`（标记清扫时按位图追踪引用）。

use crate::sema;
use librustjrt::{ClassMeta, FieldKind as RtKind};

/// 源码类型 → 运行时类元数据
pub fn class_meta(ti: &sema::TypeInfo) -> ClassMeta {
    ClassMeta {
        name: ti.name.clone(),
        fields: ti
            .ref_map
            .iter()
            .map(|k| match k {
                sema::FieldKind::Prim => RtKind::Prim,
                sema::FieldKind::Ref => RtKind::Ref,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_compilation_unit;

    #[test]
    fn lowers_ref_bitmap_to_runtime_meta() {
        let src = r#"
            class Node {
                int value;
                Node next;
                String label;
            }
        "#;
        let cu = parse_compilation_unit(src).expect("parse");
        let st = sema::SymbolTable::build(&cu);
        let ti = st.get("Node").expect("Node");
        let meta = class_meta(ti);
        assert_eq!(meta.name, "Node");
        assert_eq!(meta.fields, vec![RtKind::Prim, RtKind::Ref, RtKind::Ref]);
        assert_eq!(meta.size(), librustjrt::JavaObject::size_for(3));
    }
}
