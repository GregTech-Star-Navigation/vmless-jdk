//! 枚举降级：`enum E { A, B(2); ... }` → 普通类。
//!
//! 为什么不给 codegen / 解释器 / JIT 各加一套「枚举」概念：枚举在字节码层面
//! 本来就没有新东西 —— javac 也是把它编成一个普通 final 类。既然降到既有构造，
//! 三个引擎都不用改一行代码。降级后的形状：
//!
//! ```java
//! final class E {
//!     static final E A = new E("A", 0);
//!     static final E B = new E("B", 1, 2);      // 常量实参接到用户构造器后面
//!     private final String $name;
//!     private final int $ordinal;
//!     E(String $name, int $ordinal, <用户参数…>) {
//!         this.$name = $name;
//!         this.$ordinal = $ordinal;
//!         <用户构造器体>
//!     }
//!     String name()    { return $name; }
//!     int ordinal()    { return $ordinal; }
//!     String toString(){ return $name; }
//!     static E[] values() { E[] v = new E[2]; v[0] = A; v[1] = B; return v; }
//!     static E valueOf(String s) { …按 name() 比… }
//! }
//! ```
//!
//! 父类是 `java/lang/Object`（不是 `java/lang/Enum`）：我们的运行时里没有
//! `java/lang/Enum`，凭空继承一个不存在的类会让解释器/JIT 都找不到父类。

use crate::ast::*;

/// 把编译单元里所有 `enum` 声明改成等价的类声明；没有枚举时原样返回。
pub fn desugar_unit(cu: &CompilationUnit) -> Result<CompilationUnit, String> {
    let mut out = cu.clone();
    for t in out.types.iter_mut() {
        desugar_type(t)?;
    }
    Ok(out)
}

fn desugar_type(t: &mut TypeDecl) -> Result<(), String> {
    // 嵌套类型：嵌套**类**本来就不支持（codegen 只发射顶层类型），嵌套枚举走到这里
    // 直接给个明确的错，别让运行时报 ClassNotFound
    for m in t.members.iter_mut() {
        if let Member::Type(inner) = m {
            if inner.kind == TypeKind::Enum {
                return Err(format!(
                    "暂不支持嵌套枚举 `{}.{}`：请把 enum 声明挪到顶层（同一个文件里可以写多个顶层类型）",
                    t.name, inner.name
                ));
            }
            desugar_type(inner)?;
        }
    }
    if t.kind != TypeKind::Enum {
        return Ok(());
    }
    let name = t.name.clone();
    let self_ty = Type::Named { path: vec![name.clone()], args: vec![] };

    // 1. 拆出常量与其余成员
    let mut consts: Vec<(String, Vec<Expr>)> = Vec::new();
    let mut rest: Vec<Member> = Vec::new();
    for m in std::mem::take(&mut t.members) {
        match m {
            Member::EnumConst { name, args } => consts.push((name, args)),
            other => rest.push(other),
        }
    }

    // 2. 用户构造器：参数前面插 (String $name, int $ordinal)
    let mut has_ctor = false;
    for m in rest.iter_mut() {
        if let Member::Method(md) = m {
            if md.ret.is_none() && md.name == name {
                has_ctor = true;
                md.params.insert(
                    0,
                    Param { ty: Type::Prim("int"), name: "$ordinal".into(), varargs: false },
                );
                md.params.insert(
                    0,
                    Param { ty: Type::Named { path: vec!["String".into()], args: vec![] }, name: "$name".into(), varargs: false },
                );
                // 构造器体的最前面补两条字段赋值
                let mut body = vec![
                    stmt_assign_field("$name", "$name"),
                    stmt_assign_field("$ordinal", "$ordinal"),
                ];
                match md.body.take() {
                    Some(Stmt::Block(items)) => body.extend(items),
                    Some(other) => body.push(other),
                    None => {}
                }
                md.body = Some(Stmt::Block(body));
            }
        }
    }
    if !has_ctor {
        // 没写构造器：常量也不许带实参（带了就没法接到哪里去）
        if let Some((cname, _)) = consts.iter().find(|(_, a)| !a.is_empty()) {
            return Err(format!(
                "枚举 `{name}` 的常量 `{cname}` 带了实参，但这个枚举没有声明构造器"
            ));
        }
        let body = Stmt::Block(vec![
            stmt_assign_field("$name", "$name"),
            stmt_assign_field("$ordinal", "$ordinal"),
        ]);
        rest.push(Member::Method(MethodDecl {
            modifiers: Vec::new(),
            type_params: Vec::new(),
            ret: None,
            name: name.clone(),
            params: vec![
                Param { ty: Type::Named { path: vec!["String".into()], args: vec![] }, name: "$name".into(), varargs: false },
                Param { ty: Type::Prim("int"), name: "$ordinal".into(), varargs: false },
            ],
            throws: Vec::new(),
            body: Some(body),
        }));
    }

    // 3. 静态 final 常量：`static final E A = new E("A", 0, …)`
    let mut members: Vec<Member> = Vec::new();
    for (i, (cname, args)) in consts.iter().enumerate() {
        let mut new_args = vec![
            Expr::StrLit(cname.clone()),
            Expr::IntLit(i.to_string()),
        ];
        new_args.extend(args.iter().cloned());
        members.push(Member::Field(FieldDecl {
            modifiers: vec!["static".into(), "final".into()],
            ty: self_ty.clone(),
            name: cname.clone(),
            init: Some(Expr::New { ty: self_ty.clone(), args: new_args }),
        }));
    }
    // 4. 两个隐藏字段
    members.push(Member::Field(FieldDecl {
        modifiers: vec!["private".into(), "final".into()],
        ty: Type::Named { path: vec!["String".into()], args: vec![] },
        name: "$name".into(),
        init: None,
    }));
    members.push(Member::Field(FieldDecl {
        modifiers: vec!["private".into(), "final".into()],
        ty: Type::Prim("int"),
        name: "$ordinal".into(),
        init: None,
    }));
    // 5. 用户成员（构造器已改好签名）
    members.extend(rest);
    // 6. name() / ordinal() / toString()
    members.push(gen_method("name", Some(str_ty()), vec![], Stmt::Return(Some(Expr::Ident("$name".into())))));
    members.push(gen_method(
        "ordinal",
        Some(Type::Prim("int")),
        vec![],
        Stmt::Return(Some(Expr::Ident("$ordinal".into()))),
    ));
    members.push(gen_method(
        "toString",
        Some(str_ty()),
        vec![],
        Stmt::Return(Some(Expr::Ident("$name".into()))),
    ));
    // 7. values() / valueOf(String)
    members.push(gen_values(&self_ty, &consts));
    members.push(gen_value_of(&self_ty));

    t.kind = TypeKind::Class;
    if !t.modifiers.iter().any(|m| m == "final") {
        t.modifiers.push("final".into());
    }
    t.members = members;
    Ok(())
}

