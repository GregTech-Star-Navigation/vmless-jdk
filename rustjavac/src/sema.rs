//! 语义分析的起点：符号表。
//!
//! 这一层把 AST 变成"可以直接喂给代码生成和 Rust 运行时"的结构：
//! 每个类型有哪些字段、字段是不是引用（决定 GC 追踪位图）、方法签名是什么、
//! 父类型与接口是谁。

use crate::ast::*;
use std::collections::HashMap;

/// 字段类别：与 librustjrt 的 `FieldKind` 对应，决定标记清扫是否追踪这个槽。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    Prim,
    Ref,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldInfo {
    pub name: String,
    pub ty: Type,
    pub kind: FieldKind,
    pub is_static: bool,
    pub is_final: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParamInfo {
    pub name: String,
    pub ty: Type,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MethodInfo {
    pub name: String,
    pub params: Vec<ParamInfo>,
    pub ret: Option<Type>,
    pub is_static: bool,
    pub is_abstract: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypeInfo {
    pub name: String,
    pub kind: TypeKind,
    pub super_type: Option<Type>,
    pub interfaces: Vec<Type>,
    pub fields: Vec<FieldInfo>,
    pub methods: Vec<MethodInfo>,
    /// 引用槽位图：`ref_map[i]` 表示第 i 个实例字段是不是引用
    pub ref_map: Vec<FieldKind>,
}

impl TypeInfo {
    pub fn ref_fields(&self) -> usize {
        self.ref_map.iter().filter(|k| **k == FieldKind::Ref).count()
    }

    pub fn instance_fields(&self) -> usize {
        self.ref_map.len()
    }
}

#[derive(Default)]
pub struct SymbolTable {
    pub types: HashMap<String, TypeInfo>,
    /// 包名（同一编译单元内只有一个）
    pub package: Option<String>,
}

/// 基础类型 / 明显不是引用的类型
fn is_primitive_name(ty: &Type) -> bool {
    matches!(ty, Type::Prim(_))
}

/// 判断字段是不是引用槽：
/// 数组、泛型实参、以及任何非原始类型都算引用。
fn field_kind(ty: &Type) -> FieldKind {
    if is_primitive_name(ty) {
        FieldKind::Prim
    } else {
        FieldKind::Ref
    }
}

impl SymbolTable {
    pub fn build(cu: &CompilationUnit) -> Self {
        let mut st = SymbolTable { types: HashMap::new(), package: cu.package.clone() };
        for t in &cu.types {
            Self::collect(&mut st, t);
        }
        st
    }

    fn collect(st: &mut SymbolTable, t: &TypeDecl) {
        let mut fields = Vec::new();
        let mut methods = Vec::new();
        for m in &t.members {
            match m {
                Member::Field(f) => fields.push(FieldInfo {
                    name: f.name.clone(),
                    ty: f.ty.clone(),
                    kind: field_kind(&f.ty),
                    is_static: f.modifiers.iter().any(|m| m == "static"),
                    is_final: f.modifiers.iter().any(|m| m == "final"),
                }),
                Member::Method(md) => {
                    // 构造器（ret 为 None 且名字等于类型名）不进方法表
                    let is_ctor = md.ret.is_none() && md.name == t.name;
                    if !is_ctor {
                        methods.push(MethodInfo {
                            name: md.name.clone(),
                            params: md
                                .params
                                .iter()
                                .map(|p| ParamInfo { name: p.name.clone(), ty: p.ty.clone() })
                                .collect(),
                            ret: md.ret.clone(),
                            is_static: md.modifiers.iter().any(|m| m == "static"),
                            is_abstract: md.modifiers.iter().any(|m| m == "abstract"),
                        });
                    }
                }
                Member::Type(nested) => Self::collect(st, nested),
                Member::Init { .. } => {}
            }
        }
        let ref_map: Vec<FieldKind> = fields
            .iter()
            .filter(|f| !f.is_static)
            .map(|f| f.kind)
            .collect();

        st.types.insert(
            t.name.clone(),
            TypeInfo {
                name: t.name.clone(),
                kind: t.kind,
                super_type: t.extends.first().cloned(),
                interfaces: t.implements.clone(),
                fields,
                methods,
                ref_map,
            },
        );
    }

    pub fn get(&self, name: &str) -> Option<&TypeInfo> {
        self.types.get(name)
    }

    /// 同名方法按参数个数查找（重载解析的最简形态）
    pub fn find_method<'a>(&'a self, ty: &'a TypeInfo, name: &str, argc: usize) -> Option<&'a MethodInfo> {
        ty.methods.iter().find(|m| m.name == name && m.params.len() == argc)
    }
}

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
