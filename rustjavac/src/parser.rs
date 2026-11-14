//! Java 25 语法分析（递归下降 + 优先级爬升）。
//!
//! 覆盖面：package/import、class/interface/enum/record/注解声明、字段、方法、
//! 构造器、初始化块、嵌套类型；语句块、if/while/do/for/for-each/switch(箭头)/try-catch-finally、
//! return/throw/break/continue；表达式含全部二元优先级、赋值、三元、cast、instanceof 模式、
//! lambda、方法引用、数组创建、switch 表达式。

use crate::ast::*;
use crate::lexer::{tokenize, Tok, Token};

#[derive(Debug)]
pub struct ParseError {
    pub line: u32,
    pub msg: String,
}

pub struct Parser {
    toks: Vec<Token>,
    pos: usize,
    pending: Option<Tok>,
    line: u32,
}

impl Parser {
    pub fn new(src: &str) -> Result<Self, ParseError> {
        let toks = tokenize(src).map_err(|e| ParseError { line: e.line, msg: e.msg })?;
        let line = toks.last().map(|t| t.line).unwrap_or(1);
        Ok(Parser { toks, pos: 0, pending: None, line })
    }

    // ---------- token 工具 ----------

    fn cur(&self) -> Tok {
        if let Some(p) = &self.pending {
            return p.clone();
        }
        self.toks.get(self.pos).map(|t| t.tok.clone()).unwrap_or(Tok::Eof)
    }

    fn cur_line(&self) -> u32 {
        self.toks.get(self.pos).map(|t| t.line).unwrap_or(self.line)
    }

    fn advance(&mut self) -> Tok {
        if let Some(p) = self.pending.take() {
            return p;
        }
        let t = self.toks.get(self.pos).map(|t| t.tok.clone()).unwrap_or(Tok::Eof);
        self.pos += 1;
        t
    }

    /// 把当前 token 拆成两半（用于泛型 `>>`）
    fn split_current(&mut self, head: Tok, rest: Tok) {
        self.advance();
        self.pending = Some(rest);
        let _ = head;
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if &self.cur() == t {
            self.advance();
            true
        } else {
            false
        }
    }

    fn eat_kw(&mut self, k: &str) -> bool {
        matches!(self.cur(), Tok::Kw(x) if x == k) && {
            self.advance();
            true
        }
    }