fn str_ty() -> Type {
    Type::Named { path: vec!["String".into()], args: vec![] }
}

fn gen_method(name: &str, ret: Option<Type>, params: Vec<Param>, body: Stmt) -> Member {
    Member::Method(MethodDecl {
        modifiers: Vec::new(),
        type_params: Vec::new(),
        ret,
        name: name.to_string(),
        params,
        throws: Vec::new(),
        body: Some(body),
    })
}

fn gen_static_method(name: &str, ret: Type, params: Vec<Param>, body: Stmt) -> Member {
    Member::Method(MethodDecl {
        modifiers: vec!["static".into()],
        type_params: Vec::new(),
        ret: Some(ret),
        name: name.to_string(),
        params,
        throws: Vec::new(),
        body: Some(body),
    })
}

/// `this.<field> = <local>;`
fn stmt_assign_field(field: &str, from: &str) -> Stmt {
    Stmt::Expr(Expr::Assign {
        op: None,
        lhs: Box::new(Expr::Field(Box::new(Expr::This), field.to_string())),
        rhs: Box::new(Expr::Ident(from.to_string())),
    })
}

fn ident(n: &str) -> Expr {
    Expr::Ident(n.to_string())
}

fn arr_get(name: &str, idx: Expr) -> Expr {
    Expr::ArrayAccess {
        array: Box::new(ident(name)),
        index: Box::new(idx),
    }
}

/// `static E[] values()`：每次调用都给一份新数组（与 javac 一致）
fn gen_values(self_ty: &Type, consts: &[(String, Vec<Expr>)]) -> Member {
    let arr_ty = Type::Array(Box::new(self_ty.clone()));
    let n = consts.len();
    let mut body = vec![Stmt::LocalVar {
        ty: arr_ty.clone(),
        name: "$v".into(),
        init: Some(Expr::ArrayNew {
            elem: self_ty.clone(),
            dims: vec![Expr::IntLit(n.to_string())],
        }),
    }];
    for (i, (cname, _)) in consts.iter().enumerate() {
        body.push(Stmt::Expr(Expr::Assign {
            op: None,
            lhs: Box::new(arr_get("$v", Expr::IntLit(i.to_string()))),
            rhs: Box::new(ident(cname)),
        }));
    }
    body.push(Stmt::Return(Some(ident("$v"))));
    gen_static_method("values", arr_ty, vec![], Stmt::Block(body))
}

