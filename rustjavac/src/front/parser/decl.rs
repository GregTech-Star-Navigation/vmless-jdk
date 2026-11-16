//! 声明：类/接口/枚举/record/注解、字段、方法、类型。

use super::{ParseError, Parser};
use crate::ast::*;
use crate::lexer::Tok;

impl Parser {
    // ---------- 修饰符 / 类型 ----------

    pub(crate) fn parse_modifiers(&mut self) -> Result<Vec<String>, ParseError> {
        let mut out = Vec::new();
        loop {
            self.skip_annotations()?;
            let m = match &self.cur() {
                Tok::Kw(k) => match *k {
                    "public" | "private" | "protected" | "static" | "final" | "abstract"
                    | "native" | "synchronized" | "transient" | "volatile" | "strictfp"
                    | "default" => Some(*k),
                    _ => None,
                },
                Tok::Ident(s) if s == "non" => None,
                _ => None,
            };
            match m {
                Some(k) => {
                    self.advance();
                    out.push(k.to_string());
                }
                None => {
                    if self.is_ctx("sealed") { self.advance(); out.push("sealed".into()); continue; }
                    // non-sealed
                    if matches!(&self.cur(), Tok::Ident(s) if s == "non") {
                        let save = self.pos;
                        self.advance();
                        if matches!(self.cur(), Tok::Minus) {
                            self.advance();
                            if matches!(&self.cur(), Tok::Ident(s) if s == "sealed") {
                                self.advance();
                                out.push("non-sealed".into());
                                continue;
                            }
                        }
                        self.pos = save;
                    }
                    return Ok(out);
                }
            }
        }
    }

    /// 解析类型但**不吃**数组后缀（`new T[...]` 用）
    pub(crate) fn parse_type_no_array(&mut self) -> Result<Type, ParseError> {
        self.skip_annotations()?;
        let prim = match &self.cur() {
            Tok::Kw(k) => match *k {
                "int" => Some("int"), "long" => Some("long"), "short" => Some("short"),
                "byte" => Some("byte"), "char" => Some("char"), "boolean" => Some("boolean"),
                "float" => Some("float"), "double" => Some("double"), _ => None,
            },
            _ => None,
        };
        if let Some(p) = prim { self.advance(); return Ok(Type::Prim(p)); }
        let mut path = vec![self.expect_ident()?];
        let mut args = Vec::new();
        if matches!(self.cur(), Tok::Lt) { args = self.parse_type_args()?; }
        while matches!(self.cur(), Tok::Dot) {
            self.advance();
            path.push(self.expect_ident()?);
            if matches!(self.cur(), Tok::Lt) { args = self.parse_type_args()?; }
        }
        Ok(Type::Named { path, args })
    }

    pub(crate) fn parse_type(&mut self) -> Result<Type, ParseError> {
        self.skip_annotations()?;
        if self.is_ctx("var") {
            self.advance();
            return Ok(Type::Var);
        }
        let prim = match &self.cur() {
            Tok::Kw(k) => match *k {
                "int" => Some("int"),
                "long" => Some("long"),
                "short" => Some("short"),
                "byte" => Some("byte"),
                "char" => Some("char"),
                "boolean" => Some("boolean"),
                "float" => Some("float"),
                "double" => Some("double"),
                "void" => Some("void"),
                _ => None,
            },
            _ => None,
        };
        let mut ty = if let Some(p) = prim {
            self.advance();
            Type::Prim(p)
        } else {
            let mut path = vec![self.expect_ident()?];
            let mut args = Vec::new();
            if matches!(self.cur(), Tok::Lt) {
                args = self.parse_type_args()?;
            }
            while matches!(self.cur(), Tok::Dot) {
                self.advance();
                path.push(self.expect_ident()?);
                if matches!(self.cur(), Tok::Lt) {
                    args = self.parse_type_args()?;
                }
            }
            Type::Named { path, args }
        };
        while matches!(self.cur(), Tok::LBracket) {
            self.advance();
            self.expect(&Tok::RBracket)?;
            ty = Type::Array(Box::new(ty));
        }
        Ok(ty)
    }

