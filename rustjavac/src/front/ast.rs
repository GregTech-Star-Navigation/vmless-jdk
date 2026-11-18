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

/// 类型参数（`<T>` / `<T extends Number>`）。
///
/// 泛型只做**擦除**：`sema::erase` 把引用这个参数的类型全换成它的上界
/// （没有上界就是 `Object`）。上界要到那时候才有用，所以解析阶段不能只记名字。
#[derive(Clone, Debug, PartialEq)]
pub struct TypeParam {
    pub name: String,
    pub bound: Option<Type>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypeDecl {
    pub kind: TypeKind,
    pub name: String,
    pub modifiers: Vec<String>,
    pub type_params: Vec<TypeParam>,
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
    /// 枚举常量（`RED` / `GREEN(2)`）。`sema::enums` 会把它降成
    /// 「静态 final 字段 + 构造调用」，之后的阶段看不到这个变体。
    EnumConst { name: String, args: Vec<Expr> },
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
    pub type_params: Vec<TypeParam>,
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
    /// 带标签的语句（`outer: for (...) { … }`）。标签只对 `break`/`continue` 可见。
    Labeled { label: String, body: Box<Stmt> },
    Switch { selector: Expr, cases: Vec<SwitchCase> },
    /// `yield e;` —— switch 表达式里往外交值（只在 `Expr::SwitchExpr` 的语句树里合法）
    Yield(Expr),
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
/// `catch (A | B e)` 的多重捕获：`types` 多个元素时，同一段保护区对每个类型
/// 各记一条异常表项（catch_type 不同，落到同一段 handler 代码）。
pub struct Catch {
    pub types: Vec<Type>,
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
    /// **块体 lambda**：`(a, b) -> { … }`。语句树放在这里，codegen 会把它编成一个
    /// 合成方法 `lambda$<方法>$N`。
    LambdaStmt { params: Vec<String>, body: Box<Stmt> },
    MethodRef { recv: Box<Expr>, name: String },
    ClassLit(Type),
    /// `switch (...) { … }` 的**表达式**形态：里面是一棵语句树（`yield e;` 是 `Stmt::Yield`）。
    /// 语义上它算出一个值，codegen 用一个临时局部收值。
    SwitchExpr(Box<Stmt>),
}
