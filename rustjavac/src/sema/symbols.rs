//! 符号表本体：类型/字段/方法信息与收集过程。

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
                // 枚举常量在 `sema::enums` 里已经降成静态字段；漏到这里的按无字段处理
                Member::EnumConst { .. } => {}
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