    pub(crate) fn parse_type_args(&mut self) -> Result<Vec<Type>, ParseError> {
        self.expect(&Tok::Lt)?;
        let mut out = Vec::new();
        if matches!(self.cur(), Tok::Gt) {
            self.advance();
            return Ok(out);
        }
        loop {
            self.skip_annotations()?;
            // 通配符
            if self.eat(&Tok::Question) {
                if self.eat_kw("extends") || self.eat_kw("super") {
                    out.push(self.parse_type()?);
                } else {
                    out.push(Type::Named { path: vec!["?".into()], args: vec![] });
                }
            } else {
                out.push(self.parse_type()?);
            }
            if self.eat(&Tok::Comma) {
                continue;
            }
            // 关闭：可能是 Gt 或 Shr/Ushr（`>>`）
            match self.cur() {
                Tok::Gt => {
                    self.advance();
                    return Ok(out);
                }
                Tok::Shr => {
                    self.split_current(Tok::Gt, Tok::Gt);
                    return Ok(out);
                }
                Tok::Ushr => {
                    self.split_current(Tok::Gt, Tok::Shr);
                    return Ok(out);
                }
                other => return Err(self.err(format!("类型参数未闭合，实际 {other}"))),
            }
        }
    }

    pub(crate) fn parse_type_params(&mut self) -> Result<Vec<String>, ParseError> {
        let mut out = Vec::new();
        if !matches!(self.cur(), Tok::Lt) {
            return Ok(out);
        }
        self.expect(&Tok::Lt)?;
        loop {
            self.skip_annotations()?;
            out.push(self.expect_ident()?);
            if self.eat_kw("extends") {
                let _ = self.parse_type()?;
                while self.eat(&Tok::Amp) {
                    let _ = self.parse_type()?;
                }
            }
            if self.eat(&Tok::Comma) {
                continue;
            }
            match self.cur() {
                Tok::Gt => {
                    self.advance();
                    return Ok(out);
                }
                Tok::Shr => {
                    self.split_current(Tok::Gt, Tok::Gt);
                    return Ok(out);
                }
                other => return Err(self.err(format!("类型参数未闭合，实际 {other}"))),
            }
        }
    }

    // ---------- 类型声明 ----------

