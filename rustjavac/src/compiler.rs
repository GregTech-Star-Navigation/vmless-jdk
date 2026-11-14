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
    /// 标签 id -> 代码偏移
    labels: Vec<Option<usize>>,
    /// (操作数位置, 标签 id)，收尾时回填
    patches: Vec<(usize, usize)>,
    /// 需要 StackMapTable 帧的位置及其局部变量快照
    frames: Vec<(usize, Vec<u16>)>,
    /// 常量池里 "StackMapTable" 的索引
    stack_map_name: u16,
    /// 每个局部变量槽的验证类型：0 = int，否则是类索引（对象/数组）
    local_types: Vec<u16>,
}

impl Emit {
    fn new(max_locals: u16) -> Self {
        Emit {
            bytes: Vec::new(),
            depth: 0,
            max_depth: 0,
            max_locals,
            labels: Vec::new(),
            patches: Vec::new(),
            frames: Vec::new(),
            stack_map_name: 0,
            local_types: vec![u16::MAX; max_locals as usize],
        }
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

    fn istore(&mut self, slot: u8) -> Result<(), CompileError> {
        match slot {
            0..=3 => self.op(0x3B + slot),
            _ => return err(format!("局部变量槽 {slot} 超出当前支持范围")),
        }
        self.pop(1);
        Ok(())
    }

    /// 引用类型的存储（astore）
    fn astore(&mut self, slot: u8) -> Result<(), CompileError> {
        match slot {
            0..=3 => self.op(0x4B + slot),
            _ => return err(format!("局部变量槽 {slot} 超出当前支持范围")),
        }
        self.pop(1);
        Ok(())
    }

    /// 引用类型的读取（aload）
    fn aload(&mut self, slot: u8) -> Result<(), CompileError> {
        match slot {
            0..=3 => self.op(0x2A + slot),
            _ => return err(format!("局部变量槽 {slot} 超出当前支持范围")),
        }
        self.push(1);
        Ok(())
    }

    fn new_label(&mut self) -> usize {
        self.labels.push(None);
        self.labels.len() - 1
    }

    fn set_local_type(&mut self, slot: usize, ty: u16) {
        if slot >= self.local_types.len() {
            self.local_types.resize(slot + 1, u16::MAX);
        }
        self.local_types[slot] = ty;
    }

    fn bind(&mut self, label: usize) {
        let off = self.bytes.len();
        self.labels[label] = Some(off);
        self.frames.push((off, self.local_types.clone()));
    }

    fn branch(&mut self, opcode: u8, label: usize) {
        // 记录**操作码**位置：JVM 的跳转偏移是相对操作码地址的
        let pos = self.bytes.len();
        self.op(opcode);
        self.bytes.extend_from_slice(&[0, 0]);
        self.patches.push((pos, label));
    }

    fn goto(&mut self, label: usize) {
        self.branch(0xA7, label);
    }

    fn resolve(&mut self) -> Result<(), CompileError> {
        let patches = std::mem::take(&mut self.patches);
        for (pos, label) in patches {
            let target = self
                .labels
                .get(label)
                .and_then(|o| *o)
                .ok_or_else(|| CompileError { msg: format!("标签 {label} 未绑定") })?;
            let off = target as i32 - pos as i32;
            let off = i16::try_from(off).map_err(|_| CompileError { msg: "跳转距离超出 16 位".into() })?;
            self.bytes[pos + 1..pos + 3].copy_from_slice(&off.to_be_bytes());
        }
        self.frames.sort_unstable_by_key(|(o, _)| *o);
        self.frames.dedup_by_key(|(o, _)| *o);
        Ok(())
    }

    /// 生成 StackMapTable（全部用 full_frame，locals 全按 int）
    fn stack_map(&self) -> Option<Vec<u8>> {
        if self.frames.is_empty() {
            return None;
        }
        let mut info = Vec::new();
        info.extend_from_slice(&(self.frames.len() as u16).to_be_bytes());
        let mut prev: Option<usize> = None;
        for (off, snapshot) in &self.frames {
            let off = *off;
            let delta = match prev {
                None => off as u16,
                Some(p) => (off - p - 1) as u16,
            };
            prev = Some(off);
            info.push(255); // full_frame
            info.extend_from_slice(&delta.to_be_bytes());
            info.extend_from_slice(&self.max_locals.to_be_bytes());
            let locals = &snapshot;
            for slot in 0..self.max_locals as usize {
                let t = *locals.get(slot).unwrap_or(&u16::MAX);
                if t == u16::MAX {
                    info.push(0); // ITEM_Top（尚未初始化）
                } else if t == 0 {
                    info.push(1); // ITEM_Integer
                } else {
                    info.push(7); // ITEM_Object
                    info.extend_from_slice(&t.to_be_bytes());
                }
            }
            info.extend_from_slice(&0u16.to_be_bytes()); // 空操作数栈
        }
        Some(info)
    }

    fn to_attribute(&self, name_index: u16) -> Attribute {
        let mut info = Vec::new();
        info.extend_from_slice(&self.max_depth.to_be_bytes());
        info.extend_from_slice(&self.max_locals.to_be_bytes());
        info.extend_from_slice(&(self.bytes.len() as u32).to_be_bytes());
        info.extend_from_slice(&self.bytes);
        info.extend_from_slice(&0u16.to_be_bytes()); // exception_table_length
        match self.stack_map() {
            Some(sm) => {
                info.extend_from_slice(&1u16.to_be_bytes()); // attributes_count
                info.extend_from_slice(&self.stack_map_name.to_be_bytes());
                info.extend_from_slice(&(sm.len() as u32).to_be_bytes());
                info.extend_from_slice(&sm);
            }
            None => info.extend_from_slice(&0u16.to_be_bytes()),
        }
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

struct Ctx {
    locals: HashMap<String, u8>,
    next_slot: u8,
    /// 本类里的静态方法名 → 描述符（用于 invokestatic）
    static_methods: HashMap<String, String>,
    /// 本类实例字段：名字 → 下标
    fields: HashMap<String, usize>,
    /// 字段下标 → 描述符
    field_descs: Vec<String>,
    /// 局部变量名 → 是否引用类型
    local_ref: HashMap<String, bool>,
    /// 当前类名（内部形式 java/lang/X）
    class_name: String,
    is_static: bool,
}

impl Ctx {
    fn lookup(&self, name: &str) -> Option<u8> {
        self.locals.get(name).copied()
    }
}

fn is_println_call(e: &Expr) -> bool {
    matches!(e, Expr::Call { name, .. } if name == "println")
}

fn compile_expr(e: &Expr, em: &mut Emit, ctx: &Ctx, cp: &mut CpBuilder) -> Result<(), CompileError> {
    match e {
        Expr::IntLit(s) => {
            let v: i32 = s.parse().map_err(|_| CompileError { msg: format!("整数字面量 `{s}` 解析失败") })?;
            em.const_int(v);
            Ok(())
        }
        Expr::This => {
            em.op(0x2A); // aload_0
            em.push(1);
            Ok(())
        }
        Expr::Field(recv, name) if name == "length" && !ctx.fields.contains_key(name) => {
            compile_expr(recv, em, ctx, cp)?;
            em.op(0xBE); // arraylength
            Ok(())
        }
        Expr::Field(recv, name) => {
            compile_expr(recv, em, ctx, cp)?;
            let idx = *ctx
                .fields
                .get(name)
                .ok_or_else(|| CompileError { msg: format!("未知字段 `{name}`") })?;
            let desc = ctx.field_descs[idx].clone();
            let owner = ctx.class_name.clone();
            let f = cp.field_ref(&owner, name, &desc);
            em.op_u2(0xB4, f); // getfield
            Ok(())
        }
        Expr::ArrayNew { elem, dims } => {
            if !matches!(elem, Type::Prim("int")) {
                return err("数组目前只支持 int");
            }
            if dims.len() != 1 {
                return err("只支持一维数组");
            }
            compile_expr(&dims[0], em, ctx, cp)?;
            em.op(0xBC); // newarray
            em.bytes.push(10); // T_INT
            Ok(())
        }
        Expr::ArrayAccess { array, index } => {
            compile_expr(array, em, ctx, cp)?;
            compile_expr(index, em, ctx, cp)?;
            em.op(0x2E); // iaload
            em.pop(1);
            Ok(())
        }
        Expr::New { ty, args } => {
            let owner = match ty {
                Type::Named { path, .. } => path.join("/"),
                _ => return err("只支持创建对象"),
            };
            let arg_desc: String = args.iter().map(|a| match a {
                Expr::IntLit(_) | Expr::Ident(_) | Expr::Binary { .. } => "I".to_string(),
                Expr::StrLit(_) => "Ljava/lang/String;".to_string(),
                _ => "I".to_string(),
            }).collect();
            let ctor_desc = format!("({arg_desc})V");
            let cls = cp.class(&owner);
            em.op_u2(0xBB, cls); // new
            em.push(1);
            em.op(0x59); // dup
            em.push(1);
            for a in args {
                compile_expr(a, em, ctx, cp)?;
            }
            let init = cp.method_ref(&owner, "<init>", &ctor_desc);
            em.op_u2(0xB7, init); // invokespecial
            let n = args.len() as u16 + 1;
            em.pop(n);
            Ok(())
        }
        Expr::StrLit(s) => {
            let idx = cp.string(s);
            em.op_u2(0x13, idx);
            em.push(1);
            Ok(())
        }
        Expr::Ident(name) => {
            if let Some(slot) = ctx.lookup(name) {
                if *ctx.local_ref.get(name).unwrap_or(&false) {
                    em.aload(slot)?;
                } else {
                    em.iload(slot)?;
                }
                return Ok(());
            }
            // 隐式 this 字段
            if let Some(idx) = ctx.fields.get(name).copied() {
                em.op(0x2A); // aload_0
                em.push(1);
                let desc = ctx.field_descs[idx].clone();
                let owner = ctx.class_name.clone();
                let f = cp.field_ref(&owner, name, &desc);
                em.op_u2(0xB4, f);
                return Ok(());
            }
            err(format!("未定义的变量 `{name}`"))
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
                        let desc = if matches!(&args[0], Expr::StrLit(_)) {
                            "(Ljava/lang/String;)V"
                        } else {
                            "(I)V"
                        };
                        let println = cp.method_ref("java/io/PrintStream", "println", desc);
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

/// 把 `a < b` 这类条件编成「条件为假时跳到 label」。
fn compile_cond_false(
    e: &Expr,
    em: &mut Emit,
    ctx: &Ctx,
    cp: &mut CpBuilder,
    label: usize,
) -> Result<(), CompileError> {
    if let Expr::Binary { op, lhs, rhs } = e {
        let code = match op {
            BinOp::Lt => 0xA2u8, // if_icmpge
            BinOp::Ge => 0xA1,   // if_icmplt
            BinOp::Gt => 0xA4,   // if_icmple
            BinOp::Le => 0xA3,   // if_icmpgt
            BinOp::Eq => 0xA0,   // if_icmpne
            BinOp::Ne => 0x9F,   // if_icmpeq
            other => return err(format!("暂不支持的比较: {other:?}")),
        };
        compile_expr(lhs, em, ctx, cp)?;
        compile_expr(rhs, em, ctx, cp)?;
        em.pop(2);
        em.branch(code, label);
        return Ok(());
    }
    err("暂不支持的条件表达式")
}

fn compile_stmt(s: &Stmt, em: &mut Emit, ctx: &mut Ctx, cp: &mut CpBuilder) -> Result<bool, CompileError> {
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
        Stmt::Expr(e) if !matches!(e, Expr::Assign { .. }) => {
            compile_expr(e, em, ctx, cp)?;
            em.op(0x57); // pop
            em.pop(1);
            Ok(false)
        }
        Stmt::LocalVar { ty, name, init } => {
            let slot = ctx.next_slot;
            ctx.next_slot += 1;
            if slot >= 4 {
                return err("局部变量超过 4 个，暂不支持");
            }
            if em.max_locals < ctx.next_slot as u16 {
                em.max_locals = ctx.next_slot as u16;
            }
            ctx.locals.insert(name.clone(), slot);
            let is_ref = !matches!(ty, Type::Prim(_));
            ctx.local_ref.insert(name.clone(), is_ref);
            let vt = ver_type(&mut *cp, ty);
            em.set_local_type(slot as usize, vt);
            if let Some(e) = init {
                compile_expr(e, em, ctx, cp)?;
                if is_ref { em.astore(slot)?; } else { em.istore(slot)?; }
            }
            Ok(false)
        }
        Stmt::Expr(Expr::Assign { op: None, lhs, rhs }) if matches!(&**lhs, Expr::ArrayAccess { .. }) => {
            let (arr, idx) = match &**lhs {
                Expr::ArrayAccess { array, index } => (array.clone(), index.clone()),
                _ => unreachable!(),
            };
            compile_expr(&arr, em, ctx, cp)?;
            compile_expr(&idx, em, ctx, cp)?;
            compile_expr(rhs, em, ctx, cp)?;
            em.op(0x4F); // iastore
            em.pop(3);
            Ok(false)
        }
        Stmt::Expr(Expr::Assign { op: None, lhs, rhs }) if matches!(&**lhs, Expr::Field(..)) => {
            let (recv, fname) = match &**lhs {
                Expr::Field(r, n) => (r.clone(), n.clone()),
                _ => unreachable!(),
            };
            compile_expr(&recv, em, ctx, cp)?;
            compile_expr(rhs, em, ctx, cp)?;
            let idx = *ctx
                .fields
                .get(&fname)
                .ok_or_else(|| CompileError { msg: format!("未知字段 `{fname}`") })?;
            let desc = ctx.field_descs[idx].clone();
            let owner = ctx.class_name.clone();
            let f = cp.field_ref(&owner, &fname, &desc);
            em.op_u2(0xB5, f); // putfield
            em.pop(2);
            Ok(false)
        }
        Stmt::Expr(Expr::Assign { op: None, lhs, rhs }) => {
            let name = match &**lhs {
                Expr::Ident(n) => n.clone(),
                _ => return err("只支持给局部变量赋值"),
            };
            let slot = ctx
                .lookup(&name)
                .ok_or_else(|| CompileError { msg: format!("未定义的变量 `{name}`") })?;
            compile_expr(rhs, em, ctx, cp)?;
            em.istore(slot)?;
            Ok(false)
        }
        Stmt::For { init, cond, update, body } => {
            for s in init {
                compile_stmt(s, em, ctx, cp)?;
            }
            let start = em.new_label();
            let end = em.new_label();
            em.bind(start);
            if let Some(c) = cond {
                compile_cond_false(c, em, ctx, cp, end)?;
            }
            compile_stmt(body, em, ctx, cp)?;
            for u in update {
                // 更新表达式可能是赋值，交给语句编译器处理
                compile_stmt(&Stmt::Expr(u.clone()), em, ctx, cp)?;
            }
            em.goto(start);
            em.bind(end);
            Ok(false)
        }
        Stmt::While { cond, body } => {
            let start = em.new_label();
            let end = em.new_label();
            em.bind(start);
            compile_cond_false(cond, em, ctx, cp, end)?;
            compile_stmt(body, em, ctx, cp)?;
            em.goto(start);
            em.bind(end);
            Ok(false)
        }
        Stmt::If { cond, then, els } => {
            let l_else = em.new_label();
            let l_end = em.new_label();
            compile_cond_false(cond, em, ctx, cp, l_else)?;
            compile_stmt(then, em, ctx, cp)?;
            match els {
                Some(e) => {
                    em.goto(l_end);
                    em.bind(l_else);
                    compile_stmt(e, em, ctx, cp)?;
                    em.bind(l_end);
                }
                None => em.bind(l_else),
            }
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
    let mut static_sigs: HashMap<String, String> = HashMap::new();
    for m in &t.members {
        if let Member::Method(md) = m {
            if md.modifiers.iter().any(|x| x == "static") {
                static_sigs.insert(md.name.clone(), method_descriptor(&md.params, &md.ret));
            }
        }
    }

    // 实例字段：名字 → 下标，以及每个下标的描述符
    let mut field_index: HashMap<String, usize> = HashMap::new();
    let mut field_desc_list: Vec<String> = Vec::new();
    for m in &t.members {
        if let Member::Field(f) = m {
            if f.modifiers.iter().any(|x| x == "static") {
                continue;
            }
            field_index.insert(f.name.clone(), field_desc_list.len());
            field_desc_list.push(descriptor(&f.ty));
        }
    }

    let mut cp = CpBuilder::new();
    let this_class = cp.class(&self_name);
    let super_class = cp.class("java/lang/Object");
    let code_name = cp.utf8("Code");
    let stack_map_name = cp.utf8("StackMapTable");

    let has_ctor = t.members.iter().any(|m| matches!(m, Member::Method(md) if md.ret.is_none() && md.name == t.name));

    let mut methods: Vec<CfMember> = Vec::new();
    let mut fields: Vec<CfMember> = Vec::new();
    for m in &t.members {
        if let Member::Field(f) = m {
            if f.modifiers.iter().any(|x| x == "static") {
                continue;
            }
            let n = cp.utf8(&f.name);
            let d = cp.utf8(&descriptor(&f.ty));
            fields.push(CfMember {
                access_flags: 0x0000,
                name_index: n,
                descriptor_index: d,
                attributes: Vec::new(),
            });
        }
    }

    let obj_init = cp.method_ref("java/lang/Object", "<init>", "()V");
    if !has_ctor {
        // 默认构造器
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
    }

    for m in &t.members {
        let md = match m {
            Member::Method(md) => md,
            _ => continue,
        };
        let is_static = md.modifiers.iter().any(|x| x == "static");
        let desc = method_descriptor(&md.params, &md.ret);

        let mut em = Emit::new(if is_static { md.params.len() as u16 } else { md.params.len() as u16 + 1 });
        let is_ctor_m = md.ret.is_none() && md.name == t.name;
        if is_ctor_m {
            // 构造器必须先调用父类构造器
            em.op(0x2A);
            em.push(1);
            em.op_u2(0xB7, obj_init);
            em.pop(1);
        }
        let mut ctx = Ctx {
            locals: HashMap::new(),
            next_slot: if is_static { 0 } else { 1 },
            static_methods: static_sigs.clone(),
            fields: field_index.clone(),
            field_descs: field_desc_list.clone(),
            local_ref: HashMap::new(),
            class_name: self_name.clone(),
            is_static,
        };
        ctx.locals.insert("<this-class>".to_string(), 0);
        if !is_static {
            let this_cp = cp.class(&self_name);
            em.set_local_type(0, this_cp);
        }
        let mut slot = ctx.next_slot;
        for p in &md.params {
            let vt = ver_type(&mut cp, &p.ty);
            em.set_local_type(slot as usize, vt);
            ctx.locals.insert(p.name.clone(), slot);
            if !matches!(p.ty, Type::Prim(_)) {
                ctx.local_ref.insert(p.name.clone(), true);
            }
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

        em.stack_map_name = stack_map_name;
        em.resolve()?;
        let is_ctor = md.ret.is_none() && md.name == t.name;
        let method_name = if is_ctor { "<init>" } else { md.name.as_str() };
        let name_index = cp.utf8(method_name);
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
        fields,
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

/// 类型的验证类型编码：int → 0，对象/数组 → 常量池里的类索引
fn ver_type(cp: &mut CpBuilder, ty: &Type) -> u16 {
    match ty {
        Type::Prim(_) => 0,
        Type::Array(inner) => {
            let d = descriptor(&Type::Array(inner.clone()));
            cp.class(&d)
        }
        Type::Named { path, .. } => {
            let joined = if path.len() == 1 && is_java_lang(&path[0]) {
                format!("java/lang/{}", path[0])
            } else {
                path.join("/")
            };
            cp.class(&joined)
        }
        Type::Var => 0,
    }
}
