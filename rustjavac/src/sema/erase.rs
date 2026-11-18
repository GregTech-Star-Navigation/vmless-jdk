//! 泛型擦除：把所有类型参数换成它的上界（没有上界就是 `java/lang/Object`）。
//!
//! 我们不做类型检查，只要「能编译、能跑」：`class Box<T> { T v; }` 的字段
//! 描述符必须是 `Ljava/lang/Object;`（或 `<T extends Number>` 的 `Ljava/lang/Number;`），
//! 而不是 `LT;` —— 后者 HotSpot 直接拒绝加载（类 `T` 不存在）。
//!
//! 调用点那边（`String s = box.get();`）拿到的静态类型是 `Object`，
//! 由 `codegen::types::coerce_to` 按目标类型补 `checkcast`，所以擦除之后
//! 仍然能当具体类型用（这就是 javac 在泛型位置插 checkcast 的同一套做法）。
//!
//! 泛型实参（`Box<String>` 的 `String`）本来就进不了描述符，不用管；
//! 但数组里套类型参数（`T[]` → `[Ljava/lang/Object;`）必须跟着换。

use crate::ast::*;
use std::collections::HashMap;

/// 类型参数名 → 擦除后的类型（上界）
type Env = HashMap<String, Type>;

fn object_ty() -> Type {
    Type::Named { path: vec!["Object".into()], args: Vec::new() }
}

pub fn erase_unit(cu: &CompilationUnit) -> CompilationUnit {
    let mut out = cu.clone();
    let env = Env::new();
    for t in out.types.iter_mut() {
        erase_type_decl(t, &env);
    }
    out
}

fn erase_type_decl(t: &mut TypeDecl, outer: &Env) {
    // 类自己的类型参数：`<T>` → Object，`<T extends Number>` → Number
    let mut env = outer.clone();
    for tp in &t.type_params {
        let bound = tp.bound.clone().unwrap_or_else(object_ty);
        env.insert(tp.name.clone(), erase_ty(&bound, &env));
    }
    for b in t.extends.iter_mut() {
        *b = erase_ty(b, &env);
    }
    for b in t.implements.iter_mut() {
        *b = erase_ty(b, &env);
    }
    for m in t.members.iter_mut() {
        match m {
            Member::Field(f) => {
                f.ty = erase_ty(&f.ty, &env);
                if let Some(e) = f.init.as_mut() {
                    erase_expr(e, &env);
                }
            }
            Member::Method(md) => erase_method(md, &env),
            Member::Type(nested) => erase_type_decl(nested, &env),
            Member::Init { body, .. } => erase_stmt(body, &env),
            Member::EnumConst { args, .. } => {
                for a in args.iter_mut() {
                    erase_expr(a, &env);
                }
            }
        }
    }
}

fn erase_method(md: &mut MethodDecl, outer: &Env) {
    // 方法级类型参数（`<T> T max(T a, T b)`）只在这个方法体里有效
    let mut env = outer.clone();
    for tp in &md.type_params {
        let bound = tp.bound.clone().unwrap_or_else(object_ty);
        env.insert(tp.name.clone(), erase_ty(&bound, &env));
    }
    if let Some(r) = md.ret.as_mut() {
        *r = erase_ty(r, &env);
    }
    for p in md.params.iter_mut() {
        p.ty = erase_ty(&p.ty, &env);
    }
    for th in md.throws.iter_mut() {
        *th = erase_ty(th, &env);
    }
    if let Some(b) = md.body.as_mut() {
        erase_stmt(b, &env);
    }
}

fn erase_ty(t: &Type, env: &Env) -> Type {
    match t {
        // 裸类型参数（`T`）—— 没有实参时才可能是类型参数
        Type::Named { path, args } if path.len() == 1 && args.is_empty() => {
            match env.get(&path[0]) {
                Some(b) => b.clone(),
                None => t.clone(),
            }
        }
        Type::Named { path, args } => Type::Named {
            path: path.clone(),
            args: args.iter().map(|a| erase_ty(a, env)).collect(),
        },
        Type::Array(inner) => Type::Array(Box::new(erase_ty(inner, env))),
        other => other.clone(),
    }
}

