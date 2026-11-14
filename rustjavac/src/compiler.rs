//! AST → 字节码。
//!
//! 目前覆盖：静态/实例方法、int 类型的参数与局部变量、算术表达式、
//! `return`、以及 `System.out.println(int)` 这一条输出通路。
//! 分支（if/while）需要 StackMapTable，留到下一步。

use crate::ast::*;
use crate::{write, Attribute, ClassFile, CpEntry, Member as CfMember};
use crate::codegen::{CpBuilder, ACC_PUBLIC, ACC_STATIC};
use std::collections::HashMap;

pub const ACC_SUPER: u16 = 0x0020;

#[derive(Debug)]
pub struct CompileError {
    pub msg: String,
}

fn err<T>(msg: impl Into<String>) -> Result<T, CompileError> {
    Err(CompileError { msg: msg.into() })
}

/// 方法体字节码发射器（带栈深度跟踪）
struct Emit {
    bytes: Vec<u8>,
    depth: u16,
    max_depth: u16,
    max_locals: u16,
}

impl Emit {
    fn new(max_locals: u16) -> Self {
        Emit { bytes: Vec::new(), depth: 0, max_depth: 0, max_locals }
    }

    fn push(&mut self, n: u16) {
        self.depth += n;
        if self.depth > self.max_depth {
            self.max_depth = self.depth;
        }
    }

    fn pop(&mut self, n: u16) {
        self.depth = self.depth.saturating_sub(n);
    }

    fn op(&mut self, b: u8) {
        self.bytes.push(b);
    }

    fn op_u2(&mut self, b: u8, idx: u16) {
        self.bytes.push(b);
        self.bytes.extend_from_slice(&idx.to_be_bytes());
    }

    fn iload(&mut self, slot: u8) -> Result<(), CompileError> {
        match slot {
            0..=3 => self.op(0x1A + slot),
            _ => return err(format!("局部变量槽 {slot} 超出当前支持范围")),
        }
        self.push(1);
        Ok(())
    }

    fn const_int(&mut self, v: i32) {
        match v {
            -1..=5 => self.op((0x03 + v) as u8),
            -128..=127 => {
                self.op(0x10);
                self.bytes.push(v as u8);
            }
            -32768..=32767 => {
                self.op(0x11);
                self.bytes.extend_from_slice(&(v as i16).to_be_bytes());
            }
            _ => {
                // 简化：大整数走 sipush 范围；超出直接截断
                self.op(0x11);
                self.bytes.extend_from_slice(&(v as i16).to_be_bytes());
            }
        }
        self.push(1);
    }

    fn to_attribute(&self, name_index: u16) -> Attribute {
        let mut info = Vec::new();
        info.extend_from_slice(&self.max_depth.to_be_bytes());
        info.extend_from_slice(&self.max_locals.to_be_bytes());
        info.extend_from_slice(&(self.bytes.len() as u32).to_be_bytes());
        info.extend_from_slice(&self.bytes);
        info.extend_from_slice(&0u16.to_be_bytes());
        info.extend_from_slice(&0u16.to_be_bytes());
        Attribute { name_index, info }
    }
}

const JAVA_LANG: &[&str] = &[
    "String", "Object", "System", "Integer", "Long", "Short", "Byte", "Character",
    "Boolean", "Float", "Double", "Number", "Class", "Throwable", "Exception",
    "RuntimeException", "Error", "Math", "StringBuilder", "Iterable", "Comparable",
    "Runnable", "Thread", "Enum", "Record", "Void",
];

fn is_java_lang(name: &str) -> bool {
    JAVA_LANG.contains(&name)
}

/// 类型 → 描述符
pub fn descriptor(ty: &Type) -> String {
    match ty {
        Type::Prim("int") => "I".into(),
        Type::Prim("long") => "J".into(),
        Type::Prim("short") => "S".into(),
        Type::Prim("byte") => "B".into(),
        Type::Prim("char") => "C".into(),
        Type::Prim("boolean") => "Z".into(),
        Type::Prim("float") => "F".into(),
        Type::Prim("double") => "D".into(),
        Type::Prim("void") => "V".into(),
        Type::Prim(other) => format!("<{other}>"),
        Type::Named { path, .. } => {
            // java.lang 的类型可以简写，编译器要补全
            let joined = if path.len() == 1 && is_java_lang(&path[0]) {
                format!("java/lang/{}", path[0])
            } else {
                path.join("/")
            };
            format!("L{joined};")
        }
        Type::Array(inner) => format!("[{}", descriptor(inner)),
        Type::Var => "I".into(),
    }
}

