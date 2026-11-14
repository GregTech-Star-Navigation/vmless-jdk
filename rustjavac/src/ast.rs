//! 极简 AST：只保留后续语义分析与代码生成需要的结构。

#[derive(Clone, Debug, PartialEq)]
pub enum Type {
    Prim(&'static str),
    Named { path: Vec<String>, args: Vec<Type> },
    Array(Box<Type>),
    Var,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub ty: Type,
    pub name: String,
    pub varargs: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompilationUnit {
    pub package: Option<String>,
    pub imports: Vec<Import>,
    pub types: Vec<TypeDecl>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Import {
    pub is_static: bool,
    pub name: String,
    pub wildcard: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeKind {
    Class,
    Interface,
    Enum,
    Record,
    Annotation,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypeDecl {
    pub kind: TypeKind,
    pub name: String,
    pub modifiers: Vec<String>,
    pub type_params: Vec<String>,
    pub extends: Vec<Type>,
    pub implements: Vec<Type>,
    pub members: Vec<Member>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Member {
    Field(FieldDecl),
    Method(MethodDecl),
    Type(TypeDecl),
    Init { is_static: bool, body: Stmt },
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldDecl {
    pub modifiers: Vec<String>,
    pub ty: Type,
    pub name: String,
    pub init: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MethodDecl {
    pub modifiers: Vec<String>,
    pub type_params: Vec<String>,
    pub ret: Option<Type>,
    pub name: String,
    pub params: Vec<Param>,
    pub throws: Vec<Type>,
    pub body: Option<Stmt>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    Block(Vec<Stmt>),
    LocalVar { ty: Type, name: String, init: Option<Expr> },
    Expr(Expr),
    If { cond: Expr, then: Box<Stmt>, els: Option<Box<Stmt>> },
    While { cond: Expr, body: Box<Stmt> },
    DoWhile { body: Box<Stmt>, cond: Expr },
    For { init: Vec<Stmt>, cond: Option<Expr>, update: Vec<Expr>, body: Box<Stmt> },
    ForEach { ty: Type, name: String, iter: Expr, body: Box<Stmt> },
    Return(Option<Expr>),
    Throw(Expr),
    Break(Option<String>),
    Continue(Option<String>),
    Switch { selector: Expr, cases: Vec<SwitchCase> },
    Try { body: Box<Stmt>, catches: Vec<Catch>, finally: Option<Box<Stmt>> },
    Empty,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SwitchCase {
    pub labels: Vec<Option<Expr>>,
    pub body: Vec<Stmt>,
    pub arrow: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Catch {
    pub ty: Type,
    pub name: String,
    pub body: Stmt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add, Sub, Mul, Div, Rem,
    Shl, Shr, Ushr,
    Lt, Gt, Le, Ge, Eq, Ne,
    BitAnd, BitXor, BitOr,
    And, Or,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg, Pos, Not, BitNot, PreInc, PreDec, PostInc, PostDec,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    IntLit(String),
    FloatLit(String),
    StrLit(String),
    TextBlock(String),
    CharLit(String),
    BoolLit(bool),
    Null,
    This,
    Super,
    Ident(String),
    Field(Box<Expr>, String),
    Call { recv: Option<Box<Expr>>, name: String, args: Vec<Expr>, type_args: Vec<Type> },
    New { ty: Type, args: Vec<Expr> },
    ArrayNew { elem: Type, dims: Vec<Expr> },
    ArrayInit(Vec<Expr>),
    ArrayAccess { array: Box<Expr>, index: Box<Expr> },
    Binary { op: BinOp, lhs: Box<Expr>, rhs: Box<Expr> },
    Unary { op: UnOp, expr: Box<Expr> },
    Assign { op: Option<BinOp>, lhs: Box<Expr>, rhs: Box<Expr> },
    Cond { cond: Box<Expr>, then: Box<Expr>, els: Box<Expr> },
    Cast { ty: Type, expr: Box<Expr> },
    InstanceOf { expr: Box<Expr>, ty: Type, binding: Option<String> },
    Lambda { params: Vec<String>, body: Box<Expr> },
    MethodRef { recv: Box<Expr>, name: String },
    ClassLit(Type),
    SwitchExpr(Box<Expr>),
}
