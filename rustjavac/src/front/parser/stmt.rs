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
        // 多声明符语句（`int a = 1, b = 2;`）里排在后面的那几条：token 早就消费完了，
        // 这里按序补发。**不能包成 Block**（见 `Parser::pending_stmts` 的注释）。
        if !self.pending_stmts.is_empty() {
            return Ok(self.pending_stmts.remove(0));
        }
        if matches!(self.cur(), Tok::LBrace) {
            return self.parse_block();
        }
        if self.eat(&Tok::Semi) {
            return Ok(Stmt::Empty);
        }
        self.skip_annotations()?;
        let _ = self.eat_kw("final");
        if let Tok::Ident(lbl) = self.cur() {
            let lbl = lbl.clone();
            if matches!(self.toks.get(self.pos + 1).map(|t| &t.tok), Some(Tok::Colon)) {
                self.advance();
                self.advance();
                let body = Box::new(self.parse_statement()?);
                return Ok(Stmt::Labeled { label: lbl, body });
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
        // `yield e;` —— switch 表达式往外交值。`yield` 是**受限标识符**（不是关键字），
        // 所以按上下文判断。
        if self.is_ctx("yield") {
            self.advance();
            let e = self.parse_expr()?;
            self.expect(&Tok::Semi)?;
            return Ok(Stmt::Yield(e));
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
            // **局部类**：解析出来交给外层类型收尾时挂成嵌套类型（见 `parse_type_decl`）
            let td = self.parse_type_decl()?;
            self.local_types.push(td);
            return Ok(Stmt::Empty);
        }
        if self.is_kw("try") {
            return self.parse_try();
        }
        if self.is_kw("synchronized") {
            self.advance();
            // 锁表达式**必须留下来**：`monitorexit` 要在正常路径和异常路径上各发一次，
            // 两条路都得重新拿到同一个对象。以前这里是 `skip_balanced`，
            // 于是整段 `synchronized` 被当成普通块编出去 —— 字节码里一条
            // monitorenter/monitorexit 都没有，多线程数据静默丢失。
            let lock = if self.eat(&Tok::LParen) {
                let e = self.parse_expr()?;
                self.expect(&Tok::RParen)?;
                e
            } else {
                return Err(self.err("synchronized 后面缺少 (".into()));
            };
            let body = Box::new(self.parse_block()?);
            return Ok(Stmt::Sync { lock, body });
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
            // 第一个当场返回，其余的排队补发 —— 它们在源码里与第一个**同一作用域**，
            // 包成 Block 会让帧把槽退回 Top（多声明符的 `int[] a = …, b = …` 就编出过
            // 非法 class：`aload_2` 的槽在帧里是 top）。
            self.pending_stmts.extend(decls.drain(1..));
            return Ok(decls.pop().unwrap());
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
                // 标签位置禁掉 lambda：`case TC_NULL -> …` 与「单参数 lambda」同形，
                // 不禁就会被当成 lambda 整段吃掉（报「期望 Colon，实际 Semi」）
                let saved = self.no_lambda;
                self.no_lambda = true;
                loop {
                    labels.push(Some(self.parse_expr()?));
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.no_lambda = saved;
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
        let mut resources: Vec<Stmt> = Vec::new();
        if matches!(self.cur(), Tok::LParen) {
            // try-with-resources：`try (R r = e; S s = f) { ... }`
            self.advance();
            if !matches!(self.cur(), Tok::RParen) {
                loop {
                    self.skip_annotations()?;
                    let _ = self.eat_kw("final");
                    let ty = self.parse_type()?;
                    let name = self.expect_ident()?;
                    self.expect(&Tok::Assign)?;
                    // 资源初始化式里可能有 `,`（`new X(a, b)`）—— parse_expr 会自己吃干净
                    let init = self.parse_expr()?;
                    resources.push(Stmt::LocalVar { ty, name, init: Some(init) });
                    if !self.eat(&Tok::Semi) {
                        break;
                    }
                }
            }
            self.expect(&Tok::RParen)?;
        }
        let body = Box::new(self.parse_block()?);
        let mut catches = Vec::new();
        while self.is_kw("catch") {
            self.advance();
            self.expect(&Tok::LParen)?;
            let _ = self.eat_kw("final");
            // 多重捕获：`catch (A | B e)`
            let mut types = vec![self.parse_type()?];
            while self.eat(&Tok::Bar) {
                types.push(self.parse_type()?);
            }
            let name = self.expect_ident()?;
            self.expect(&Tok::RParen)?;
            let cbody = self.parse_block()?;
            catches.push(Catch { types, name, body: cbody });
        }
        let finally = if self.eat_kw("finally") { Some(Box::new(self.parse_block()?)) } else { None };
        if resources.is_empty() {
            return Ok(Stmt::Try { body, catches, finally });
        }
        // 糖化：`try (R r = e) { B } finally { F }`
        //   → `{ R r = e; try { B } finally { if (r != null) r.close(); F } }`
        // 多个资源按声明顺序，关闭按**反序**（java 的规则）；关闭放在用户 finally 之前。
        let mut fin_stmts: Vec<Stmt> = Vec::new();
        for r in resources.iter().rev() {
            let name = match r {
                Stmt::LocalVar { name, .. } => name.clone(),
                _ => unreachable!(),
            };
            fin_stmts.push(Stmt::If {
                cond: Expr::Binary {
                    op: BinOp::Ne,
                    lhs: Box::new(Expr::Ident(name.clone())),
                    rhs: Box::new(Expr::Null),
                },
                then: Box::new(Stmt::Block(vec![Stmt::Expr(Expr::Call {
                    recv: Some(Box::new(Expr::Ident(name))),
                    name: "close".into(),
                    args: Vec::new(),
                    type_args: Vec::new(),
                })])),
                els: None,
            });
        }
        if let Some(f) = finally {
            fin_stmts.push(*f);
        }
        let mut block = resources;
        block.push(Stmt::Try {
            body,
            catches,
            finally: Some(Box::new(Stmt::Block(fin_stmts))),
        });
        Ok(Stmt::Block(block))
    }
}