/// `static E valueOf(String s)`：按 `name()` 线性查找，找不到返回 null
/// （真 javac 抛 IllegalArgumentException；我们的异常体系里没有它，保持简单）
fn gen_value_of(self_ty: &Type) -> Member {
    let arr_ty = Type::Array(Box::new(self_ty.clone()));
    let i_cond = Expr::Binary {
        op: BinOp::Lt,
        lhs: Box::new(ident("$i")),
        rhs: Box::new(Expr::Field(Box::new(ident("$vs")), "length".into())),
    };
    let name_field = Expr::Field(Box::new(arr_get("$vs", ident("$i"))), "$name".into());
    let eq = Expr::Call {
        recv: Some(Box::new(name_field)),
        name: "equals".into(),
        args: vec![ident("$s")],
        type_args: Vec::new(),
    };
    let body = Stmt::Block(vec![
        Stmt::LocalVar {
            ty: arr_ty,
            name: "$vs".into(),
            init: Some(Expr::Call {
                recv: None,
                name: "values".into(),
                args: Vec::new(),
                type_args: Vec::new(),
            }),
        },
        Stmt::For {
            init: vec![Stmt::LocalVar {
                ty: Type::Prim("int"),
                name: "$i".into(),
                init: Some(Expr::IntLit("0".into())),
            }],
            cond: Some(i_cond),
            update: vec![Expr::Assign {
                op: None,
                lhs: Box::new(ident("$i")),
                rhs: Box::new(Expr::Binary {
                    op: BinOp::Add,
                    lhs: Box::new(ident("$i")),
                    rhs: Box::new(Expr::IntLit("1".into())),
                }),
            }],
            body: Box::new(Stmt::If {
                cond: eq,
                then: Box::new(Stmt::Return(Some(arr_get("$vs", ident("$i"))))),
                els: None,
            }),
        },
        Stmt::Return(Some(Expr::Null)),
    ]);
    gen_static_method(
        "valueOf",
        self_ty.clone(),
        vec![Param { ty: str_ty(), name: "$s".into(), varargs: false }],
        body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_compilation_unit;

    #[test]
    fn desugars_simple_enum() {
        let src = r#"enum Color { RED, GREEN, BLUE }"#;
        let cu = parse_compilation_unit(src).expect("parse");
        let out = desugar_unit(&cu).expect("desugar");
        let t = &out.types[0];
        assert_eq!(t.kind, TypeKind::Class);
        // 三个常量字段 + $name + $ordinal
        let fields: Vec<&str> = t
            .members
            .iter()
            .filter_map(|m| match m {
                Member::Field(f) => Some(f.name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(fields, vec!["RED", "GREEN", "BLUE", "$name", "$ordinal"]);
        // 方法：构造器 + name + ordinal + toString + values + valueOf
        let methods: Vec<&str> = t
            .members
            .iter()
            .filter_map(|m| match m {
                Member::Method(md) => Some(md.name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            methods,
            vec!["Color", "name", "ordinal", "toString", "values", "valueOf"]
        );
    }

    #[test]
    fn desugars_enum_with_constant_args() {
        let src = r#"
            enum Code {
                OK(200), BAD(500);
                int v;
                Code(int v) { this.v = v; }
            }
        "#;
        let cu = parse_compilation_unit(src).expect("parse");
        let out = desugar_unit(&cu).expect("desugar");
        let t = &out.types[0];
        let Member::Method(ctor) = t
            .members
            .iter()
            .find(|m| matches!(m, Member::Method(md) if md.name == "Code"))
            .expect("ctor")
        else {
            panic!("ctor");
        };
        // (String, int) 被插到用户参数前面
        assert_eq!(ctor.params.len(), 3);
        assert_eq!(ctor.params[0].name, "$name");
        assert_eq!(ctor.params[1].name, "$ordinal");
        assert_eq!(ctor.params[2].name, "v");
    }

    #[test]
    fn rejects_constant_args_without_ctor() {
        let src = r#"enum Bad { A(1) }"#;
        let cu = parse_compilation_unit(src).expect("parse");
        assert!(desugar_unit(&cu).is_err());
    }
}
