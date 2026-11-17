//! 表达式：优先级爬升、一元/后缀、primary、lambda、cast。

use super::{ParseError, Parser};
use crate::ast::*;
use crate::lexer::Tok;

impl Parser {
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

    pub(crate) fn try_parse_lambda(&mut self) -> Result<Option<Expr>, ParseError> {
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

    pub(crate) fn finish_lambda(&mut self, params: Vec<String>) -> Result<Expr, ParseError> {
        let body = if matches!(self.cur(), Tok::LBrace) {
            let b = self.parse_block()?;
            Expr::SwitchExpr(Box::new(Expr::Ident(format!("{b:?}"))))
        } else {
            self.parse_expr()?
        };
        Ok(Expr::Lambda { params, body: Box::new(body) })
    }

    pub(crate) fn parse_ternary(&mut self) -> Result<Expr, ParseError> {
        let cond = self.parse_binary(0)?;
        if self.eat(&Tok::Question) {
            let then = self.parse_expr()?;
            self.expect(&Tok::Colon)?;
            let els = self.parse_ternary()?;
            return Ok(Expr::Cond { cond: Box::new(cond), then: Box::new(then), els: Box::new(els) });
        }
        Ok(cond)
    }

    pub(crate) fn bin_prec(t: &Tok) -> Option<(BinOp, u8)> {
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

    pub(crate) fn parse_binary(&mut self, min_prec: u8) -> Result<Expr, ParseError> {
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

    pub(crate) fn parse_unary(&mut self) -> Result<Expr, ParseError> {
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

    pub(crate) fn try_parse_cast(&mut self) -> Result<Option<Expr>, ParseError> {
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

    pub(crate) fn parse_postfix(&mut self) -> Result<Expr, ParseError> {
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

    pub(crate) fn parse_args(&mut self) -> Result<Vec<Expr>, ParseError> {
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

    pub(crate) fn parse_array_or_expr(&mut self) -> Result<Expr, ParseError> {
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

    pub(crate) fn parse_primary(&mut self) -> Result<Expr, ParseError> {
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
                    // `super(...)` 是父类构造器调用（不是普通方法调用）：
                    // 用 recv = Super 与 `super.m()` 统一表示
                    return Ok(Expr::Call {
                        recv: Some(Box::new(Expr::Super)),
                        name: "super".into(),
                        args,
                        type_args: vec![],
                    });
                }
                Ok(Expr::Super)
            }
            Tok::Kw("new") => {
                self.advance();
                let ty = self.parse_type_no_array()?;
                // 数组创建
                if matches!(self.cur(), Tok::LBracket) {
                    let mut dims = Vec::new();
                    let mut extra = 0usize; // 后面那些没给长度的 `[]`
                    while matches!(self.cur(), Tok::LBracket) {
                        self.advance();
                        if matches!(self.cur(), Tok::RBracket) {
                            self.advance();
                            extra += 1;
                            continue;
                        }
                        if extra > 0 {
                            return Err(self.err("数组维度的长度必须写在前面（`new int[3][]`）".into()));
                        }
                        dims.push(self.parse_expr()?);
                        self.expect(&Tok::RBracket)?;
                    }
                    if matches!(self.cur(), Tok::LBrace) {
                        let init = self.parse_array_or_expr()?;
                        // `new T[]{…}` / `new T[][]{…}`：元素类型由 T 与 `[]` 个数给出。
                        // 包一层 Cast 把类型带给 codegen（裸 `{…}` 没有类型信息）
                        let mut arr_ty = ty.clone();
                        for _ in 0..extra.max(1) {
                            arr_ty = Type::Array(Box::new(arr_ty));
                        }
                        return Ok(Expr::Cast { ty: arr_ty, expr: Box::new(init) });
                    }
                    // `new int[3][]` 的元素类型是 `int[]`：把没给长度的维度包进 elem，
                    // 这样 `dims.len()` 就是 multianewarray 的维数，elem 是最后一维的元素
                    let mut elem = ty;
                    for _ in 0..extra {
                        elem = Type::Array(Box::new(elem));
                    }
                    return Ok(Expr::ArrayNew { elem, dims });
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
