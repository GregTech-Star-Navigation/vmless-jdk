//! token 种类、关键字表与 token 值。

use std::fmt;

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Ident(String),
    Kw(&'static str),
    IntLit(String),
    FloatLit(String),
    CharLit(String),
    StrLit(String),
    TextBlock(String),

    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Semi,
    Comma,
    Dot,
    Ellipsis,
    At,
    ColonColon,
    Colon,

    Assign,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Amp,
    Bar,
    Caret,
    Bang,
    Tilde,
    Lt,
    Gt,
    Le,
    Ge,
    EqEq,
    Ne,
    AndAnd,
    OrOr,
    Shl,
    Shr,
    Ushr,

    PlusAssign,
    MinusAssign,
    StarAssign,
    SlashAssign,
    PercentAssign,
    AmpAssign,
    BarAssign,
    CaretAssign,
    ShlAssign,
    ShrAssign,
    UshrAssign,

    PlusPlus,
    MinusMinus,
    Question,
    Arrow,
    Eof,
}

impl fmt::Display for Tok {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tok::Ident(s) => write!(f, "标识符 `{s}`"),
            Tok::Kw(k) => write!(f, "关键字 `{k}`"),
            Tok::IntLit(s) | Tok::FloatLit(s) => write!(f, "数字 `{s}`"),
            Tok::StrLit(s) => write!(f, "字符串 `{s}`"),
            Tok::TextBlock(_) => write!(f, "文本块"),
            Tok::CharLit(s) => write!(f, "字符 `{s}`"),
            other => write!(f, "{other:?}"),
        }
    }
}

pub const KEYWORDS: &[&str] = &[
    "abstract", "assert", "boolean", "break", "byte", "case", "catch", "char", "class", "const",
    "continue", "default", "do", "double", "else", "enum", "extends", "final", "finally", "float",
    "for", "goto", "if", "implements", "import", "instanceof", "int", "interface", "long", "native",
    "new", "package", "private", "protected", "public", "return", "short", "static", "strictfp",
    "super", "switch", "synchronized", "this", "throw", "throws", "transient", "try", "void",
    "volatile", "while", "true", "false", "null",
];

pub struct Token {
    pub tok: Tok,
    pub line: u32,
}