fn erase_stmt(s: &mut Stmt, env: &Env) {
    match s {
        Stmt::Block(items) => {
            for i in items.iter_mut() {
                erase_stmt(i, env);
            }
        }
        Stmt::LocalVar { ty, init, .. } => {
            *ty = erase_ty(ty, env);
            if let Some(e) = init.as_mut() {
                erase_expr(e, env);
            }
        }
        Stmt::Expr(e) => erase_expr(e, env),
        Stmt::If { cond, then, els } => {
            erase_expr(cond, env);
            erase_stmt(then, env);
            if let Some(e) = els.as_mut() {
                erase_stmt(e, env);
            }
        }
        Stmt::While { cond, body } => {
            erase_expr(cond, env);
            erase_stmt(body, env);
        }
        Stmt::DoWhile { body, cond } => {
            erase_stmt(body, env);
            erase_expr(cond, env);
        }
        Stmt::For { init, cond, update, body } => {
            for i in init.iter_mut() {
                erase_stmt(i, env);
            }
            if let Some(c) = cond.as_mut() {
                erase_expr(c, env);
            }
            for u in update.iter_mut() {
                erase_expr(u, env);
            }
            erase_stmt(body, env);
        }
        Stmt::ForEach { ty, iter, body, .. } => {
            *ty = erase_ty(ty, env);
            erase_expr(iter, env);
            erase_stmt(body, env);
        }
        Stmt::Return(e) => {
            if let Some(x) = e.as_mut() {
                erase_expr(x, env);
            }
        }
        Stmt::Throw(e) => erase_expr(e, env),
        Stmt::Labeled { body, .. } => erase_stmt(body, env),
        Stmt::Switch { selector, cases } => {
            erase_expr(selector, env);
            for c in cases.iter_mut() {
                for l in c.labels.iter_mut().flatten() {
                    erase_expr(l, env);
                }
                for b in c.body.iter_mut() {
                    erase_stmt(b, env);
                }
            }
        }
        Stmt::Try { body, catches, finally } => {
            erase_stmt(body, env);
            for c in catches.iter_mut() {
                for t in c.types.iter_mut() {
                    *t = erase_ty(t, env);
                }
                erase_stmt(&mut c.body, env);
            }
            if let Some(f) = finally.as_mut() {
                erase_stmt(f, env);
            }
        }
        Stmt::Yield(e) => erase_expr(e, env),
        Stmt::Break(_) | Stmt::Continue(_) | Stmt::Empty => {}
    }
}

fn erase_expr(e: &mut Expr, env: &Env) {
    match e {
        Expr::Field(r, _) => erase_expr(r, env),
        Expr::Call { recv, args, type_args, .. } => {
            if let Some(r) = recv.as_mut() {
                erase_expr(r, env);
            }
            for a in args.iter_mut() {
                erase_expr(a, env);
            }
            for t in type_args.iter_mut() {
                *t = erase_ty(t, env);
            }
        }
        Expr::New { ty, args } => {
            *ty = erase_ty(ty, env);
            for a in args.iter_mut() {
                erase_expr(a, env);
            }
        }
        Expr::ArrayNew { elem, dims } => {
            *elem = erase_ty(elem, env);
            for d in dims.iter_mut() {
                erase_expr(d, env);
            }
        }
        Expr::ArrayInit(items) => {
            for i in items.iter_mut() {
                erase_expr(i, env);
            }
        }
        Expr::ArrayAccess { array, index } => {
            erase_expr(array, env);
            erase_expr(index, env);
        }
        Expr::Binary { lhs, rhs, .. } => {
            erase_expr(lhs, env);
            erase_expr(rhs, env);
        }
        Expr::Unary { expr, .. } => erase_expr(expr, env),
        Expr::Assign { lhs, rhs, .. } => {
            erase_expr(lhs, env);
            erase_expr(rhs, env);
        }
        Expr::Cond { cond, then, els } => {
            erase_expr(cond, env);
            erase_expr(then, env);
            erase_expr(els, env);
        }
        Expr::Cast { ty, expr } => {
            *ty = erase_ty(ty, env);
            erase_expr(expr, env);
        }
        Expr::InstanceOf { expr, ty, .. } => {
            erase_expr(expr, env);
            *ty = erase_ty(ty, env);
        }
        Expr::Lambda { body, .. } => erase_expr(body, env),
        Expr::LambdaStmt { body, .. } => erase_stmt(body, env),
        Expr::MethodRef { recv, .. } => erase_expr(recv, env),
        Expr::ClassLit(t) => *t = erase_ty(t, env),
        Expr::SwitchExpr(s) => erase_stmt(s, env),
        Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::StrLit(_)
        | Expr::TextBlock(_)
        | Expr::CharLit(_)
        | Expr::BoolLit(_)
        | Expr::Null
        | Expr::This
        | Expr::Super
        | Expr::Ident(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_compilation_unit;

    fn descriptors_of(src: &str) -> Vec<String> {
        let cu = parse_compilation_unit(src).expect("parse");
        let cu = erase_unit(&cu);
        let mut out = Vec::new();
        for t in &cu.types {
            for m in &t.members {
                match m {
                    Member::Field(f) => out.push(crate::codegen::descriptor(&f.ty)),
                    Member::Method(md) => {
                        out.push(crate::codegen::types::method_descriptor(&md.params, &md.ret));
                    }
                    _ => {}
                }
            }
        }
        out
    }

    #[test]
    fn type_parameter_becomes_object() {
        let got = descriptors_of("class Box<T> { T v; T get() { return v; } }");
        assert_eq!(got, vec!["Ljava/lang/Object;".to_string(), "()Ljava/lang/Object;".to_string()]);
    }

    #[test]
    fn bound_is_used_when_present() {
        let got = descriptors_of("class NBox<T extends Number> { T v; }");
        assert_eq!(got, vec!["Ljava/lang/Number;".to_string()]);
    }

    #[test]
    fn method_type_parameter_erases() {
        let got = descriptors_of("class M { static <T> T max(T a, T b) { return a; } }");
        assert_eq!(got, vec!["(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;".to_string()]);
    }

    #[test]
    fn array_of_type_parameter_erases() {
        let got = descriptors_of("class A<T> { T[] all; }");
        assert_eq!(got, vec!["[Ljava/lang/Object;".to_string()]);
    }
}