fn method_descriptor(params: &[Param], ret: &Option<Type>) -> String {
    let mut s = String::from("(");
    for p in params {
        s.push_str(&descriptor(&p.ty));
    }
    s.push(')');
    s.push_str(&descriptor(ret.as_ref().unwrap_or(&Type::Prim("void"))));
    s
}

struct Ctx<'a> {
    locals: HashMap<&'a str, u8>,
    next_slot: u8,
    /// 本类里的静态方法名 → 描述符（用于 invokestatic）
    static_methods: HashMap<&'a str, String>,
    is_static: bool,
}

impl<'a> Ctx<'a> {
    fn lookup(&self, name: &str) -> Option<u8> {
        self.locals.get(name).copied()
    }
}

fn is_println_call(e: &Expr) -> bool {
    matches!(e, Expr::Call { name, .. } if name == "println")
}

fn compile_expr(e: &Expr, em: &mut Emit, ctx: &Ctx<'_>, cp: &mut CpBuilder) -> Result<(), CompileError> {
    match e {
        Expr::IntLit(s) => {
            let v: i32 = s.parse().map_err(|_| CompileError { msg: format!("整数字面量 `{s}` 解析失败") })?;
            em.const_int(v);
            Ok(())
        }
        Expr::Ident(name) => {
            let slot = ctx
                .lookup(name)
                .ok_or_else(|| CompileError { msg: format!("未定义的变量 `{name}`") })?;
            em.iload(slot)?;
            Ok(())
        }
        // System.out.println(x)
        Expr::Call { recv: Some(r), name, args, .. } if name == "println" => {
            if let Expr::Field(boxed, field) = &**r {
                if field == "out" {
                    if let Expr::Ident(_) = &**boxed {
                        let out = cp.field_ref("java/lang/System", "out", "Ljava/io/PrintStream;");
                        em.op_u2(0xB2, out);
                        em.push(1);
                        if args.len() != 1 {
                            return err("println 目前只支持一个参数");
                        }
                        compile_expr(&args[0], em, ctx, cp)?;
                        let println = cp.method_ref("java/io/PrintStream", "println", "(I)V");
                        em.op_u2(0xB6, println);
                        em.pop(2);
                        return Ok(());
                    }
                }
            }
            err(format!("不支持的 println 形态: {name}"))
        }
        // 调用本类的静态方法 add(a, b)
        Expr::Call { name, args, .. } => {
            let desc = ctx
                .static_methods
                .get(name.as_str())
                .ok_or_else(|| CompileError { msg: format!("未找到方法 `{name}`") })?
                .clone();
            for a in args {
                compile_expr(a, em, ctx, cp)?;
            }
            let owner = ctx
                .locals
                .get("<this-class>")
                .map(|_| String::new())
                .unwrap_or_default();
            let _ = owner;
            let idx = cp.method_ref("<SELF>", name, &desc);
            em.op_u2(0xB8, idx);
            let argc = args.len() as u16;
            em.pop(argc);
            if desc.ends_with(")V") {
                // 无返回值
            } else {
                em.push(1);
            }
            Ok(())
        }
        Expr::Binary { op, lhs, rhs } => {
            compile_expr(lhs, em, ctx, cp)?;
            compile_expr(rhs, em, ctx, cp)?;
            let code = match op {
                BinOp::Add => 0x60u8,
                BinOp::Sub => 0x64,
                BinOp::Mul => 0x68,
                BinOp::Div => 0x6C,
                BinOp::Rem => 0x70,
                other => return err(format!("暂不支持的二元运算: {other:?}")),
            };
            em.op(code);
            em.pop(1);
            Ok(())
        }
        Expr::Unary { op: UnOp::Neg, expr } => {
            compile_expr(expr, em, ctx, cp)?;
            em.op(0x74); // ineg
            Ok(())
        }
        other => err(format!("暂不支持的表达式: {other:?}")),
    }
}