    pub(crate) fn parse_type_decl(&mut self) -> Result<TypeDecl, ParseError> {
        let modifiers = self.parse_modifiers()?;

        let kind = if self.eat(&Tok::At) {
            self.expect_kw("interface")?;
            TypeKind::Annotation
        } else if self.is_kw("class") {
            self.advance();
            TypeKind::Class
        } else if self.is_kw("interface") {
            self.advance();
            TypeKind::Interface
        } else if self.is_kw("enum") {
            self.advance();
            TypeKind::Enum
        } else if self.is_ctx("record") {
            self.advance();
            TypeKind::Record
        } else {
            return Err(self.err(format!("期望类型声明，实际 {}", self.cur())));
        };

        let name = self.expect_ident()?;
        let type_params = self.parse_type_params()?;

        // record 的头部参数列表
        let mut record_params = Vec::new();
        if kind == TypeKind::Record && matches!(self.cur(), Tok::LParen) {
            self.advance();
            if !matches!(self.cur(), Tok::RParen) {
                loop {
                    self.skip_annotations()?;
                    let ty = self.parse_type()?;
                    let pname = self.expect_ident()?;
                    let varargs = self.eat(&Tok::Ellipsis);
                    record_params.push(Param { ty, name: pname, varargs });
                    if self.eat(&Tok::Comma) {
                        continue;
                    }
                    break;
                }
            }
            self.expect(&Tok::RParen)?;
        }

        let mut extends = Vec::new();
        let mut implements = Vec::new();
        loop {
            if self.eat_kw("extends") {
                extends.push(self.parse_type()?);
                while self.eat(&Tok::Comma) {
                    extends.push(self.parse_type()?);
                }
            } else if self.eat_kw("implements") {
                implements.push(self.parse_type()?);
                while self.eat(&Tok::Comma) {
                    implements.push(self.parse_type()?);
                }
            } else if self.eat_ctx("permits") {
                let _ = self.parse_type()?;
                while self.eat(&Tok::Comma) {
                    let _ = self.parse_type()?;
                }
            } else {
                break;
            }
        }

        let mut members = Vec::new();
        if self.eat(&Tok::Semi) {
            // 无体的 interface/注解声明
            return Ok(TypeDecl { kind, name, modifiers, type_params, extends, implements, members });
        }
        self.expect(&Tok::LBrace)?;

        // 枚举常量
        if kind == TypeKind::Enum {
            self.skip_annotations()?;
            if !matches!(self.cur(), Tok::RBrace | Tok::Semi) {
                loop {
                    self.skip_annotations()?;
                    let _cname = self.expect_ident()?;
                    if matches!(self.cur(), Tok::LParen) {
                        self.skip_balanced(Tok::LParen, Tok::RParen)?;
                    }
                    if self.eat(&Tok::LBrace) {
                        // 带体的常量：跳过实现
                        self.skip_to_matching(Tok::RBrace)?;
                    }
                    if self.eat(&Tok::Comma) {
                        self.skip_annotations()?;
                        if matches!(self.cur(), Tok::Semi | Tok::RBrace) {
                            break;
                        }
                        continue;
                    }
                    break;
                }
            }
            self.eat(&Tok::Semi);
        }

        while !matches!(self.cur(), Tok::RBrace | Tok::Eof) {
            if self.eat(&Tok::Semi) {
                continue;
            }
            members.push(self.parse_member()?);
        }
        self.expect(&Tok::RBrace)?;

        // record 的隐式字段
        for p in record_params {
            members.push(Member::Field(FieldDecl {
                modifiers: vec!["private".into(), "final".into()],
                ty: p.ty,
                name: p.name,
                init: None,
            }));
        }

        Ok(TypeDecl { kind, name, modifiers, type_params, extends, implements, members })
    }

    pub(crate) fn skip_to_matching(&mut self, close: Tok) -> Result<(), ParseError> {
        let mut depth = 1;
        while depth > 0 {
            let t = self.advance();
            if t == Tok::Eof {
                return Err(self.err("块未闭合".into()));
            }
            if t == close {
                depth -= 1;
            } else if t == Tok::LBrace {
                depth += 1;
            }
        }
        Ok(())
    }

