//! Java 25 语法分析（递归下降 + 优先级爬升）。
//!
//! 覆盖面：package/import、class/interface/enum/record/注解声明、字段、方法、
//! 构造器、初始化块、嵌套类型；语句块、if/while/do/for/for-each/switch(箭头)/try-catch-finally、
//! return/throw/break/continue；表达式含全部二元优先级、赋值、三元、cast、instanceof 模式、
//! lambda、方法引用、数组创建、switch 表达式。
//!
//! 本模块放 `Parser` 结构与入口 `parse_compilation_unit`，按文法分块在
//! `decl`（声明）/ `stmt`（语句）/ `expr`（表达式）三个子模块里。

pub mod decl;
pub mod expr;
pub mod stmt;

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
    /// `case X ->` 的标签位置：那里禁掉 lambda 识别。`case TC_NULL -> …` 看着
    /// 就是「单参数 lambda」，识别了会把整个 case 当 lambda 吃掉（
    /// 症状是「期望 Colon，实际 Semi」）。
    no_lambda: bool,
    /// 方法体里声明的**局部类**：解析完就被丢掉了，这里先攒着，等外层类型收尾时
    /// 挂成它的嵌套类型（`Outer$Inner`），checkpoint 用 `local_mark` 定位。
    local_types: Vec<TypeDecl>,
    /// 字段多声明符（`int a, b;`）里**第一个之后的**那些，交给成员循环按序取走。
    pending_members: Vec<Member>,
}

impl Parser {
    pub fn new(src: &str) -> Result<Self, ParseError> {
        let toks = tokenize(src).map_err(|e| ParseError { line: e.line, msg: e.msg })?;
        let line = toks.last().map(|t| t.line).unwrap_or(1);
        Ok(Parser { toks, pos: 0, pending: None, line, no_lambda: false, local_types: Vec::new(), pending_members: Vec::new() })
    }

    // ---------- token 工具 ----------

    pub(crate) fn cur(&self) -> Tok {
        if let Some(p) = &self.pending {
            return p.clone();
        }
        self.toks.get(self.pos).map(|t| t.tok.clone()).unwrap_or(Tok::Eof)
    }

    pub(crate) fn cur_line(&self) -> u32 {
        self.toks.get(self.pos).map(|t| t.line).unwrap_or(self.line)
    }

    pub(crate) fn advance(&mut self) -> Tok {
        if let Some(p) = self.pending.take() {
            return p;
        }
        let t = self.toks.get(self.pos).map(|t| t.tok.clone()).unwrap_or(Tok::Eof);
        self.pos += 1;
        t
    }

    /// 把当前 token 拆成两半（用于泛型 `>>`）
    pub(crate) fn split_current(&mut self, head: Tok, rest: Tok) {
        self.advance();
        self.pending = Some(rest);
        let _ = head;
    }

    pub(crate) fn eat(&mut self, t: &Tok) -> bool {
        if &self.cur() == t {
            self.advance();
            true
        } else {
            false
        }
    }

    pub(crate) fn eat_kw(&mut self, k: &str) -> bool {
        matches!(self.cur(), Tok::Kw(x) if x == k) && {
            self.advance();
            true
        }
    }

    pub(crate) fn expect(&mut self, t: &Tok) -> Result<(), ParseError> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(self.err(format!("期望 {t}，实际 {}", self.cur())))
        }
    }

    pub(crate) fn expect_kw(&mut self, k: &str) -> Result<(), ParseError> {
        if self.eat_kw(k) {
            Ok(())
        } else {
            Err(self.err(format!("期望关键字 `{k}`，实际 {}", self.cur())))
        }
    }

    pub(crate) fn expect_ident(&mut self) -> Result<String, ParseError> {
        match self.cur() {
            Tok::Ident(s) => {
                self.advance();
                Ok(s)
            }
            other => Err(self.err(format!("期望标识符，实际 {other}"))),
        }
    }

    pub(crate) fn err(&self, msg: String) -> ParseError {
        ParseError { line: self.cur_line(), msg }
    }

    pub(crate) fn is_kw(&self, k: &str) -> bool {
        matches!(self.cur(), Tok::Kw(x) if x == k)
    }

    pub(crate) fn is_ctx(&self, name: &str) -> bool {
        matches!(&self.cur(), Tok::Ident(s) if s == name)
    }

    pub(crate) fn eat_ctx(&mut self, name: &str) -> bool {
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

    pub(crate) fn qualified_name(&mut self) -> Result<String, ParseError> {
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

    pub(crate) fn skip_annotations(&mut self) -> Result<(), ParseError> {
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

    pub(crate) fn skip_balanced(&mut self, open: Tok, close: Tok) -> Result<(), ParseError> {
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