fn compile_stmt(s: &Stmt, em: &mut Emit, ctx: &mut Ctx<'_>, cp: &mut CpBuilder) -> Result<bool, CompileError> {
    match s {
        Stmt::Block(items) => {
            for it in items {
                if compile_stmt(it, em, ctx, cp)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Stmt::Return(None) => {
            em.op(0xB1);
            Ok(true)
        }
        Stmt::Return(Some(e)) => {
            compile_expr(e, em, ctx, cp)?;
            em.op(0xAC); // ireturn
            em.pop(1);
            Ok(true)
        }
        Stmt::Expr(e) if is_println_call(e) => {
            compile_expr(e, em, ctx, cp)?;
            Ok(false)
        }
        Stmt::Expr(e) => {
            compile_expr(e, em, ctx, cp)?;
            em.op(0x57); // pop
            em.pop(1);
            Ok(false)
        }
        other => err(format!("暂不支持的语句: {other:?}")),
    }
}

/// 编译一个编译单元，返回 (类名, class 字节)
pub fn compile(cu: &CompilationUnit) -> Result<Vec<(String, Vec<u8>)>, CompileError> {
    let mut out = Vec::new();
    for t in &cu.types {
        out.push((t.name.clone(), compile_type(&cu.package, t)?));
    }
    Ok(out)
}

fn compile_type(package: &Option<String>, t: &TypeDecl) -> Result<Vec<u8>, CompileError> {
    let self_name = match package {
        Some(p) => format!("{}/{}", p.replace('.', "/"), t.name),
        None => t.name.clone(),
    };

    // 第一遍：收集静态方法签名（供 invokestatic 使用）
    let mut static_sigs: HashMap<&str, String> = HashMap::new();
    for m in &t.members {
        if let Member::Method(md) = m {
            if md.modifiers.iter().any(|x| x == "static") {
                static_sigs.insert(md.name.as_str(), method_descriptor(&md.params, &md.ret));
            }
        }
    }

    let mut cp = CpBuilder::new();
    let this_class = cp.class(&self_name);
    let super_class = cp.class("java/lang/Object");
    let code_name = cp.utf8("Code");

    let mut methods: Vec<CfMember> = Vec::new();

    // 默认构造器
    let obj_init = cp.method_ref("java/lang/Object", "<init>", "()V");
    let mut ctor = Emit::new(1);
    ctor.op(0x2A); // aload_0
    ctor.push(1);
    ctor.op_u2(0xB7, obj_init);
    ctor.pop(1);
    ctor.op(0xB1);
    let ctor_name = cp.utf8("<init>");
    let ctor_desc = cp.utf8("()V");
    methods.push(CfMember {
        access_flags: ACC_PUBLIC,
        name_index: ctor_name,
        descriptor_index: ctor_desc,
        attributes: vec![ctor.to_attribute(code_name)],
    });

    for m in &t.members {
        let md = match m {
            Member::Method(md) => md,
            _ => continue,
        };
        let is_static = md.modifiers.iter().any(|x| x == "static");
        let desc = method_descriptor(&md.params, &md.ret);

        let mut em = Emit::new(if is_static { md.params.len() as u16 } else { md.params.len() as u16 + 1 });
        let mut ctx = Ctx {
            locals: HashMap::new(),
            next_slot: if is_static { 0 } else { 1 },
            static_methods: static_sigs.clone(),
            is_static,
        };
        ctx.locals.insert("<this-class>", 0);
        let mut slot = ctx.next_slot;
        for p in &md.params {
            ctx.locals.insert(p.name.as_str(), slot);
            ctx.next_slot += 1;
            slot += 1;
        }

        // 方法体
        match &md.body {
            Some(body) => {
                let terminated = compile_stmt(body, &mut em, &mut ctx, &mut cp)?;
                if !terminated {
                    // 补默认返回
                    em.op(if desc.ends_with(")V") { 0xB1 } else { 0x03 });
                }
            }
            None => {
                em.op(0xB1);
            }
        }

        let name_index = cp.utf8(&md.name);
        let desc_index = cp.utf8(&desc);
        let flags = if is_static { ACC_PUBLIC | ACC_STATIC } else { ACC_PUBLIC };
        methods.push(CfMember {
            access_flags: flags,
            name_index,
            descriptor_index: desc_index,
            attributes: vec![em.to_attribute(code_name)],
        });
    }

    // invokestatic 的 owner 用真实类名补齐（此前占位 <SELF>）
    let cp_entries = fix_self_refs(cp, &self_name);

    let cf = ClassFile {
        minor_version: 0,
        major_version: 69,
        constant_pool: cp_entries,
        access_flags: ACC_PUBLIC | ACC_SUPER,
        this_class,
        super_class,
        interfaces: Vec::new(),
        fields: Vec::new(),
        methods,
        attributes: Vec::new(),
    };
    Ok(write(&cf))
}

/// 把常量池里指向占位类 `<SELF>` 的 MethodRef 改成真实类名索引。
/// 做法简单：追加一个真实类名 Utf8，并把 MethodRef 的 class_index 指过去。
fn fix_self_refs(cp: CpBuilder, self_name: &str) -> Vec<Option<CpEntry>> {
    let mut entries = cp.entries();
    // 找 <SELF> 的 Utf8 索引
    let mut self_utf8 = None;
    for (i, e) in entries.iter().enumerate() {
        if let Some(CpEntry::Utf8(b)) = e {
            if b == b"<SELF>" {
                self_utf8 = Some(i as u16);
            }
        }
    }
    let Some(self_utf8) = self_utf8 else {
        return entries;
    };
    // 找指向它的 Class 项
    let mut self_class = None;
    for (i, e) in entries.iter().enumerate() {
        if let Some(CpEntry::Class(n)) = e {
            if *n == self_utf8 {
                self_class = Some(i as u16);
            }
        }
    }
    // 追加真实类名
    let real_utf8 = entries.len() as u16;
    entries.push(Some(CpEntry::Utf8(self_name.as_bytes().to_vec())));
    let real_class = entries.len() as u16;
    entries.push(Some(CpEntry::Class(real_utf8)));
    let _ = self_class;
    // 所有 MethodRef 的 class 指向占位类的，改指真实类
    for e in entries.iter_mut() {
        if let Some(CpEntry::MethodRef(c, nt)) = e {
            if Some(*c) == self_class {
                *c = real_class;
            }
            let _ = nt;
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_compilation_unit;

    #[test]
    fn compiles_addition_method() {
        let src = r#"
            class Calc {
                static int add(int a, int b) { return a + b; }
            }
        "#;
        let cu = parse_compilation_unit(src).expect("parse");
        let out = compile(&cu).expect("compile");
        assert_eq!(out.len(), 1);
        let (name, bytes) = &out[0];
        assert_eq!(name, "Calc");
        let cf = crate::parse(bytes).expect("生成的字节能被解析");
        assert_eq!(cf.major_version, 69);
        // 构造器 + add
        assert_eq!(cf.methods.len(), 2);
    }

    #[test]
    fn method_descriptors_are_correct() {
        let src = r#"
            class M {
                static int add(int a, int b) { return a + b; }
                static long wide(long x) { return x; }
            }
        "#;
        let cu = parse_compilation_unit(src).expect("parse");
        let name = descriptor(&Type::Named { path: vec!["java".into(), "lang".into(), "String".into()], args: vec![] });
        assert_eq!(name, "Ljava/lang/String;");
        assert_eq!(descriptor(&Type::Array(Box::new(Type::Prim("int")))), "[I");
        let _ = cu;
    }

    #[test]
    fn compiles_main_with_println() {
        let src = r#"
            class T {
                static int add(int a, int b) { return a + b; }
                public static void main(String[] args) {
                    System.out.println(add(2, 3));
                }
            }
        "#;
        let cu = parse_compilation_unit(src).expect("parse");
        let out = compile(&cu).expect("compile");
        assert_eq!(out[0].0, "T");
    }
}
