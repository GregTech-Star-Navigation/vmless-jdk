//! 语句：块、条件、循环、switch、try、跳转。

use super::{ParseError, Parser};
use crate::ast::*;
use crate::lexer::Tok;

impl Parser {
    // ---------- 语句 ----------

    pub(crate) fn parse_block(&mut self) -> Result<Stmt, ParseError> {
        self.expect(&Tok::LBrace)?;
        let mut stmts = Vec::new();
        while !matches!(self.cur(), Tok::RBrace | Tok::Eof) {
            stmts.push(self.parse_statement()?);
        }
        self.expect(&Tok::RBrace)?;
        Ok(Stmt::Block(stmts))
    }

    pub(crate) fn parse_statement(&mut self) -> Result<Stmt, ParseError> {
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
    pub(crate) fn try_parse_local_type(&mut self) -> Result<Option<Type>, ParseError> {
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

    pub(crate) fn parse_for(&mut self) -> Result<Stmt, ParseError> {
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

    pub(crate) fn parse_switch_stmt(&mut self) -> Result<Stmt, ParseError> {
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

    pub(crate) fn parse_try(&mut self) -> Result<Stmt, ParseError> {
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
}
