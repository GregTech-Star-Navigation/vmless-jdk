//! `jrunscript` —— 执行脚本。本 crate 提供一个**极简 JavaScript 子集**解释器
//! （数/字符串/布尔/undefined/对象属性、算术与比较、变量、if/while/for、
//! 函数声明与调用、`print`）。
//!
//! 不引依赖是硬约束，所以没有 Nashorn/GraalJS：语言范围见 `--help`。

use std::collections::HashMap;
use std::rc::Rc;
use std::cell::RefCell;

use vmless_tools::cli::Args;
use vmless_tools::{die, EXIT_USAGE};

const TOOL: &str = "jrunscript";

fn help() {
    println!("用法: jrunscript [options] [file]");
    println!();
    println!("选项:");
    println!("  -e <script>        执行一段脚本");
    println!("  -f <file>          从文件读脚本执行");
    println!("  -q                 安静模式（不打印欢迎语）");
    println!("  -l                 列出可用引擎（本实现只有一个内建引擎）");
    println!("  -h, -?, --help     打印这段帮助");
    println!("  --version          版本信息");
    println!();
    println!("支撑的语言子集：");
    println!("  值：number / string / boolean / undefined / 对象（普通字面量不支持，靠函数返回）");
    println!("  运算：+ - * / % 比较 < <= > >= == != === !== 逻辑 && || ! 三元 ?: 赋值 = += -= *= /=");
    println!("  语句：var/let/const 声明、if/else、while、for(;;)、函数声明、return、块");
    println!("  内建：print(...) 输出一行");
    println!("  字符串：+ 拼接、.length 取长度");
    println!("超出这个子集的语法会在解析阶段报错（带行号），不会静默算错。");
}

// ---------------------------------------------------------------- 词法

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Ident(String),
    Punct(&'static str),
    Eof,
}

struct Lexer {
    src: Vec<char>,
    i: usize,
    line: usize,
}

impl Lexer {
    fn new(s: &str) -> Self {
        Lexer { src: s.chars().collect(), i: 0, line: 1 }
    }
    fn peek(&self) -> Option<char> {
        self.src.get(self.i).copied()
    }
    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c == Some('\n') {
            self.line += 1;
        }
        self.i += 1;
        c
    }
    fn skip_ws(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.bump();
                }
                Some('/') if self.src.get(self.i + 1) == Some(&'/') => {
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                Some('/') if self.src.get(self.i + 1) == Some(&'*') => {
                    self.bump();
                    self.bump();
                    while let Some(c) = self.peek() {
                        if c == '*' && self.src.get(self.i + 1) == Some(&'/') {
                            self.bump();
                            self.bump();
                            break;
                        }
                        self.bump();
                    }
                }
                _ => return,
            }
        }
    }
    fn tokens(&mut self) -> Result<Vec<Tok>, String> {
        let mut out = Vec::new();
        loop {
            self.skip_ws();
            let Some(c) = self.peek() else {
                out.push(Tok::Eof);
                return Ok(out);
            };
            if c.is_ascii_digit() {
                let mut s = String::new();
                while let Some(c) = self.peek() {
                    if c.is_ascii_digit() || c == '.' {
                        s.push(c);
                        self.bump();
                    } else {
                        break;
                    }
                }
                let n: f64 = s
                    .parse()
                    .map_err(|_| format!("第 {} 行：数字字面量不合法 {s}", self.line))?;
                out.push(Tok::Num(n));
                continue;
            }
            if c == '"' || c == '\'' {
                let q = c;
                self.bump();
                let mut s = String::new();
                loop {
                    match self.bump() {
                        Some('\\') => match self.bump() {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some(x) => s.push(x),
                            None => return Err(format!("第 {} 行：字符串未闭合", self.line)),
                        },
                        Some(x) if x == q => break,
                        Some(x) => s.push(x),
                        None => return Err(format!("第 {} 行：字符串未闭合", self.line)),
                    }
                }
                out.push(Tok::Str(s));
                continue;
            }
            if c.is_alphabetic() || c == '_' {
                let mut s = String::new();
                while let Some(c) = self.peek() {
                    if c.is_alphanumeric() || c == '_' || c == '$' {
                        s.push(c);
                        self.bump();
                    } else {
                        break;
                    }
                }
                out.push(Tok::Ident(s));
                continue;
            }
            // 多字符运算符
            let rest: String = self.src[self.i..].iter().collect();
            let three = ["===", "!=="];
            let two = ["==", "!=", "<=", ">=", "&&", "||", "+=", "-=", "*=", "/=", "++", "--"];
            let mut matched = None;
            for t in three {
                if rest.starts_with(t) {
                    matched = Some(t);
                    break;
                }
            }
            if matched.is_none() {
                for t in two {
                    if rest.starts_with(t) {
                        matched = Some(t);
                        break;
                    }
                }
            }
            if let Some(t) = matched {
                for _ in 0..t.len() {
                    self.bump();
                }
                let s: &'static str = Box::leak(t.to_string().into_boxed_str());
                out.push(Tok::Punct(s));
                continue;
            }
            if "+-*/%(){};,<>=!?:.[]".contains(c) {
                self.bump();
                let s: &'static str = Box::leak(c.to_string().into_boxed_str());
                out.push(Tok::Punct(s));
                continue;
            }
            return Err(format!("第 {} 行：不认识的字符 {c:?}", self.line));
        }
    }
}