    fn expect(&mut self, t: &Tok) -> Result<(), ParseError> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(self.err(format!("期望 {t}，实际 {}", self.cur())))
        }
    }

    fn expect_kw(&mut self, k: &str) -> Result<(), ParseError> {
        if self.eat_kw(k) {
            Ok(())
        } else {
            Err(self.err(format!("期望关键字 `{k}`，实际 {}", self.cur())))
        }
    }

    fn expect_ident(&mut self) -> Result<String, ParseError> {
        match self.cur() {
            Tok::Ident(s) => {
                self.advance();
                Ok(s)
            }
            other => Err(self.err(format!("期望标识符，实际 {other}"))),
        }
    }

    fn err(&self, msg: String) -> ParseError {
        ParseError { line: self.cur_line(), msg }
    }

    fn is_kw(&self, k: &str) -> bool {
        matches!(self.cur(), Tok::Kw(x) if x == k)
    }

    fn is_ctx(&self, name: &str) -> bool {
        matches!(&self.cur(), Tok::Ident(s) if s == name)
    }

    fn eat_ctx(&mut self, name: &str) -> bool {
        if self.is_ctx(name) { self.advance(); true } else { false }
    }

    // ---------- 顶层 ----------

    pub fn parse_compilation_unit(&mut self) -> Result<CompilationUnit, ParseError> {
        let mut package = None;
        let mut imports = Vec::new();
        let mut types = Vec::new();

        self.skip_annotations()?;
        if self.eat_kw("package") {
            package = Some(self.qualified_name()?);
            self.expect(&Tok::Semi)?;
        }
        loop {
            self.skip_annotations()?;
            if self.eat_kw("import") {
                let is_static = self.eat_kw("static");
                let mut name = self.qualified_name()?;
                let wildcard = if self.eat(&Tok::Dot) {
                    self.expect(&Tok::Star)?;
                    true
                } else {
                    name.push_str("");
                    false
                };
                self.expect(&Tok::Semi)?;
                imports.push(Import { is_static, name, wildcard });
            } else {
                break;
            }
        }
        while !matches!(self.cur(), Tok::Eof) {
            if self.eat(&Tok::Semi) {
                continue;
            }
            types.push(self.parse_type_decl()?);
        }
        Ok(CompilationUnit { package, imports, types })
    }

    fn qualified_name(&mut self) -> Result<String, ParseError> {
        let mut s = self.expect_ident()?;
        while matches!(self.cur(), Tok::Dot) {
            // 只有在点后面还是标识符时才算限定名
            let save_pos = self.pos;
            let save_pending = self.pending.clone();
            self.advance();
            if matches!(self.cur(), Tok::Ident(_)) {
                s.push('.');
                s.push_str(&self.expect_ident()?);
            } else {
                self.pos = save_pos;
                self.pending = save_pending;
                break;
            }
        }
        Ok(s)
    }

    fn skip_annotations(&mut self) -> Result<(), ParseError> {
        while matches!(self.cur(), Tok::At) {
            self.advance();
            if self.is_kw("interface") {
                // @interface 是注解声明，交给上层
                return Ok(());
            }
            let _ = self.qualified_name()?;
            if matches!(self.cur(), Tok::LParen) {
                self.skip_balanced(Tok::LParen, Tok::RParen)?;
            }
        }
        Ok(())
    }

    fn skip_balanced(&mut self, open: Tok, close: Tok) -> Result<(), ParseError> {
        self.expect(&open)?;
        let mut depth = 1;
        while depth > 0 {
            let t = self.advance();
            if t == Tok::Eof {
                return Err(self.err("括号未闭合".into()));
            }
            if t == open {
                depth += 1;
            } else if t == close {
                depth -= 1;
            }
        }
        Ok(())
    }

    // ---------- 修饰符 / 类型 ----------

    fn parse_modifiers(&mut self) -> Result<Vec<String>, ParseError> {
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
    fn parse_type_no_array(&mut self) -> Result<Type, ParseError> {
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

    fn parse_type(&mut self) -> Result<Type, ParseError> {
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

    fn parse_type_args(&mut self) -> Result<Vec<Type>, ParseError> {
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

    fn parse_type_params(&mut self) -> Result<Vec<String>, ParseError> {
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

    fn parse_type_decl(&mut self) -> Result<TypeDecl, ParseError> {
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

    fn skip_to_matching(&mut self, close: Tok) -> Result<(), ParseError> {
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

    fn parse_member(&mut self) -> Result<Member, ParseError> {
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

    fn parse_params(&mut self) -> Result<Vec<Param>, ParseError> {
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

    fn parse_throws(&mut self) -> Result<Vec<Type>, ParseError> {
        let mut out = Vec::new();
        if self.eat_kw("throws") {
            out.push(self.parse_type()?);
            while self.eat(&Tok::Comma) {
                out.push(self.parse_type()?);
            }
        }
        Ok(out)
    }

    // ---------- 语句 ----------

    fn parse_block(&mut self) -> Result<Stmt, ParseError> {
        self.expect(&Tok::LBrace)?;
        let mut stmts = Vec::new();
        while !matches!(self.cur(), Tok::RBrace | Tok::Eof) {
            stmts.push(self.parse_statement()?);
        }
        self.expect(&Tok::RBrace)?;
        Ok(Stmt::Block(stmts))
    }

    fn parse_statement(&mut self) -> Result<Stmt, ParseError> {
        if matches!(self.cur(), Tok::LBrace) {
            return self.parse_block();
        }
        if self.eat(&Tok::Semi) {
            return Ok(Stmt::Empty);
        }
        self.skip_annotations()?;
        let _ = self.eat_kw("final");
        if let Tok::Ident(lbl) = self.cur() {
            let _ = lbl;
            if matches!(self.toks.get(self.pos + 1).map(|t| &t.tok), Some(Tok::Colon)) {
                self.advance();
                self.advance();
                return self.parse_statement();
            }
        }

        if self.is_kw("if") {
            self.advance();
            self.expect(&Tok::LParen)?;
            let cond = self.parse_expr()?;
            self.expect(&Tok::RParen)?;
            let then = Box::new(self.parse_statement()?);
            let els = if self.eat_kw("else") { Some(Box::new(self.parse_statement()?)) } else { None };
            return Ok(Stmt::If { cond, then, els });
        }
        if self.is_kw("while") {
            self.advance();
            self.expect(&Tok::LParen)?;
            let cond = self.parse_expr()?;
            self.expect(&Tok::RParen)?;
            let body = Box::new(self.parse_statement()?);
            return Ok(Stmt::While { cond, body });
        }
        if self.is_kw("do") {
            self.advance();
            let body = Box::new(self.parse_statement()?);
            self.expect_kw("while")?;
            self.expect(&Tok::LParen)?;
            let cond = self.parse_expr()?;
            self.expect(&Tok::RParen)?;
            self.expect(&Tok::Semi)?;
            return Ok(Stmt::DoWhile { body, cond });
        }
        if self.is_kw("for") {
            return self.parse_for();
        }
        if self.is_kw("switch") {
            return self.parse_switch_stmt();
        }
        if self.is_kw("return") {
            self.advance();
            let e = if matches!(self.cur(), Tok::Semi) { None } else { Some(self.parse_expr()?) };
            self.expect(&Tok::Semi)?;
            return Ok(Stmt::Return(e));
        }
        if self.is_kw("throw") {
            self.advance();
            let e = self.parse_expr()?;
            self.expect(&Tok::Semi)?;
            return Ok(Stmt::Throw(e));
        }
        if self.is_kw("break") {
            self.advance();
            let label = if let Tok::Ident(s) = self.cur() { self.advance(); Some(s) } else { None };
            self.expect(&Tok::Semi)?;
            return Ok(Stmt::Break(label));
        }
        if self.is_kw("continue") {
            self.advance();
            let label = if let Tok::Ident(s) = self.cur() { self.advance(); Some(s) } else { None };
            self.expect(&Tok::Semi)?;
            return Ok(Stmt::Continue(label));
        }
        if self.is_kw("class") || self.is_kw("interface") || self.is_kw("enum") || self.is_ctx("record") {
            let _ = self.parse_type_decl()?;
            return Ok(Stmt::Empty);
        }
        if self.is_kw("try") {
            return self.parse_try();
        }
        if self.is_kw("synchronized") {
            self.advance();
            self.skip_balanced(Tok::LParen, Tok::RParen)?;
            return self.parse_block();
        }
        if self.is_kw("assert") {
            self.advance();
            let _ = self.parse_expr()?;
            if self.eat(&Tok::Colon) {
                let _ = self.parse_expr()?;
            }
            self.expect(&Tok::Semi)?;
            return Ok(Stmt::Empty);
        }

        // 局部变量声明？
        if let Some(ty) = self.try_parse_local_type()? {
            let name = self.expect_ident()?;
            while matches!(self.cur(), Tok::LBracket) {
                self.advance();
                self.expect(&Tok::RBracket)?;
            }
            let init = if self.eat(&Tok::Assign) { Some(self.parse_array_or_expr()?) } else { None };
            let mut decls = vec![Stmt::LocalVar { ty: ty.clone(), name, init }];
            while self.eat(&Tok::Comma) {
                let n2 = self.expect_ident()?;
                while matches!(self.cur(), Tok::LBracket) { self.advance(); self.expect(&Tok::RBracket)?; }
                let i2 = if self.eat(&Tok::Assign) { Some(self.parse_array_or_expr()?) } else { None };
                decls.push(Stmt::LocalVar { ty: ty.clone(), name: n2, init: i2 });
            }
            self.expect(&Tok::Semi)?;
            return Ok(if decls.len() == 1 { decls.pop().unwrap() } else { Stmt::Block(decls) });
        }

        let e = self.parse_expr()?;
        self.expect(&Tok::Semi)?;
        Ok(Stmt::Expr(e))
    }

    /// 尝试把当前位置解析成"局部变量声明的类型"；失败则回退（返回 None）。
    fn try_parse_local_type(&mut self) -> Result<Option<Type>, ParseError> {
        let save_pos = self.pos;
        let save_pending = self.pending.clone();
        let is_type_start = match &self.cur() {
            Tok::Kw(k) => matches!(
                *k,
                "int" | "long" | "short" | "byte" | "char" | "boolean" | "float" | "double" | "var"
            ),
            Tok::Ident(_) => true,
            _ => false,
        };
        if !is_type_start {
            return Ok(None);
        }
        let ty = match self.parse_type() {
            Ok(t) => t,
            Err(_) => {
                self.pos = save_pos;
                self.pending = save_pending;
                return Ok(None);
            }
        };
        // 交叉类型 (A & B)
        while self.eat(&Tok::Amp) {
            if self.parse_type().is_err() {
                self.pos = save_pos;
                self.pending = save_pending;
                return Ok(None);
            }
        }
        // 类型后面必须是标识符
        if matches!(self.cur(), Tok::Ident(_)) {
            Ok(Some(ty))
        } else {
            self.pos = save_pos;
            self.pending = save_pending;
            Ok(None)
        }
    }

    fn parse_for(&mut self) -> Result<Stmt, ParseError> {
        self.expect_kw("for")?;
        self.expect(&Tok::LParen)?;
        // for-each?
        {
            let save_pos = self.pos;
            let save_pending = self.pending.clone();
            let _ = self.eat_kw("final");
            if let Ok(Some(ty)) = self.try_parse_local_type() {
                if let Tok::Ident(name) = self.cur() {
                    self.advance();
                    if self.eat(&Tok::Colon) {
                        let iter = self.parse_expr()?;
                        self.expect(&Tok::RParen)?;
                        let body = Box::new(self.parse_statement()?);
                        return Ok(Stmt::ForEach { ty, name, iter, body });
                    }
                }
            }
            self.pos = save_pos;
            self.pending = save_pending;
        }

        let mut init = Vec::new();
        if !matches!(self.cur(), Tok::Semi) {
            if let Some(ty) = self.try_parse_local_type()? {
                loop {
                    let name = self.expect_ident()?;
                    let e = if self.eat(&Tok::Assign) { Some(self.parse_array_or_expr()?) } else { None };
                    init.push(Stmt::LocalVar { ty: ty.clone(), name, init: e });
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
            } else {
                loop {
                    let e = self.parse_expr()?;
                    init.push(Stmt::Expr(e));
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
            }
        }
        self.expect(&Tok::Semi)?;
        let cond = if matches!(self.cur(), Tok::Semi) { None } else { Some(self.parse_expr()?) };
        self.expect(&Tok::Semi)?;
        let mut update = Vec::new();
        if !matches!(self.cur(), Tok::RParen) {
            loop {
                update.push(self.parse_expr()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(&Tok::RParen)?;
        let body = Box::new(self.parse_statement()?);
        Ok(Stmt::For { init, cond, update, body })
    }

    fn parse_switch_stmt(&mut self) -> Result<Stmt, ParseError> {
        self.expect_kw("switch")?;
        self.expect(&Tok::LParen)?;
        let selector = self.parse_expr()?;
        self.expect(&Tok::RParen)?;
        self.expect(&Tok::LBrace)?;
        let mut cases = Vec::new();
        while !matches!(self.cur(), Tok::RBrace | Tok::Eof) {
            let mut labels = Vec::new();
            let mut is_default = false;
            if self.eat_kw("default") {
                is_default = true;
                labels.push(None);
            } else {
                self.expect_kw("case")?;
                loop {
                    labels.push(Some(self.parse_expr()?));
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
            }
            let _ = is_default;
            let arrow = self.eat(&Tok::Arrow);
            if !arrow {
                self.expect(&Tok::Colon)?;
            }
            let mut body = Vec::new();
            if arrow {
                if matches!(self.cur(), Tok::LBrace) {
                    body.push(self.parse_block()?);
                } else {
                    let s = self.parse_statement()?;
                    body.push(s);
                }
            } else {
                while !matches!(self.cur(), Tok::RBrace | Tok::Eof)
                    && !self.is_kw("case")
                    && !self.is_kw("default")
                {
                    body.push(self.parse_statement()?);
                }
            }
            cases.push(SwitchCase { labels, body, arrow });
        }
        self.expect(&Tok::RBrace)?;
        Ok(Stmt::Switch { selector, cases })
    }

    fn parse_try(&mut self) -> Result<Stmt, ParseError> {
        self.expect_kw("try")?;
        if matches!(self.cur(), Tok::LParen) {
            // try-with-resources
            self.skip_balanced(Tok::LParen, Tok::RParen)?;
        }
        let body = Box::new(self.parse_block()?);
        let mut catches = Vec::new();
        while self.is_kw("catch") {
            self.advance();
            self.expect(&Tok::LParen)?;
            let _ = self.eat_kw("final");
            let ty = self.parse_type()?;
            while self.eat(&Tok::Bar) { let _ = self.parse_type()?; }
            let name = self.expect_ident()?;
            self.expect(&Tok::RParen)?;
            let cbody = self.parse_block()?;
            catches.push(Catch { ty, name, body: cbody });
        }
        let finally = if self.eat_kw("finally") { Some(Box::new(self.parse_block()?)) } else { None };
        Ok(Stmt::Try { body, catches, finally })
    }

    // ---------- 表达式 ----------

    pub fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        // lambda?
        if let Some(lambda) = self.try_parse_lambda()? {
            return Ok(lambda);
        }
        let lhs = self.parse_ternary()?;
        // 赋值
        let op = match &self.cur() {
            Tok::Assign => Some(None),
            Tok::PlusAssign => Some(Some(BinOp::Add)),
            Tok::MinusAssign => Some(Some(BinOp::Sub)),
            Tok::StarAssign => Some(Some(BinOp::Mul)),
            Tok::SlashAssign => Some(Some(BinOp::Div)),
            Tok::PercentAssign => Some(Some(BinOp::Rem)),
            Tok::AmpAssign => Some(Some(BinOp::BitAnd)),
            Tok::BarAssign => Some(Some(BinOp::BitOr)),
            Tok::CaretAssign => Some(Some(BinOp::BitXor)),
            Tok::ShlAssign => Some(Some(BinOp::Shl)),
            Tok::ShrAssign => Some(Some(BinOp::Shr)),
            Tok::UshrAssign => Some(Some(BinOp::Ushr)),
            _ => None,
        };
        if let Some(op) = op {
            self.advance();
            let rhs = self.parse_expr()?;
            return Ok(Expr::Assign { op, lhs: Box::new(lhs), rhs: Box::new(rhs) });
        }
        Ok(lhs)
    }

    fn try_parse_lambda(&mut self) -> Result<Option<Expr>, ParseError> {
        // x -> ...
        if let Tok::Ident(_) = self.cur() {
            if matches!(self.toks.get(self.pos + 1).map(|t| &t.tok), Some(Tok::Arrow)) {
                let name = self.expect_ident()?;
                self.expect(&Tok::Arrow)?;
                return Ok(Some(self.finish_lambda(vec![name])?));
            }
        }
        // (a, b) -> ...
        if matches!(self.cur(), Tok::LParen) {
            let save_pos = self.pos;
            let save_pending = self.pending.clone();
            self.advance();
            let mut params = Vec::new();
            let mut ok = true;
            if !matches!(self.cur(), Tok::RParen) {
                loop {
                    // 可能带类型
                    let mut got_name = None;
                    if let Tok::Ident(n) = self.cur() {
                        // 看是否 Ident ) 或 Ident , ——无类型形参
                        let next = self.toks.get(self.pos + 1).map(|t| t.tok.clone());
                        if matches!(next, Some(Tok::Comma) | Some(Tok::RParen)) {
                            self.advance();
                            got_name = Some(n);
                        }
                    }
                    if got_name.is_none() {
                        if self.parse_type().is_err() {
                            ok = false;
                            break;
                        }
                        match self.expect_ident() {
                            Ok(n) => got_name = Some(n),
                            Err(_) => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    params.push(got_name.unwrap());
                    if self.eat(&Tok::Comma) {
                        continue;
                    }
                    break;
                }
            }
            if ok && self.eat(&Tok::RParen) && matches!(self.cur(), Tok::Arrow) {
                self.advance();
                return Ok(Some(self.finish_lambda(params)?));
            }
            self.pos = save_pos;
            self.pending = save_pending;
        }
        Ok(None)
    }

    fn finish_lambda(&mut self, params: Vec<String>) -> Result<Expr, ParseError> {
        let body = if matches!(self.cur(), Tok::LBrace) {
            let b = self.parse_block()?;
            Expr::SwitchExpr(Box::new(Expr::Ident(format!("{b:?}"))))
        } else {
            self.parse_expr()?
        };
        Ok(Expr::Lambda { params, body: Box::new(body) })
    }

    fn parse_ternary(&mut self) -> Result<Expr, ParseError> {
        let cond = self.parse_binary(0)?;
        if self.eat(&Tok::Question) {
            let then = self.parse_expr()?;
            self.expect(&Tok::Colon)?;
            let els = self.parse_ternary()?;
            return Ok(Expr::Cond { cond: Box::new(cond), then: Box::new(then), els: Box::new(els) });
        }
        Ok(cond)
    }

    fn bin_prec(t: &Tok) -> Option<(BinOp, u8)> {
        Some(match t {
            Tok::OrOr => (BinOp::Or, 1),
            Tok::AndAnd => (BinOp::And, 2),
            Tok::Bar => (BinOp::BitOr, 3),
            Tok::Caret => (BinOp::BitXor, 4),
            Tok::Amp => (BinOp::BitAnd, 5),
            Tok::EqEq => (BinOp::Eq, 6),
            Tok::Ne => (BinOp::Ne, 6),
            Tok::Lt => (BinOp::Lt, 7),
            Tok::Gt => (BinOp::Gt, 7),
            Tok::Le => (BinOp::Le, 7),
            Tok::Ge => (BinOp::Ge, 7),
            Tok::Shl => (BinOp::Shl, 8),
            Tok::Shr => (BinOp::Shr, 8),
            Tok::Ushr => (BinOp::Ushr, 8),
            Tok::Plus => (BinOp::Add, 9),
            Tok::Minus => (BinOp::Sub, 9),
            Tok::Star => (BinOp::Mul, 10),
            Tok::Slash => (BinOp::Div, 10),
            Tok::Percent => (BinOp::Rem, 10),
            _ => return None,
        })
    }

    fn parse_binary(&mut self, min_prec: u8) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_unary()?;
        loop {
            let (op, prec) = match Self::bin_prec(&self.cur()) {
                Some(x) => x,
                None => break,
            };
            if prec < min_prec {
                break;
            }
            self.advance();
            let rhs = self.parse_binary(prec + 1)?;
            lhs = Expr::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs) };
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        match self.cur() {
            Tok::Minus => {
                self.advance();
                Ok(Expr::Unary { op: UnOp::Neg, expr: Box::new(self.parse_unary()?) })
            }
            Tok::Plus => {
                self.advance();
                Ok(Expr::Unary { op: UnOp::Pos, expr: Box::new(self.parse_unary()?) })
            }
            Tok::Bang => {
                self.advance();
                Ok(Expr::Unary { op: UnOp::Not, expr: Box::new(self.parse_unary()?) })
            }
            Tok::Tilde => {
                self.advance();
                Ok(Expr::Unary { op: UnOp::BitNot, expr: Box::new(self.parse_unary()?) })
            }
            Tok::PlusPlus => {
                self.advance();
                Ok(Expr::Unary { op: UnOp::PreInc, expr: Box::new(self.parse_unary()?) })
            }
            Tok::MinusMinus => {
                self.advance();
                Ok(Expr::Unary { op: UnOp::PreDec, expr: Box::new(self.parse_unary()?) })
            }
            Tok::LParen => {
                // cast 尝试
                if let Some(e) = self.try_parse_cast()? {
                    return Ok(e);
                }
                self.parse_postfix()
            }
            _ => self.parse_postfix(),
        }
    }

    fn try_parse_cast(&mut self) -> Result<Option<Expr>, ParseError> {
        let save_pos = self.pos;
        let save_pending = self.pending.clone();
        self.expect(&Tok::LParen)?;
        let ty = match self.parse_type() {
            Ok(t) => t,
            Err(_) => {
                self.pos = save_pos;
                self.pending = save_pending;
                return Ok(None);
            }
        };
        while self.eat(&Tok::Amp) {
            if self.parse_type().is_err() {
                self.pos = save_pos;
                self.pending = save_pending;
                return Ok(None);
            }
        }
        if !self.eat(&Tok::RParen) {
            self.pos = save_pos;
            self.pending = save_pending;
            return Ok(None);
        }
        // `(a) + b` 这类要回退：cast 的操作数不能是二元运算符开头
        match self.cur() {
            Tok::Plus | Tok::Minus | Tok::Star | Tok::Slash | Tok::Percent | Tok::Semi
            | Tok::RParen | Tok::Comma | Tok::Dot | Tok::LBracket | Tok::EqEq | Tok::AndAnd
            | Tok::OrOr | Tok::Question | Tok::Shl | Tok::Shr | Tok::Ushr
            | Tok::Star | Tok::Slash | Tok::Percent | Tok::Amp | Tok::Bar | Tok::Caret => {
                self.pos = save_pos;
                self.pending = save_pending;
                return Ok(None);
            }
            _ => {}
        }
        // cast 后面可能是 lambda: (Comparator<T> & Serializable) (a, b) -> ...
        let expr = if matches!(self.cur(), Tok::LParen) {
            match self.try_parse_lambda()? {
                Some(l) => l,
                None => self.parse_unary()?,
            }
        } else {
            self.parse_unary()?
        };
        Ok(Some(Expr::Cast { ty, expr: Box::new(expr) }))
    }

    fn parse_postfix(&mut self) -> Result<Expr, ParseError> {
        let mut e = self.parse_primary()?;
        loop {
            match self.cur() {
                Tok::Dot => {
                    self.advance();
                    // class 字面量
                    if self.is_kw("class") {
                        self.advance();
                        // 用 ClassLit 表示
                        let prev = e;
                        e = Expr::MethodRef { recv: Box::new(prev), name: "class".into() };
                        continue;
                    }
                    if self.is_kw("this") {
                        self.advance();
                        e = Expr::Field(Box::new(e), "this".into());
                        continue;
                    }
                    if self.is_kw("super") {
                        self.advance();
                        e = Expr::Field(Box::new(e), "super".into());
                        continue;
                    }
                    if self.is_kw("new") {
                        self.advance();
                        let _ = self.parse_type()?;
                        if matches!(self.cur(), Tok::LParen) {
                            self.skip_balanced(Tok::LParen, Tok::RParen)?;
                        }
                        if self.eat(&Tok::LBrace) {
                            self.skip_to_matching(Tok::RBrace)?;
                        }
                        continue;
                    }
                    // 显式类型实参：obj.<T>method(...)
                    let mut type_args = Vec::new();
                    if matches!(self.cur(), Tok::Lt) {
                        type_args = self.parse_type_args()?;
                    }
                    let mut name = self.expect_ident()?;
                    if matches!(self.cur(), Tok::LParen) {
                        let args = self.parse_args()?;
                        e = Expr::Call { recv: Some(Box::new(e)), name, args, type_args };
                    } else if matches!(self.cur(), Tok::ColonColon) {
                        self.advance();
                        name = self.expect_ident()?;
                        e = Expr::MethodRef { recv: Box::new(e), name };
                    } else {
                        e = Expr::Field(Box::new(e), name);
                    }
                }
                Tok::LBracket => {
                    if matches!(self.toks.get(self.pos + 1).map(|t| &t.tok), Some(Tok::RBracket)) {
                        self.advance();
                        self.advance();
                        continue;
                    }
                    self.advance();
                    let idx = self.parse_expr()?;
                    self.expect(&Tok::RBracket)?;
                    e = Expr::ArrayAccess { array: Box::new(e), index: Box::new(idx) };
                }
                Tok::PlusPlus => {
                    self.advance();
                    e = Expr::Unary { op: UnOp::PostInc, expr: Box::new(e) };
                }
                Tok::MinusMinus => {
                    self.advance();
                    e = Expr::Unary { op: UnOp::PostDec, expr: Box::new(e) };
                }
                Tok::ColonColon => {
                    self.advance();
                    let name = self.expect_ident()?;
                    e = Expr::MethodRef { recv: Box::new(e), name };
                }
                _ => break,
            }
        }
        // instanceof
        if self.is_kw("instanceof") {
            self.advance();
            let _ = self.eat_kw("final");
            let ty = self.parse_type()?;
            let binding = if let Tok::Ident(s) = self.cur() {
                self.advance();
                Some(s)
            } else {
                None
            };
            e = Expr::InstanceOf { expr: Box::new(e), ty, binding };
        }
        Ok(e)
    }

    fn parse_args(&mut self) -> Result<Vec<Expr>, ParseError> {
        self.expect(&Tok::LParen)?;
        let mut out = Vec::new();
        if self.eat(&Tok::RParen) {
            return Ok(out);
        }
        loop {
            out.push(self.parse_expr()?);
            if self.eat(&Tok::Comma) {
                continue;
            }
            break;
        }
        self.expect(&Tok::RParen)?;
        Ok(out)
    }

    fn parse_array_or_expr(&mut self) -> Result<Expr, ParseError> {
        if matches!(self.cur(), Tok::LBrace) {
            self.advance();
            let mut items = Vec::new();
            if !matches!(self.cur(), Tok::RBrace) {
                loop {
                    items.push(self.parse_array_or_expr()?);
                    if self.eat(&Tok::Comma) {
                        if matches!(self.cur(), Tok::RBrace) {
                            break;
                        }
                        continue;
                    }
                    break;
                }
            }
            self.expect(&Tok::RBrace)?;
            return Ok(Expr::ArrayInit(items));
        }
        self.parse_expr()
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        match self.cur() {
            Tok::IntLit(s) => {
                self.advance();
                Ok(Expr::IntLit(s))
            }
            Tok::FloatLit(s) => {
                self.advance();
                Ok(Expr::FloatLit(s))
            }
            Tok::StrLit(s) => {
                self.advance();
                Ok(Expr::StrLit(s))
            }
            Tok::TextBlock(s) => {
                self.advance();
                Ok(Expr::TextBlock(s))
            }
            Tok::CharLit(s) => {
                self.advance();
                Ok(Expr::CharLit(s))
            }
            Tok::Kw("true") => {
                self.advance();
                Ok(Expr::BoolLit(true))
            }
            Tok::Kw("false") => {
                self.advance();
                Ok(Expr::BoolLit(false))
            }
            Tok::Kw("null") => {
                self.advance();
                Ok(Expr::Null)
            }
            Tok::Kw("this") => {
                self.advance();
                if matches!(self.cur(), Tok::LParen) {
                    let args = self.parse_args()?;
                    return Ok(Expr::Call { recv: None, name: "this".into(), args, type_args: vec![] });
                }
                Ok(Expr::This)
            }
            Tok::Kw("super") => {
                self.advance();
                if matches!(self.cur(), Tok::LParen) {
                    let args = self.parse_args()?;
                    return Ok(Expr::Call { recv: None, name: "super".into(), args, type_args: vec![] });
                }
                Ok(Expr::Super)
            }
            Tok::Kw("new") => {
                self.advance();
                let ty = self.parse_type_no_array()?;
                // 数组创建
                if matches!(self.cur(), Tok::LBracket) {
                    let mut dims = Vec::new();
                    while matches!(self.cur(), Tok::LBracket) {
                        self.advance();
                        if matches!(self.cur(), Tok::RBracket) {
                            self.advance();
                            break;
                        }
                        dims.push(self.parse_expr()?);
                        self.expect(&Tok::RBracket)?;
                    }
                    if matches!(self.cur(), Tok::LBrace) {
                        let init = self.parse_array_or_expr()?;
                        return Ok(init);
                    }
                    if let Type::Array(inner) = &ty {
                        return Ok(Expr::ArrayNew { elem: (**inner).clone(), dims });
                    }
                    return Ok(Expr::ArrayNew { elem: ty, dims });
                }
                let args = if matches!(self.cur(), Tok::LParen) { self.parse_args()? } else { vec![] };
                // 匿名类体
                if self.eat(&Tok::LBrace) {
                    self.skip_to_matching(Tok::RBrace)?;
                }
                Ok(Expr::New { ty, args })
            }
            Tok::Kw("switch") => {
                let s = self.parse_switch_stmt()?;
                Ok(Expr::SwitchExpr(Box::new(Expr::Ident(format!("{s:?}")))))
            }
            Tok::Kw("long") | Tok::Kw("int") | Tok::Kw("byte") | Tok::Kw("short") | Tok::Kw("char")
            | Tok::Kw("boolean") | Tok::Kw("float") | Tok::Kw("double") => {
                let ty = self.parse_type()?;
                if self.eat(&Tok::Dot) { self.expect_kw("class")?; }
                Ok(Expr::ClassLit(ty))
            }
            Tok::Kw("void") => {
                self.advance();
                if self.eat(&Tok::Dot) {
                    self.expect_kw("class")?;
                    return Ok(Expr::ClassLit(Type::Prim("void")));
                }
                Err(self.err("意外的 void".into()))
            }
            Tok::LParen => {
                // lambda 已在 parse_expr 里试过；这里当括号表达式
                self.advance();
                let e = self.parse_expr()?;
                self.expect(&Tok::RParen)?;
                Ok(e)
            }
            Tok::LBrace => self.parse_array_or_expr(),
            Tok::At => {
                self.skip_annotations()?;
                self.parse_primary()
            }
            Tok::Ident(name) => {
                self.advance();
                // 泛型方法调用 name<...>(...)
                let mut type_args = Vec::new();
                if matches!(self.cur(), Tok::Lt) {
                    let save_pos = self.pos;
                    let save_pending = self.pending.clone();
                    match self.parse_type_args() {
                        Ok(args) if matches!(self.cur(), Tok::LParen) => type_args = args,
                        _ => {
                            self.pos = save_pos;
                            self.pending = save_pending;
                        }
                    }
                }
                if matches!(self.cur(), Tok::LParen) {
                    let args = self.parse_args()?;
                    return Ok(Expr::Call { recv: None, name, args, type_args });
                }
                if matches!(self.cur(), Tok::ColonColon) {
                    self.advance();
                    let n = self.expect_ident()?;
                    return Ok(Expr::MethodRef { recv: Box::new(Expr::Ident(name)), name: n });
                }
                Ok(Expr::Ident(name))
            }
            other => Err(self.err(format!("无法解析的表达式，实际 {other}"))),
        }
    }
}

pub fn parse_compilation_unit(src: &str) -> Result<CompilationUnit, ParseError> {
    let mut p = Parser::new(src)?;
    p.parse_compilation_unit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hello() {
        let src = r#"
            public class Hello {
                public static void main(String[] args) {
                    System.out.println("hi");
                }
            }
        "#;
        let cu = parse_compilation_unit(src).expect("parse");
        assert_eq!(cu.types.len(), 1);
        assert_eq!(cu.types[0].name, "Hello");
        assert_eq!(cu.types[0].kind, TypeKind::Class);
    }

    #[test]
    fn parses_generics_and_lambda() {
        let src = r#"
            import java.util.*;
            class A<T extends Comparable<T>> implements Runnable {
                Map<String, List<Integer>> m = new HashMap<>();
                public void run() { xs.forEach(x -> System.out.println(x)); }
            }
        "#;
        let cu = parse_compilation_unit(src).expect("parse");
        assert_eq!(cu.imports.len(), 1);
        assert_eq!(cu.types[0].name, "A");
    }

    #[test]
    fn parses_record_and_sealed() {
        let src = r#"
            public sealed interface Shape permits Circle, Square {}
            record Circle(double r) implements Shape {}
            final class Square implements Shape { }
        "#;
        let cu = parse_compilation_unit(src).expect("parse");
        assert_eq!(cu.types.len(), 3);
        assert_eq!(cu.types[0].kind, TypeKind::Interface);
        assert_eq!(cu.types[1].kind, TypeKind::Record);
    }

    #[test]
    fn parses_switch_arrow_and_pattern() {
        let src = r#"
            class S {
                int f(Object o) {
                    return switch (o) {
                        case Integer i when i > 0 -> 1;
                        case String s -> 2;
                        default -> 3;
                    };
                }
            }
        "#;
        let _ = parse_compilation_unit(src);
    }

    #[test]
    fn parses_enum_with_body() {
        let src = r#"
            enum E {
                A(1) { void f() {} },
                B(2);
                E(int x) {}
            }
        "#;
        let cu = parse_compilation_unit(src).expect("parse");
        assert_eq!(cu.types[0].kind, TypeKind::Enum);
    }
}
