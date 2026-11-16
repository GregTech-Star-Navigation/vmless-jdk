//! Java 25 词法分析。
//!
//! 先按 JLS §3.3 处理 unicode 转义（`\uXXXX`），再切 token。
//! 支持：标识符、全部关键字、各类字面量（含文本块）、注释、全部运算符。
//!
//! token 种类与关键字表在 `token` 子模块，本模块只放词法主循环。

pub mod token;

pub use token::*;

#[derive(Debug)]
pub struct LexError {
    pub line: u32,
    pub msg: String,
}

/// 把源码里的 unicode 转义替换成真实字符（JLS §3.3）。
fn translate_unicode_escapes(src: &str) -> String {
    let b: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == '\\' && i + 1 < b.len() && b[i + 1] == 'u' {
            // 允许多个连续的 u
            let mut j = i + 1;
            while j < b.len() && b[j] == 'u' {
                j += 1;
            }
            if j + 4 <= b.len() {
                let hex: String = b[j..j + 4].iter().collect();
                if let Ok(v) = u32::from_str_radix(&hex, 16) {
                    if let Some(c) = char::from_u32(v) {
                        out.push(c);
                        i = j + 4;
                        continue;
                    }
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

pub struct Lexer {
    src: Vec<char>,
    pos: usize,
    line: u32,
}

impl Lexer {
    pub fn new(source: &str) -> Self {
        let translated = translate_unicode_escapes(source);
        Lexer { src: translated.chars().collect(), pos: 0, line: 1 }
    }

    pub fn line(&self) -> u32 {
        self.line
    }

    fn peek(&self) -> Option<char> {
        self.src.get(self.pos).copied()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.src.get(self.pos + n).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }

    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.bump();
                }
                Some('/') if self.peek_at(1) == Some('/') => {
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                Some('/') if self.peek_at(1) == Some('*') => {
                    self.bump();
                    self.bump();
                    while let Some(c) = self.peek() {
                        if c == '*' && self.peek_at(1) == Some('/') {
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

    pub fn next_token(&mut self) -> Result<Token, LexError> {
        self.skip_trivia();
        let line = self.line;
        let c = match self.peek() {
            Some(c) => c,
            None => return Ok(Token { tok: Tok::Eof, line }),
        };

        if c == '"' {
            if self.peek_at(1) == Some('"') && self.peek_at(2) == Some('"') {
                let s = self.text_block()?;
                return Ok(Token { tok: Tok::TextBlock(s), line });
            }
            let s = self.quoted('"')?;
            return Ok(Token { tok: Tok::StrLit(s), line });
        }
        if c == '\'' {
            let s = self.quoted('\'')?;
            return Ok(Token { tok: Tok::CharLit(s), line });
        }
        if c.is_ascii_digit() || (c == '.' && matches!(self.peek_at(1), Some(d) if d.is_ascii_digit())) {
            let t = self.number();
            return Ok(Token { tok: t, line });
        }
        if c == '_' || c.is_alphabetic() {
            let start = self.pos;
            while let Some(ch) = self.peek() {
                if ch == '_' || ch.is_alphanumeric() {
                    self.bump();
                } else {
                    break;
                }
            }
            let word: String = self.src[start..self.pos].iter().collect();
            if let Some(k) = KEYWORDS.iter().find(|k| **k == word) {
                return Ok(Token { tok: Tok::Kw(k), line });
            }
            return Ok(Token { tok: Tok::Ident(word), line });
        }

        let t = self.punct()?;
        Ok(Token { tok: t, line })
    }

    fn punct(&mut self) -> Result<Tok, LexError> {
        let c = self.bump().unwrap();
        let next = self.peek();
        macro_rules! two {
            ($ch:expr, $tok:expr, $tok2:expr) => {{
                if next == Some($ch) { self.bump(); $tok2 } else { $tok }
            }};
        }
        Ok(match c {
            '(' => Tok::LParen,
            ')' => Tok::RParen,
            '{' => Tok::LBrace,
            '}' => Tok::RBrace,
            '[' => Tok::LBracket,
            ']' => Tok::RBracket,
            ';' => Tok::Semi,
            ',' => Tok::Comma,
            '@' => Tok::At,
            '~' => Tok::Tilde,
            '?' => Tok::Question,
            '.' => {
                if next == Some('.') && self.peek_at(1) == Some('.') {
                    self.bump();
                    self.bump();
                    Tok::Ellipsis
                } else {
                    Tok::Dot
                }
            }
            ':' => {
                if next == Some(':') {
                    self.bump();
                    Tok::ColonColon
                } else {
                    Tok::Colon
                }
            }
            '+' => {
                if next == Some('+') {
                    self.bump();
                    Tok::PlusPlus
                } else if next == Some('=') {
                    self.bump();
                    Tok::PlusAssign
                } else {
                    Tok::Plus
                }
            }
            '-' => {
                if next == Some('-') {
                    self.bump();
                    Tok::MinusMinus
                } else if next == Some('=') {
                    self.bump();
                    Tok::MinusAssign
                } else if next == Some('>') {
                    self.bump();
                    Tok::Arrow
                } else {
                    Tok::Minus
                }
            }
            '*' => two!('=', Tok::Star, Tok::StarAssign),
            '/' => two!('=', Tok::Slash, Tok::SlashAssign),
            '%' => two!('=', Tok::Percent, Tok::PercentAssign),
            '^' => two!('=', Tok::Caret, Tok::CaretAssign),
            '!' => two!('=', Tok::Bang, Tok::Ne),
            '=' => two!('=', Tok::Assign, Tok::EqEq),
            '&' => {
                if next == Some('&') {
                    self.bump();
                    Tok::AndAnd
                } else if next == Some('=') {
                    self.bump();
                    Tok::AmpAssign
                } else {
                    Tok::Amp
                }
            }
            '|' => {
                if next == Some('|') {
                    self.bump();
                    Tok::OrOr
                } else if next == Some('=') {
                    self.bump();
                    Tok::BarAssign
                } else {
                    Tok::Bar
                }
            }
            '<' => {
                if next == Some('<') {
                    self.bump();
                    if self.peek() == Some('=') {
                        self.bump();
                        Tok::ShlAssign
                    } else {
                        Tok::Shl
                    }
                } else if next == Some('=') {
                    self.bump();
                    Tok::Le
                } else {
                    Tok::Lt
                }
            }
            '>' => {
                // 注意：泛型里的 `>>` 由 parser 在需要时拆分
                if next == Some('>') {
                    self.bump();
                    if self.peek() == Some('>') {
                        self.bump();
                        if self.peek() == Some('=') {
                            self.bump();
                            Tok::UshrAssign
                        } else {
                            Tok::Ushr
                        }
                    } else if self.peek() == Some('=') {
                        self.bump();
                        Tok::ShrAssign
                    } else {
                        Tok::Shr
                    }
                } else if next == Some('=') {
                    self.bump();
                    Tok::Ge
                } else {
                    Tok::Gt
                }
            }
            other => {
                return Err(LexError { line: self.line, msg: format!("意外的字符 `{other}`") })
            }
        })
    }

    /// 字符串/字符字面量；返回**去掉引号的原始内容**（保留转义）
    fn quoted(&mut self, quote: char) -> Result<String, LexError> {
        self.bump(); // 开引号
        let mut s = String::new();
        loop {
            match self.peek() {
                None => return Err(LexError { line: self.line, msg: "字面量未闭合".into() }),
                Some('\\') => {
                    s.push(self.bump().unwrap());
                    if let Some(c) = self.bump() {
                        s.push(c);
                    }
                }
                Some(c) if c == quote => {
                    self.bump();
                    return Ok(s);
                }
                Some(c) => {
                    s.push(c);
                    self.bump();
                }
            }
        }
    }

    fn text_block(&mut self) -> Result<String, LexError> {
        self.bump();
        self.bump();
        self.bump();
        let mut s = String::new();
        loop {
            if self.peek() == Some('"') && self.peek_at(1) == Some('"') && self.peek_at(2) == Some('"') {
                self.bump();
                self.bump();
                self.bump();
                return Ok(s);
            }
            match self.bump() {
                None => return Err(LexError { line: self.line, msg: "文本块未闭合".into() }),
                Some(c) => s.push(c),
            }
        }
    }

    fn number(&mut self) -> Tok {
        let start = self.pos;
        let mut is_float = false;
        if self.peek() == Some('0')
            && matches!(self.peek_at(1), Some('x') | Some('X') | Some('b') | Some('B'))
        {
            self.bump();
            self.bump();
            while let Some(c) = self.peek() {
                if c.is_ascii_hexdigit() || c == '_' {
                    self.bump();
                } else {
                    break;
                }
            }
            if matches!(self.peek(), Some('.') ) {
                is_float = true;
                self.bump();
                while let Some(c) = self.peek() {
                    if c.is_ascii_hexdigit() || c == '_' {
                        self.bump();
                    } else {
                        break;
                    }
                }
            }
            if matches!(self.peek(), Some('p') | Some('P')) {
                is_float = true;
                self.bump();
                if matches!(self.peek(), Some('+') | Some('-')) {
                    self.bump();
                }
                while let Some(c) = self.peek() {
                    if c.is_ascii_digit() || c == '_' {
                        self.bump();
                    } else {
                        break;
                    }
                }
            }
        } else {
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() || c == '_' {
                    self.bump();
                } else {
                    break;
                }
            }
            if self.peek() == Some('.') && matches!(self.peek_at(1), Some(d) if d.is_ascii_digit()) {
                is_float = true;
                self.bump();
                while let Some(c) = self.peek() {
                    if c.is_ascii_digit() || c == '_' {
                        self.bump();
                    } else {
                        break;
                    }
                }
            }
            if matches!(self.peek(), Some('e') | Some('E')) {
                let save = self.pos;
                self.bump();
                if matches!(self.peek(), Some('+') | Some('-')) {
                    self.bump();
                }
                if matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    is_float = true;
                    while let Some(c) = self.peek() {
                        if c.is_ascii_digit() || c == '_' {
                            self.bump();
                        } else {
                            break;
                        }
                    }
                } else {
                    self.pos = save;
                }
            }
        }
        match self.peek() {
            Some('l') | Some('L') => {
                self.bump();
            }
            Some('f') | Some('F') | Some('d') | Some('D') => {
                is_float = true;
                self.bump();
            }
            _ => {}
        }
        let text: String = self.src[start..self.pos].iter().collect();
        if is_float {
            Tok::FloatLit(text)
        } else {
            Tok::IntLit(text)
        }
    }
}

pub fn tokenize(src: &str) -> Result<Vec<Token>, LexError> {
    let mut lx = Lexer::new(src);
    let mut out = Vec::new();
    loop {
        let t = lx.next_token()?;
        let done = t.tok == Tok::Eof;
        out.push(t);
        if done {
            return Ok(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexes_basic_java() {
        let toks = tokenize("package a.b; public class X { int f = 1_000; String s = \"hi\"; }").unwrap();
        assert!(matches!(toks[0].tok, Tok::Kw("package")));
        assert!(toks.iter().any(|t| matches!(&t.tok, Tok::IntLit(s) if s == "1_000")));
        assert!(toks.iter().any(|t| matches!(&t.tok, Tok::StrLit(s) if s == "hi")));
    }

    #[test]
    fn lexes_text_block_and_lambda() {
        let src = "var s = \"\"\"\n  hi\n  \"\"\"; xs.forEach(x -> x + 1);";
        let toks = tokenize(src).unwrap();
        assert!(toks.iter().any(|t| matches!(t.tok, Tok::TextBlock(_))));
        assert!(toks.iter().any(|t| matches!(t.tok, Tok::Arrow)));
    }

    #[test]
    fn handles_unicode_escape() {
        let toks = tokenize("int \\u0061bc = 1;").unwrap();
        assert!(toks.iter().any(|t| matches!(&t.tok, Tok::Ident(s) if s == "abc")));
    }

    #[test]
    fn skips_comments() {
        let toks = tokenize("// x\n/* y */ final int z;").unwrap();
        assert!(matches!(toks[0].tok, Tok::Kw("final")));
    }
}