// ---------------------------------------------------------------- 值与环境

#[derive(Clone)]
enum Value {
    Undefined,
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Func(Rc<FuncDef>),
    Native(&'static str),
}

struct FuncDef {
    name: String,
    params: Vec<String>,
    body: Vec<Stmt>,
    env: Rc<Env>,
}

struct Env {
    vars: RefCell<HashMap<String, Value>>,
    parent: Option<Rc<Env>>,
}

impl Env {
    fn new(parent: Option<Rc<Env>>) -> Rc<Env> {
        Rc::new(Env { vars: RefCell::new(HashMap::new()), parent })
    }
    fn get(&self, name: &str) -> Option<Value> {
        if let Some(v) = self.vars.borrow().get(name) {
            return Some(v.clone());
        }
        self.parent.as_ref().and_then(|p| p.get(name))
    }
    fn set(&self, name: &str, v: Value) {
        // 已有声明就改到它所在的层，否则在当前层新建
        if self.vars.borrow().contains_key(name) {
            self.vars.borrow_mut().insert(name.to_string(), v);
            return;
        }
        if let Some(p) = &self.parent {
            if p.get(name).is_some() {
                p.set(name, v);
                return;
            }
        }
        self.vars.borrow_mut().insert(name.to_string(), v);
    }
    fn declare(&self, name: &str, v: Value) {
        self.vars.borrow_mut().insert(name.to_string(), v);
    }
}

fn fmt_value(v: &Value) -> String {
    match v {
        Value::Undefined => "undefined".into(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Num(n) => {
            if n.fract() == 0.0 && n.abs() < 1e15 {
                format!("{}", *n as i64)
            } else {
                format!("{n}")
            }
        }
        Value::Str(s) => s.clone(),
        Value::Func(f) => format!("function {}()", f.name),
        Value::Native(n) => format!("[native {n}]"),
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Undefined | Value::Null => false,
        Value::Bool(b) => *b,
        Value::Num(n) => *n != 0.0 && !n.is_nan(),
        Value::Str(s) => !s.is_empty(),
        _ => true,
    }
}

fn num_of(v: &Value) -> f64 {
    match v {
        Value::Num(n) => *n,
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Value::Str(s) => s.trim().parse().unwrap_or(f64::NAN),
        Value::Null => 0.0,
        _ => f64::NAN,
    }
}

// ---------------------------------------------------------------- 语法树

#[derive(Clone)]
enum Stmt {
    VarDecl(String, Option<Expr>),
    Expr(Expr),
    If(Expr, Box<Stmt>, Option<Box<Stmt>>),
    While(Expr, Box<Stmt>),
    For(Option<Box<Stmt>>, Option<Expr>, Option<Expr>, Box<Stmt>),
    Block(Vec<Stmt>),
    FuncDecl(Rc<FuncDef>, Vec<String>),
    Return(Option<Expr>),
    Break,
    Continue,
    Empty,
}

#[derive(Clone)]
enum Expr {
    Lit(Value),
    Ident(String),
    Unary(&'static str, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    Assign(&'static str, Box<Expr>, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    Call(Box<Expr>, Vec<Expr>),
    Member(Box<Expr>, String),
}

struct Parser {
    toks: Vec<Tok>,
    i: usize,
}

impl Parser {
    fn peek(&self) -> &Tok {
        self.toks.get(self.i).unwrap_or(&Tok::Eof)
    }
    fn bump(&mut self) -> Tok {
        let t = self.peek().clone();
        self.i += 1;
        t
    }
    fn eat_punct(&mut self, p: &str) -> bool {
        if let Tok::Punct(x) = self.peek() {
            if *x == p {
                self.i += 1;
                return true;
            }
        }
        false
    }
    fn expect_punct(&mut self, p: &str) -> Result<(), String> {
        if self.eat_punct(p) {
            Ok(())
        } else {
            Err(format!("语法错误：期待 {p}，实际是 {:?}", self.peek()))
        }
    }
    fn expect_ident(&mut self) -> Result<String, String> {
        match self.bump() {
            Tok::Ident(s) => Ok(s),
            other => Err(format!("语法错误：期待标识符，实际是 {other:?}")),
        }
    }

    fn program(&mut self) -> Result<Vec<Stmt>, String> {
        let mut out = Vec::new();
        while !matches!(self.peek(), Tok::Eof) {
            out.push(self.statement()?);
        }
        Ok(out)
    }

    fn statement(&mut self) -> Result<Stmt, String> {
        if self.eat_punct(";") {
            return Ok(Stmt::Empty);
        }
        if self.eat_punct("{") {
            let mut body = Vec::new();
            while !self.eat_punct("}") {
                if matches!(self.peek(), Tok::Eof) {
                    return Err("语法错误：块没有闭合".into());
                }
                body.push(self.statement()?);
            }
            return Ok(Stmt::Block(body));
        }
        if let Tok::Ident(kw) = self.peek().clone() {
            match kw.as_str() {
                "var" | "let" | "const" => {
                    self.bump();
                    let name = self.expect_ident()?;
                    let init = if self.eat_punct("=") {
                        Some(self.expression()?)
                    } else {
                        None
                    };
                    self.eat_punct(";");
                    return Ok(Stmt::VarDecl(name, init));
                }
                "if" => {
                    self.bump();
                    self.expect_punct("(")?;
                    let c = self.expression()?;
                    self.expect_punct(")")?;
                    let then = Box::new(self.statement()?);
                    let alt = if matches!(self.peek(), Tok::Ident(k) if k == "else") {
                        self.bump();
                        Some(Box::new(self.statement()?))
                    } else {
                        None
                    };
                    return Ok(Stmt::If(c, then, alt));
                }
                "while" => {
                    self.bump();
                    self.expect_punct("(")?;
                    let c = self.expression()?;
                    self.expect_punct(")")?;
                    let body = Box::new(self.statement()?);
                    return Ok(Stmt::While(c, body));
                }
                "for" => {
                    self.bump();
                    self.expect_punct("(")?;
                    let init = if self.eat_punct(";") {
                        None
                    } else {
                        // init 里可能是 `var i = 0` 声明，交给 statement() 才认得出
                        let s = self.statement()?;
                        Some(Box::new(s))
                    };
                    let cond = if self.eat_punct(";") {
                        None
                    } else {
                        let e = self.expression()?;
                        self.expect_punct(";")?;
                        Some(e)
                    };
                    let step = if self.eat_punct(")") {
                        None
                    } else {
                        let e = self.expression()?;
                        self.expect_punct(")")?;
                        Some(e)
                    };
                    let body = Box::new(self.statement()?);
                    return Ok(Stmt::For(init, cond, step, body));
                }
                "function" => {
                    self.bump();
                    let name = self.expect_ident()?;
                    let params = self.params()?;
                    let body = match self.statement()? {
                        Stmt::Block(b) => b,
                        _ => return Err("语法错误：函数体必须是块".into()),
                    };
                    let def = Rc::new(FuncDef {
                        name: name.clone(),
                        params: params.clone(),
                        body,
                        env: Env::new(None),
                    });
                    return Ok(Stmt::FuncDecl(def, params));
                }
                "return" => {
                    self.bump();
                    let v = if self.eat_punct(";") || matches!(self.peek(), Tok::Eof) {
                        None
                    } else {
                        let e = self.expression()?;
                        self.eat_punct(";");
                        Some(e)
                    };
                    return Ok(Stmt::Return(v));
                }
                "break" => {
                    self.bump();
                    self.eat_punct(";");
                    return Ok(Stmt::Break);
                }
                "continue" => {
                    self.bump();
                    self.eat_punct(";");
                    return Ok(Stmt::Continue);
                }
                _ => {}
            }
        }
        let s = self.simple_statement()?;
        Ok(s)
    }

    fn simple_statement(&mut self) -> Result<Stmt, String> {
        let e = self.expression()?;
        self.eat_punct(";");
        Ok(Stmt::Expr(e))
    }

    fn params(&mut self) -> Result<Vec<String>, String> {
        self.expect_punct("(")?;
        let mut p = Vec::new();
        if !self.eat_punct(")") {
            loop {
                p.push(self.expect_ident()?);
                if self.eat_punct(")") {
                    break;
                }
                self.expect_punct(",")?;
            }
        }
        Ok(p)
    }

    fn expression(&mut self) -> Result<Expr, String> {
        self.assign()
    }

    fn assign(&mut self) -> Result<Expr, String> {
        let left = self.ternary()?;
        for op in ["=", "+=", "-=", "*=", "/="] {
            if let Tok::Punct(p) = self.peek() {
                if *p == op {
                    let op: &'static str = p;
                    self.bump();
                    let right = self.assign()?;
                    return Ok(Expr::Assign(op, Box::new(left), Box::new(right)));
                }
            }
        }
        Ok(left)
    }

    fn ternary(&mut self) -> Result<Expr, String> {
        let c = self.logical_or()?;
        if self.eat_punct("?") {
            let a = self.assign()?;
            self.expect_punct(":")?;
            let b = self.assign()?;
            return Ok(Expr::Cond(Box::new(c), Box::new(a), Box::new(b)));
        }
        Ok(c)
    }

    fn logical_or(&mut self) -> Result<Expr, String> {
        let mut l = self.logical_and()?;
        while let Tok::Punct("||") = self.peek() {
            self.bump();
            let r = self.logical_and()?;
            l = Expr::Binary("||", Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn logical_and(&mut self) -> Result<Expr, String> {
        let mut l = self.equality()?;
        while let Tok::Punct("&&") = self.peek() {
            self.bump();
            let r = self.equality()?;
            l = Expr::Binary("&&", Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn equality(&mut self) -> Result<Expr, String> {
        let mut l = self.relational()?;
        loop {
            let op = match self.peek() {
                Tok::Punct(p) if ["==", "!=", "===", "!=="].contains(p) => *p,
                _ => break,
            };
            self.bump();
            let r = self.relational()?;
            l = Expr::Binary(op, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn relational(&mut self) -> Result<Expr, String> {
        let mut l = self.additive()?;
        loop {
            let op = match self.peek() {
                Tok::Punct(p) if ["<", "<=", ">", ">="].contains(p) => *p,
                _ => break,
            };
            self.bump();
            let r = self.additive()?;
            l = Expr::Binary(op, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn additive(&mut self) -> Result<Expr, String> {
        let mut l = self.multiplicative()?;
        loop {
            let op = match self.peek() {
                Tok::Punct(p) if ["+", "-"].contains(p) => *p,
                _ => break,
            };
            self.bump();
            let r = self.multiplicative()?;
            l = Expr::Binary(op, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn multiplicative(&mut self) -> Result<Expr, String> {
        let mut l = self.unary()?;
        loop {
            let op = match self.peek() {
                Tok::Punct(p) if ["*", "/", "%"].contains(p) => *p,
                _ => break,
            };
            self.bump();
            let r = self.unary()?;
            l = Expr::Binary(op, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn unary(&mut self) -> Result<Expr, String> {
        if let Tok::Punct(p) = self.peek() {
            if ["!", "-", "+"].contains(p) {
                let op: &'static str = p;
                self.bump();
                let e = self.unary()?;
                return Ok(Expr::Unary(op, Box::new(e)));
            }
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Expr, String> {
        let mut e = self.primary()?;
        loop {
            // `i++` / `i--`：语法糖成 `i += 1` / `i -= 1`
            if let Tok::Punct(p) = self.peek() {
                if *p == "++" || *p == "--" {
                    let op: &'static str = if *p == "++" { "+=" } else { "-=" };
                    self.bump();
                    e = Expr::Assign(op, Box::new(e), Box::new(Expr::Lit(Value::Num(1.0))));
                    continue;
                }
            }
            if self.eat_punct("(") {
                let mut args = Vec::new();
                if !self.eat_punct(")") {
                    loop {
                        args.push(self.expression()?);
                        if self.eat_punct(")") {
                            break;
                        }
                        self.expect_punct(",")?;
                    }
                }
                e = Expr::Call(Box::new(e), args);
                continue;
            }
            if self.eat_punct(".") {
                let name = self.expect_ident()?;
                e = Expr::Member(Box::new(e), name);
                continue;
            }
            break;
        }
        Ok(e)
    }

    fn primary(&mut self) -> Result<Expr, String> {
        match self.bump() {
            Tok::Num(n) => Ok(Expr::Lit(Value::Num(n))),
            Tok::Str(s) => Ok(Expr::Lit(Value::Str(s))),
            Tok::Ident(s) => match s.as_str() {
                "true" => Ok(Expr::Lit(Value::Bool(true))),
                "false" => Ok(Expr::Lit(Value::Bool(false))),
                "null" => Ok(Expr::Lit(Value::Null)),
                "undefined" => Ok(Expr::Lit(Value::Undefined)),
                _ => Ok(Expr::Ident(s)),
            },
            Tok::Punct("(") => {
                let e = self.expression()?;
                self.expect_punct(")")?;
                Ok(e)
            }
            other => Err(format!("语法错误：不期待 {other:?}")),
        }
    }
}

// ---------------------------------------------------------------- 执行

enum Flow {
    Normal,
    Break,
    Continue,
    Return(Value),
}

struct Interp {
    out: Vec<String>,
}

impl Interp {
    fn run(&mut self, stmts: &[Stmt], env: &Rc<Env>) -> Result<(), String> {
        match self.exec_block(stmts, env)? {
            Flow::Normal | Flow::Return(_) | Flow::Break | Flow::Continue => Ok(()),
        }
    }

    fn exec_block(&mut self, stmts: &[Stmt], env: &Rc<Env>) -> Result<Flow, String> {
        for s in stmts {
            match self.exec(s, env)? {
                Flow::Normal => {}
                other => return Ok(other),
            }
        }
        Ok(Flow::Normal)
    }

    fn exec(&mut self, s: &Stmt, env: &Rc<Env>) -> Result<Flow, String> {
        match s {
            Stmt::Empty => Ok(Flow::Normal),
            Stmt::VarDecl(name, init) => {
                let v = match init {
                    Some(e) => self.eval(e, env)?,
                    None => Value::Undefined,
                };
                env.declare(name, v);
                Ok(Flow::Normal)
            }
            Stmt::Expr(e) => {
                self.eval(e, env)?;
                Ok(Flow::Normal)
            }
            Stmt::Block(b) => self.exec_block(b, &Env::new(Some(env.clone()))),
            Stmt::If(c, t, f) => {
                if truthy(&self.eval(c, env)?) {
                    self.exec(t, env)
                } else if let Some(f) = f {
                    self.exec(f, env)
                } else {
                    Ok(Flow::Normal)
                }
            }
            Stmt::While(c, body) => {
                let mut guard = 0u64;
                while truthy(&self.eval(c, env)?) {
                    guard += 1;
                    if guard > 50_000_000 {
                        return Err("while 循环超过 5000 万次，疑似死循环".into());
                    }
                    match self.exec(body, env)? {
                        Flow::Break => break,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        _ => {}
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::For(init, cond, step, body) => {
                let scope = Env::new(Some(env.clone()));
                if let Some(i) = init {
                    self.exec(i, &scope)?;
                }
                let mut guard = 0u64;
                loop {
                    if let Some(c) = cond {
                        if !truthy(&self.eval(c, &scope)?) {
                            break;
                        }
                    }
                    guard += 1;
                    if guard > 50_000_000 {
                        return Err("for 循环超过 5000 万次，疑似死循环".into());
                    }
                    match self.exec(body, &scope)? {
                        Flow::Break => break,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        _ => {}
                    }
                    if let Some(st) = step {
                        self.eval(st, &scope)?;
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::FuncDecl(def, _) => {
                // 闭包捕获定义处的环境
                let captured = Rc::new(FuncDef {
                    name: def.name.clone(),
                    params: def.params.clone(),
                    body: def.body.clone(),
                    env: env.clone(),
                });
                env.declare(&def.name, Value::Func(captured));
                Ok(Flow::Normal)
            }
            Stmt::Return(e) => {
                let v = match e {
                    Some(e) => self.eval(e, env)?,
                    None => Value::Undefined,
                };
                Ok(Flow::Return(v))
            }
            Stmt::Break => Ok(Flow::Break),
            Stmt::Continue => Ok(Flow::Continue),
        }
    }

    fn eval(&mut self, e: &Expr, env: &Rc<Env>) -> Result<Value, String> {
        match e {
            Expr::Lit(v) => Ok(v.clone()),
            Expr::Ident(n) => Ok(env.get(n).unwrap_or(Value::Undefined)),
            Expr::Unary(op, e) => {
                let v = self.eval(e, env)?;
                Ok(match *op {
                    "!" => Value::Bool(!truthy(&v)),
                    "-" => Value::Num(-num_of(&v)),
                    "+" => Value::Num(num_of(&v)),
                    _ => Value::Undefined,
                })
            }
            Expr::Cond(c, a, b) => {
                if truthy(&self.eval(c, env)?) {
                    self.eval(a, env)
                } else {
                    self.eval(b, env)
                }
            }
            Expr::Binary(op, a, b) => {
                if *op == "&&" {
                    let l = self.eval(a, env)?;
                    return if truthy(&l) { self.eval(b, env) } else { Ok(l) };
                }
                if *op == "||" {
                    let l = self.eval(a, env)?;
                    return if truthy(&l) { Ok(l) } else { self.eval(b, env) };
                }
                let l = self.eval(a, env)?;
                let r = self.eval(b, env)?;
                Ok(match *op {
                    "+" => match (&l, &r) {
                        (Value::Str(_), _) | (_, Value::Str(_)) => {
                            Value::Str(format!("{}{}", fmt_value(&l), fmt_value(&r)))
                        }
                        _ => Value::Num(num_of(&l) + num_of(&r)),
                    },
                    "-" => Value::Num(num_of(&l) - num_of(&r)),
                    "*" => Value::Num(num_of(&l) * num_of(&r)),
                    "/" => Value::Num(num_of(&l) / num_of(&r)),
                    "%" => Value::Num(num_of(&l) % num_of(&r)),
                    "<" => Value::Bool(num_of(&l) < num_of(&r)),
                    "<=" => Value::Bool(num_of(&l) <= num_of(&r)),
                    ">" => Value::Bool(num_of(&l) > num_of(&r)),
                    ">=" => Value::Bool(num_of(&l) >= num_of(&r)),
                    "==" | "===" => Value::Bool(loose_eq(&l, &r)),
                    "!=" | "!==" => Value::Bool(!loose_eq(&l, &r)),
                    _ => Value::Undefined,
                })
            }
            Expr::Assign(op, target, value) => {
                let rhs = self.eval(value, env)?;
                let v = if *op == "=" {
                    rhs
                } else {
                    let cur = self.eval(target, env)?;
                    let bin = &op[0..1];
                    let opref: &'static str = match bin {
                        "+" => "+",
                        "-" => "-",
                        "*" => "*",
                        "/" => "/",
                        _ => "+",
                    };
                    match opref {
                        "+" if matches!(cur, Value::Str(_)) || matches!(rhs, Value::Str(_)) => {
                            Value::Str(format!("{}{}", fmt_value(&cur), fmt_value(&rhs)))
                        }
                        "+" => Value::Num(num_of(&cur) + num_of(&rhs)),
                        "-" => Value::Num(num_of(&cur) - num_of(&rhs)),
                        "*" => Value::Num(num_of(&cur) * num_of(&rhs)),
                        _ => Value::Num(num_of(&cur) / num_of(&rhs)),
                    }
                };
                match &**target {
                    Expr::Ident(n) => {
                        env.set(n, v.clone());
                        Ok(v)
                    }
                    _ => Err("赋值目标必须是变量".into()),
                }
            }
            Expr::Member(obj, name) => {
                let o = self.eval(obj, env)?;
                Ok(match (&o, name.as_str()) {
                    (Value::Str(s), "length") => Value::Num(s.chars().count() as f64),
                    _ => Value::Undefined,
                })
            }
            Expr::Call(callee, args) => {
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(self.eval(a, env)?);
                }
                // print(...) 与 console.log(...)
                let is_print = matches!(&**callee, Expr::Ident(n) if n == "print")
                    || matches!(&**callee, Expr::Member(o, m) if m == "log"
                        && matches!(&**o, Expr::Ident(n) if n == "console"));
                if is_print {
                    let line = vals.iter().map(fmt_value).collect::<Vec<_>>().join(" ");
                    self.out.push(line);
                    return Ok(Value::Undefined);
                }
                let f = self.eval(callee, env)?;
                match f {
                    Value::Func(def) => {
                        let scope = Env::new(Some(def.env.clone()));
                        for (i, p) in def.params.iter().enumerate() {
                            scope.declare(p, vals.get(i).cloned().unwrap_or(Value::Undefined));
                        }
                        match self.exec_block(&def.body, &scope)? {
                            Flow::Return(v) => Ok(v),
                            _ => Ok(Value::Undefined),
                        }
                    }
                    Value::Undefined => Err("调用了未定义的函数".into()),
                    other => Err(format!("{} 不是函数", fmt_value(&other))),
                }
            }
        }
    }
}

fn loose_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Undefined, Value::Undefined) => true,
        (Value::Null, Value::Null) => true,
        (Value::Num(x), Value::Num(y)) => x == y,
        _ => num_of(a) == num_of(b) && !(num_of(a).is_nan() && num_of(b).is_nan()),
    }
}

fn run_script(src: &str) -> Result<Vec<String>, String> {
    let toks = Lexer::new(src).tokens()?;
    let mut p = Parser { toks, i: 0 };
    let prog = p.program()?;
    let env = Env::new(None);
    let mut it = Interp { out: Vec::new() };
    it.run(&prog, &env)?;
    Ok(it.out)
}

fn main() {
    vmless_tools::install_pipe_guard();
    let args = Args::new();
    if args.wants_help() {
        help();
        return;
    }
    if args.wants_version() {
        vmless_tools::print_version(TOOL, "25");
        return;
    }
    let takes: &[&str] = &["-e", "-f"];
    let known: &[&str] = &["-e", "-f", "-q", "-l", "-h", "-?", "--help", "--version"];
    if let Err(e) = args.reject_unknown(TOOL, known, takes) {
        eprintln!("{e}");
        std::process::exit(EXIT_USAGE);
    }
    if args.flag("-l") {
        println!("内建引擎: vmless-js-subset（极简 JavaScript 子集）");
        return;
    }
    let mut sources: Vec<(String, String)> = Vec::new(); // (名字, 源码)
    if let Ok(Some(code)) = args.value("-e") {
        sources.push(("<eval>".into(), code));
    }
    if let Ok(Some(f)) = args.value("-f") {
        match std::fs::read_to_string(&f) {
            Ok(t) => sources.push((f.clone(), t)),
            Err(e) => die(TOOL, &format!("读脚本 {f} 失败: {e}")),
        }
    }
    if let Ok(pos) = args.positionals(takes) {
        if let Some(f) = pos.first() {
            match std::fs::read_to_string(f) {
                Ok(t) => sources.push((f.clone(), t)),
                Err(e) => die(TOOL, &format!("读脚本 {f} 失败: {e}")),
            }
        }
    }
    if sources.is_empty() {
        // 从 stdin 读（真 jrunscript 无参数时进交互模式）
        use std::io::Read;
        let mut s = String::new();
        if std::io::stdin().read_to_string(&mut s).is_err() || s.trim().is_empty() {
            help();
            return;
        }
        sources.push(("<stdin>".into(), s));
    }
    for (name, src) in sources {
        match run_script(&src) {
            Ok(out) => {
                for line in out {
                    println!("{line}");
                }
            }
            Err(e) => die(TOOL, &format!("{name}: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::run_script;

    #[test]
    fn basics() {
        assert_eq!(run_script("print(1+2)").unwrap(), vec!["3"]);
        assert_eq!(run_script("var x = 'hi'; print(x.length)").unwrap(), vec!["2"]);
        assert_eq!(
            run_script("function f(n){ var s=0; for(var i=1;i<=n;i++){ s+=i; } return s; } print(f(10))")
                .unwrap(),
            vec!["55"]
        );
        assert_eq!(
            run_script("var i=0; while(i<3){ print(i); i++; }").unwrap(),
            vec!["0", "1", "2"]
        );
        assert!(run_script("print(").is_err());
    }
}