    pub(crate) fn parse_member(&mut self) -> Result<Member, ParseError> {
        let modifiers = self.parse_modifiers()?;

        if self.is_kw("class") || self.is_kw("interface") || self.is_kw("enum") || self.is_kw("record")
            || (matches!(self.cur(), Tok::At))
        {
            let mut d = self.parse_type_decl()?;
            d.modifiers = modifiers;
            return Ok(Member::Type(d));
        }
        // 构造器
        if matches!(self.cur(), Tok::LBrace) {
            let is_static = modifiers.iter().any(|m| m == "static");
            let body = self.parse_block()?;
            return Ok(Member::Init { is_static, body });
        }
        // 泛型方法或构造器的类型参数
        let tparams = if matches!(self.cur(), Tok::Lt) { self.parse_type_params()? } else { vec![] };

        // 构造器：Ident(
        if let Tok::Ident(n) = self.cur() {
            if matches!(self.toks.get(self.pos + 1).map(|t| &t.tok), Some(Tok::LParen)) {
                self.advance();
                let params = self.parse_params()?;
                let throws = self.parse_throws()?;
                let body = if matches!(self.cur(), Tok::LBrace) {
                    self.expect(&Tok::LBrace)?;
                    let first = self.toks.get(self.pos).map(|t| t.tok.clone());
                    if matches!(first, Some(Tok::Kw("this")) | Some(Tok::Kw("super"))) {
                        // 构造器调用语句
                        let mut stmts = Vec::new();
                        let e = self.parse_statement()?;
                        stmts.push(e);
                        while !matches!(self.cur(), Tok::RBrace | Tok::Eof) {
                            stmts.push(self.parse_statement()?);
                        }
                        self.expect(&Tok::RBrace)?;
                        Stmt::Block(stmts)
                    } else {
                        let mut stmts = Vec::new();
                        while !matches!(self.cur(), Tok::RBrace | Tok::Eof) {
                            stmts.push(self.parse_statement()?);
                        }
                        self.expect(&Tok::RBrace)?;
                        Stmt::Block(stmts)
                    }
                } else {
                    self.expect(&Tok::Semi)?;
                    return Ok(Member::Method(MethodDecl {
                        modifiers,
                        type_params: tparams,
                        ret: None,
                        name: n,
                        params,
                        throws,
                        body: None,
                    }));
                };
                return Ok(Member::Method(MethodDecl {
                    modifiers,
                    type_params: tparams,
                    ret: None,
                    name: n,
                    params,
                    throws,
                    body: Some(body),
                }));
            }
        }

        let ty = self.parse_type()?;
        let name = self.expect_ident()?;
        if matches!(self.cur(), Tok::LParen) {
            let params = self.parse_params()?;
            while matches!(self.cur(), Tok::LBracket) {
                self.advance();
                self.expect(&Tok::RBracket)?;
            }
            let throws = self.parse_throws()?;
            let body = if self.eat(&Tok::Semi) {
                None
            } else if self.eat_kw("default") {
                let _ = self.parse_expr()?;
                self.expect(&Tok::Semi)?;
                None
            } else {
                Some(self.parse_block()?)
            };
            return Ok(Member::Method(MethodDecl {
                modifiers,
                type_params: tparams,
                ret: Some(ty),
                name,
                params,
                throws,
                body,
            }));
        }

        // 字段的 C 风格数组声明 `int fields[]`
        while matches!(self.cur(), Tok::LBracket) {
            self.advance();
            self.expect(&Tok::RBracket)?;
        }
        // 字段（可能有多个声明符）
        let mut init = None;
        if self.eat(&Tok::Assign) {
            init = Some(self.parse_array_or_expr()?);
        }
        let first = Member::Field(FieldDecl { modifiers: modifiers.clone(), ty: ty.clone(), name, init });
        if self.eat(&Tok::Comma) {
            // 简化：其余声明符忽略初始化表达式，但必须消费
            loop {
                let _n = self.expect_ident()?;
                while matches!(self.cur(), Tok::LBracket) {
                    self.advance();
                    self.expect(&Tok::RBracket)?;
                }
                if self.eat(&Tok::Assign) {
                    let _ = self.parse_array_or_expr()?;
                }
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(&Tok::Semi)?;
        Ok(first)
    }

    pub(crate) fn parse_params(&mut self) -> Result<Vec<Param>, ParseError> {
        self.expect(&Tok::LParen)?;
        let mut out = Vec::new();
        if self.eat(&Tok::RParen) {
            return Ok(out);
        }
        loop {
            self.skip_annotations()?;
            let _ = self.eat_kw("final");
            let ty = self.parse_type()?;
            // 接收者参数 this
            if self.is_kw("this") {
                self.advance();
                if self.eat(&Tok::Comma) {
                    continue;
                }
                break;
            }
            let varargs = self.eat(&Tok::Ellipsis);
            let name = self.expect_ident()?;
            while matches!(self.cur(), Tok::LBracket) {
                self.advance();
                self.expect(&Tok::RBracket)?;
            }
            out.push(Param { ty, name, varargs });
            if self.eat(&Tok::Comma) {
                continue;
            }
            break;
        }
        self.expect(&Tok::RParen)?;
        Ok(out)
    }

    pub(crate) fn parse_throws(&mut self) -> Result<Vec<Type>, ParseError> {
        let mut out = Vec::new();
        if self.eat_kw("throws") {
            out.push(self.parse_type()?);
            while self.eat(&Tok::Comma) {
                out.push(self.parse_type()?);
            }
        }
        Ok(out)
    }
}
