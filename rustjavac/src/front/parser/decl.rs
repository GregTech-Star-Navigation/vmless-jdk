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

    pub(crate) fn parse_type_params(&mut self) -> Result<Vec<TypeParam>, ParseError> {
        let mut out = Vec::new();
        if !matches!(self.cur(), Tok::Lt) {
            return Ok(out);
        }
        self.expect(&Tok::Lt)?;
        loop {
            self.skip_annotations()?;
            let name = self.expect_ident()?;
            // 擦除时按第一个上界走（`<T extends A & B>` 只留 A）
            let mut bound = None;
            if self.eat_kw("extends") {
                bound = Some(self.parse_type()?);
                while self.eat(&Tok::Amp) {
                    let _ = self.parse_type()?;
                }
            }
            out.push(TypeParam { name, bound });
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
                    // `T...` 的声明类型就是 `T[]`：描述符/字段/局部槽都按数组走，
                    // 只有调用点需要知道「多余的实参要打包成数组」
                    let ty = if varargs { Type::Array(Box::new(ty)) } else { ty };
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
        // 本类型体里收集到的局部类从这里开始记账（方法体里的类声明会往
        // `self.local_types` 里追加）
        let local_mark = self.local_types.len();
        self.expect(&Tok::LBrace)?;

        // 枚举常量
        if kind == TypeKind::Enum {
            self.skip_annotations()?;
            if !matches!(self.cur(), Tok::RBrace | Tok::Semi) {
                loop {
                    self.skip_annotations()?;
                    let cname = self.expect_ident()?;
                    let mut cargs = Vec::new();
                    if self.eat(&Tok::LParen) {
                        if !matches!(self.cur(), Tok::RParen) {
                            loop {
                                cargs.push(self.parse_expr()?);
                                if self.eat(&Tok::Comma) {
                                    continue;
                                }
                                break;
                            }
                        }
                        self.expect(&Tok::RParen)?;
                    }
                    members.push(Member::EnumConst { name: cname, args: cargs });
                    if self.eat(&Tok::LBrace) {
                        // 带体的常量（每个常量自己覆写方法）暂不支持：跳过它的实现
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
            // 多声明符字段（`int a, b;`）的其余部分按源码顺序接在后面
            members.extend(self.pending_members.drain(..));
        }
        self.expect(&Tok::RBrace)?;

        // record 的隐式成员：字段 + **同名访问器 + 规范构造器 + toString**
        //
        // 少了访问器，`p.x()` 报「未找到实例方法 x」；少了规范构造器，
        // `new P(1,2)` 根本编不出来。三样都是语言规定的（JLS 8.10.3），
        // 用户自己写了的同名方法不覆盖（自己的实现优先）。
        let declared: Vec<String> = members
            .iter()
            .filter_map(|m| match m {
                Member::Method(md) => Some(md.name.clone()),
                _ => None,
            })
            .collect();
        let ctor_params = record_params.clone();
        for p in &record_params {
            members.push(Member::Field(FieldDecl {
                modifiers: vec!["private".into(), "final".into()],
                ty: p.ty.clone(),
                name: p.name.clone(),
                init: None,
            }));
            // 访问器：`public int x() { return x; }`（名字与组件同名）
            if !declared.contains(&p.name) {
                members.push(Member::Method(MethodDecl {
                    modifiers: vec!["public".into()],
                    type_params: Vec::new(),
                    ret: Some(p.ty.clone()),
                    name: p.name.clone(),
                    params: Vec::new(),
                    throws: Vec::new(),
                    body: Some(Stmt::Block(vec![Stmt::Return(Some(Expr::Ident(p.name.clone())))])),
                }));
            }
        }
        // 规范构造器：`P(int x, int y) { this.x = x; this.y = y; }`
        if kind == TypeKind::Record && !declared.contains(&name) {
            let body: Vec<Stmt> = ctor_params
                .iter()
                .map(|p| {
                    Stmt::Expr(Expr::Assign {
                        op: None,
                        lhs: Box::new(Expr::Field(Box::new(Expr::This), p.name.clone())),
                        rhs: Box::new(Expr::Ident(p.name.clone())),
                    })
                })
                .collect();
            members.push(Member::Method(MethodDecl {
                modifiers: vec!["public".into()],
                type_params: Vec::new(),
                ret: None,
                name: name.clone(),
                params: ctor_params.clone(),
                throws: Vec::new(),
                body: Some(Stmt::Block(body)),
            }));
        }
        // `toString()`：`"P[x=" + x + ", y=" + y + "]"`。record 的契约要求它按组件
        // 打印；不生成就是 `Object.toString` 的 `P@1a2b`，语义上是错的。
        if kind == TypeKind::Record && !declared.contains(&"toString".to_string()) {
            let mut parts: Vec<Expr> = Vec::new();
            if let [first, rest @ ..] = &ctor_params[..] {
                let _ = first;
                let mut e = Expr::StrLit(format!("{name}["));
                for (i, c) in ctor_params.iter().enumerate() {
                    if i > 0 {
                        e = Expr::Binary {
                            op: BinOp::Add,
                            lhs: Box::new(e),
                            rhs: Box::new(Expr::StrLit(", ".into())),
                        };
                    }
                    e = Expr::Binary {
                        op: BinOp::Add,
                        lhs: Box::new(e),
                        rhs: Box::new(Expr::StrLit(format!("{}=", c.name))),
                    };
                    e = Expr::Binary {
                        op: BinOp::Add,
                        lhs: Box::new(e),
                        rhs: Box::new(Expr::Ident(c.name.clone())),
                    };
                }
                let _ = rest;
                parts.push(Expr::Binary {
                    op: BinOp::Add,
                    lhs: Box::new(e),
                    rhs: Box::new(Expr::StrLit("]".into())),
                });
            }
            if let Some(expr) = parts.pop() {
                members.push(Member::Method(MethodDecl {
                    modifiers: vec!["public".into()],
                    type_params: Vec::new(),
                    ret: Some(Type::Named { path: vec!["String".into()], args: Vec::new() }),
                    name: "toString".into(),
                    params: Vec::new(),
                    throws: Vec::new(),
                    body: Some(Stmt::Block(vec![Stmt::Return(Some(expr))])),
                }));
            }
        }

        // 方法体里声明的局部类挂成这个类型的嵌套类型：`sema::flatten` 会把它们
        // 拍平成顶层类型 `Outer$Inner`，`rewrite_expr` 再把使用点改名 —— 放在这里
        // 而不是丢掉，是因为 java.base 里大量 `static Arena global() { class Holder {…}
        // return Holder.GLOBAL; }` 这种写法（丢了就报「未定义的变量 Holder」）。
        for lt in self.local_types.split_off(local_mark) {
            members.push(Member::Type(lt));
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

        // `record` 是**上下文**关键字（lexer 里是 Ident），必须按 is_ctx 判；
        // 用 is_kw 的话 `private record H(int a) …` 会被当成字段声明，
        // 报「期望 Semi，实际 LParen」。
        if self.is_kw("class")
            || self.is_kw("interface")
            || self.is_kw("enum")
            || (self.is_ctx("record")
                && matches!(self.toks.get(self.pos + 1).map(|t| &t.tok), Some(Tok::Ident(_))))
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
            // 多声明符：`private int majorVersion, minorVersion;` —— 以前只留第一个，
            // 后面那些名字就完全不存在，用到时全报「未定义的变量」
            loop {
                let n2 = self.expect_ident()?;
                let mut ty2 = ty.clone();
                while matches!(self.cur(), Tok::LBracket) {
                    self.advance();
                    self.expect(&Tok::RBracket)?;
                    ty2 = Type::Array(Box::new(ty2));
                }
                let init2 = if self.eat(&Tok::Assign) { Some(self.parse_array_or_expr()?) } else { None };
                self.pending_members.push(Member::Field(FieldDecl {
                    modifiers: modifiers.clone(),
                    ty: ty2,
                    name: n2,
                    init: init2,
                }));
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
            // `String... args` 就是 `String[] args`（javac 也是这么记描述符的）：
            // 声明侧按数组走，调用侧的打包在 codegen 里按描述符做
            let ty = if varargs { Type::Array(Box::new(ty)) } else { ty };
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
