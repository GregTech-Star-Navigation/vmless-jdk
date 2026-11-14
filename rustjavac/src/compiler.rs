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

// 局部变量 / 表达式的类型种类
const K_INT: u8 = 0;
const K_LONG: u8 = 1;
const K_REF: u8 = 2;
const K_DOUBLE: u8 = 3;
/// boolean：JVM 里与 int 同一套 load/store/算术字节码，只有 println 的重载不同
/// （println(Z) 打 "true"/"false"）
const K_BOOL: u8 = 4;

fn kind_wide(k: u8) -> bool {
    k == K_LONG || k == K_DOUBLE
}

/// 类型 → 种类
fn kind_of(ty: &Type) -> u8 {
    match ty {
        Type::Prim("long") => K_LONG,
        Type::Prim("double") => K_DOUBLE,
        Type::Prim("boolean") => K_BOOL,
        Type::Prim(_) => K_INT,
        _ => K_REF,
    }
}

/// 按描述符首字符判断种类（字段、方法返回值用）
fn kind_from_desc(desc: &str) -> u8 {
    match desc.as_bytes().first().copied() {
        Some(b'J') => K_LONG,
        Some(b'D') => K_DOUBLE,
        Some(b'Z') => K_BOOL,
        Some(b'L') | Some(b'[') => K_REF,
        _ => K_INT,
    }
}

/// 二元/比较运算的公共类型：double > long > int
fn promote(a: u8, b: u8) -> u8 {
    if a == K_DOUBLE || b == K_DOUBLE {
        K_DOUBLE
    } else if a == K_LONG || b == K_LONG {
        K_LONG
    } else {
        K_INT
    }
}

/// 按描述符的返回值判断种类（用于 Call 表达式）
///
/// 必须看**返回类型那一段**（`)` 之后）：直接看描述符最后一个字符会把
/// `()Ljava/lang/String;` 的 `;` 当成 int。
fn kind_from_ret(desc: &str) -> u8 {
    let ret = desc.rsplit(')').next().unwrap_or("");
    kind_from_desc(ret)
}

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
        if slot <= 3 {
            self.op(0x1A + slot);
        } else {
            self.op(0x15);
            self.bytes.push(slot);
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
        if slot <= 3 {
            self.op(0x3B + slot);
        } else {
            self.op(0x36);
            self.bytes.push(slot);
        }
        self.pop(1);
        Ok(())
    }

    /// long 的读取（lload）
    fn lload(&mut self, slot: u8) -> Result<(), CompileError> {
        if slot <= 3 { self.op(0x1E + slot); } else { self.op(0x16); self.bytes.push(slot); }
        self.push(2);
        Ok(())
    }

    /// long 的存储（lstore）
    fn lstore(&mut self, slot: u8) -> Result<(), CompileError> {
        if slot <= 3 { self.op(0x3F + slot); } else { self.op(0x37); self.bytes.push(slot); }
        self.pop(2);
        Ok(())
    }

    /// double 的读取（dload）
    fn dload(&mut self, slot: u8) -> Result<(), CompileError> {
        if slot <= 3 { self.op(0x26 + slot); } else { self.op(0x18); self.bytes.push(slot); }
        self.push(2);
        Ok(())
    }

    /// double 的存储（dstore）
    fn dstore(&mut self, slot: u8) -> Result<(), CompileError> {
        // 注意：0x38 是 fstore，dstore 是 0x39
        if slot <= 3 { self.op(0x47 + slot); } else { self.op(0x39); self.bytes.push(slot); }
        self.pop(2);
        Ok(())
    }

    /// 按种类发射 load
    fn load_slot(&mut self, slot: u8, kind: u8) -> Result<(), CompileError> {
        match kind {
            K_LONG => self.lload(slot),
            K_DOUBLE => self.dload(slot),
            K_REF => self.aload(slot),
            _ => self.iload(slot),
        }
    }

    /// 按种类发射 store
    fn store_slot(&mut self, slot: u8, kind: u8) -> Result<(), CompileError> {
        match kind {
            K_LONG => self.lstore(slot),
            K_DOUBLE => self.dstore(slot),
            K_REF => self.astore(slot),
            _ => self.istore(slot),
        }
    }

    /// 引用类型的存储（astore）
    fn astore(&mut self, slot: u8) -> Result<(), CompileError> {
        if slot <= 3 {
            self.op(0x4B + slot);
        } else {
            self.op(0x3A);
            self.bytes.push(slot);
        }
        self.pop(1);
        Ok(())
    }

    /// 引用类型的读取（aload）
    fn aload(&mut self, slot: u8) -> Result<(), CompileError> {
        if slot <= 3 {
            self.op(0x2A + slot);
        } else {
            self.op(0x19);
            self.bytes.push(slot);
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
        self.bind_t(label, None);
    }

    /// 绑定标签，并可选地把脏槽（`SCRATCH_SLOT`）的验证类型单独写进这一帧。
    ///
    /// 脏槽必须先写后读，所以「跳到假分支」那一帧里它还是 Top（未赋值），
    /// 「汇合点」那一帧里才是 Integer —— 全局的 `local_types` 只能有一个值，
    /// 表达不了这种差别，只能在绑定这一帧时单独覆盖。
    fn bind_t(&mut self, label: usize, scratch_ty: Option<u16>) {
        let off = self.bytes.len();
        self.labels[label] = Some(off);
        let mut snap = self.local_types.clone();
        if let Some(t) = scratch_ty {
            let s = SCRATCH_SLOT as usize;
            if snap.len() <= s {
                snap.resize(s + 1, u16::MAX);
            }
            snap[s] = t;
        }
        self.frames.push((off, snap));
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

    /// 把栈顶已经算好的比较结果用 `jump_false`（条件为假时跳）收尾成 0/1 压回栈上。
    ///
    /// 跳到 l_false 的是「假」分支（写 0），落空的是「真」分支（写 1）；
    /// 两个分支各自 `iconst` + `istore` 到脏槽，再在 L_end 汇合。
    ///
    /// 只在操作数栈为空的位置可用：本项目的 StackMapTable 一律声明「栈为 0」
    /// （`Emit::stack_map`），所以任何跳转点都不能压着别的值。
    fn bool_from_branch(&mut self, jump_false: u8) -> Result<(), CompileError> {
        if self.depth != 0 {
            return err("比较结果作为表达式时，前面不能有未消费的操作数（操作数栈必须为空）");
        }
        let s = SCRATCH_SLOT;
        if self.max_locals < s as u16 + 1 {
            self.max_locals = s as u16 + 1;
        }
        let l_false = self.new_label();
        let l_end = self.new_label();
        self.branch(jump_false, l_false);
        self.const_int(1);
        let _ = self.istore(s);
        self.goto(l_end);
        self.bind_t(l_false, Some(u16::MAX)); // 假分支上脏槽还没存过 → ITEM_Top
        self.const_int(0);
        let _ = self.istore(s);
        self.bind_t(l_end, Some(0)); // 汇合点上两条路径都存过 → ITEM_Integer
        let _ = self.iload(s);
        Ok(())
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

    /// 生成 StackMapTable（全部用 full_frame）
    ///
    /// locals 数组里 **一个变量占一个条目**：long/double 只写一个
    /// ITEM_Long/ITEM_Double，紧跟的第二个槽由 HotSpot 自己补 Top
    /// （stackMapTable.cpp 的 full_frame 分支会按 is_category2 自行展开，
    /// 再多写一个 ITEM_Top 就会让展开后的槽数超出 max_locals，
    /// 报 ClassFormatError: bad type array size）。
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
            let mut entries: Vec<u8> = Vec::new();
            let mut slot = 0usize;
            let mut n_entries = 0u16;
            while slot < self.max_locals as usize {
                let t = *snapshot.get(slot).unwrap_or(&u16::MAX);
                if t == 0xFFFE {
                    entries.push(4); // ITEM_Long，吃掉第 2 槽
                    slot += 2;
                } else if t == 0xFFFD {
                    entries.push(3); // ITEM_Double，吃掉第 2 槽
                    slot += 2;
                } else if t == u16::MAX {
                    entries.push(0); // ITEM_Top（尚未初始化）
                    slot += 1;
                } else if t == 0 {
                    entries.push(1); // ITEM_Integer
                    slot += 1;
                } else {
                    entries.push(7); // ITEM_Object
                    entries.extend_from_slice(&t.to_be_bytes());
                    slot += 1;
                }
                n_entries += 1;
            }
            info.push(255); // full_frame
            info.extend_from_slice(&delta.to_be_bytes());
            info.extend_from_slice(&n_entries.to_be_bytes());
            info.extend_from_slice(&entries);
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

/// 「比较 → 0/1」用的脏槽：放在局部变量表的最高固定位置。
/// 用前先用 `set_local_type(.., 0)` 声明成 int，`max_locals` 会跟着抬到 251。
const SCRATCH_SLOT: u8 = 250;

const JAVA_LANG: &[&str] = &["String", "Object", "System", "Integer", "Long", "Short", "Byte", "Character",
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
    /// 本类实例方法：名字 → 描述符
    instance_methods: HashMap<String, String>,
    /// 本类实例字段：名字 → 下标
    fields: HashMap<String, usize>,
    /// 字段下标 → 描述符
    field_descs: Vec<String>,
    /// 字段下标 → 声明类型（选 getfield/putfield 的宽度与 anewarray 的元素类要用）
    field_tys: Vec<Type>,
    /// 局部变量名 → 类型种类（0=int,1=long,2=ref）
    local_kind: HashMap<String, u8>,
    /// 局部变量名 → 声明类型（选数组元素宽度、anewarray/checkcast 的元素类要用）
    local_ty: HashMap<String, Type>,
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

/// println 的引用重载：声明类型确实是 String（或推不出来）才用 println(String)。
///
/// 声明成 `Object` 的局部变量，在有栈图的帧里验证器眼里就是 Object，
/// 这时发 `println(String)` 会直接 VerifyError —— 必须退回 `println(Object)`。
fn ref_println_desc(e: &Expr, ctx: &Ctx) -> &'static str {
    match expr_type(e, ctx) {
        Some(Type::Named { path, .. }) => {
            let joined = path.join("/");
            if joined == "String" || joined == "java/lang/String" {
                "(Ljava/lang/String;)V"
            } else {
                "(Ljava/lang/Object;)V"
            }
        }
        // 推不出来（方法调用的返回值等）就按老行为当 String
        _ => "(Ljava/lang/String;)V",
    }
}

/// 这个表达式是不是一次返回 void 的方法调用（语句位置上不能弹返回值）
fn is_void_call(e: &Expr, ctx: &Ctx) -> bool {
    let Expr::Call { recv, name, .. } = e else { return false };
    let desc = if recv.is_some() {
        ctx.instance_methods.get(name.as_str())
    } else {
        ctx.static_methods.get(name.as_str())
    };
    matches!(desc, Some(d) if d.ends_with(")V"))
}

fn compile_expr(e: &Expr, em: &mut Emit, ctx: &Ctx, cp: &mut CpBuilder) -> Result<(), CompileError> {
    match e {
        Expr::IntLit(s) => {
            if s.ends_with('L') || s.ends_with('l') {
                let v: i64 = s[..s.len() - 1].parse().map_err(|_| CompileError { msg: format!("long 字面量 `{s}` 解析失败") })?;
                if v == 0 || v == 1 {
                    em.op(0x09 + v as u8); // lconst_0/1
                } else {
                    let idx = cp.long(v);
                    em.op_u2(0x14, idx); // ldc2_w
                }
                em.push(2);
                return Ok(());
            }
            let v: i32 = s.parse().map_err(|_| CompileError { msg: format!("整数字面量 `{s}` 解析失败") })?;
            if (-32768..=32767).contains(&v) {
                em.const_int(v);
            } else {
                // 超出 sipush 范围，走常量池 ldc_w
                let idx = cp.int(v);
                em.op_u2(0x13, idx);
                em.push(1);
            }
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
            em.pop(1);
            em.push(if kind_wide(kind_of(&ctx.field_tys[idx])) { 2 } else { 1 });
            Ok(())
        }
        Expr::ArrayNew { elem, dims } => {
            if dims.len() != 1 {
                return err("只支持一维数组");
            }
            compile_expr(&dims[0], em, ctx, cp)?;
            // newarray 的操作数是元素类型码，引用元素走 anewarray（操作数是元素类）
            let atype = match elem {
                Type::Prim("int") => Some(10u8),    // T_INT
                Type::Prim("long") => Some(11),     // T_LONG
                Type::Prim("double") => Some(7),    // T_DOUBLE
                _ => None,
            };
            match atype {
                Some(a) => {
                    em.op(0xBC); // newarray
                    em.bytes.push(a);
                }
                None => {
                    if kind_of(elem) != K_REF {
                        return err(format!("不支持的数组元素类型: {elem:?}"));
                    }
                    let cls = ver_type(cp, elem);
                    em.op_u2(0xBD, cls); // anewarray
                }
            }
            em.pop(1);
            em.push(1);
            Ok(())
        }
        Expr::ArrayAccess { array, index } => {
            let k = elem_kind(array, ctx);
            compile_expr(array, em, ctx, cp)?;
            compile_expr(index, em, ctx, cp)?;
            em.op(match k {
                K_LONG => 0x2F,   // laload
                K_DOUBLE => 0x31, // daload
                K_REF => 0x32,    // aaload
                _ => 0x2E,        // iaload
            });
            em.pop(2);
            em.push(if kind_wide(k) { 2 } else { 1 });
            Ok(())
        }
        Expr::New { ty, args } => {
            let owner = match ty {
                Type::Named { path, .. } => path.join("/"),
                _ => return err("只支持创建对象"),
            };
            let arg_desc: String = args
                .iter()
                .map(|a| match expr_kind(a, ctx) {
                    K_LONG => "J".to_string(),
                    K_DOUBLE => "D".to_string(),
                    K_REF => expr_type(a, ctx)
                        .map(|t| descriptor(&t))
                        .unwrap_or_else(|| "Ljava/lang/String;".to_string()),
                    _ => "I".to_string(),
                })
                .collect();
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
        Expr::Cast { ty, expr } => {
            let from = expr_kind(expr, ctx);
            let to = kind_of(ty);
            compile_expr(expr, em, ctx, cp)?;
            if to == K_REF {
                // 引用转换不是数值转换：要发 checkcast（失败抛 ClassCastException）
                if from != K_REF {
                    return err("只能把引用值转成引用类型");
                }
                let cls = ver_type(cp, ty);
                em.op_u2(0xC0, cls); // checkcast
            } else {
                coerce(from, to, em)?;
            }
            Ok(())
        }
        Expr::InstanceOf { expr, ty, binding } => {
            if binding.is_some() {
                return err("暂不支持 instanceof 模式绑定（if (o instanceof T t)）");
            }
            compile_expr(expr, em, ctx, cp)?;
            let cls = ver_type(cp, ty);
            em.op_u2(0xC1, cls); // instanceof
            em.pop(1);
            em.push(1); // 压 0/1
            Ok(())
        }
        Expr::Null => {
            em.op(0x01); // aconst_null
            em.push(1);
            Ok(())
        }
        Expr::BoolLit(b) => {
            em.const_int(if *b { 1 } else { 0 });
            Ok(())
        }
        Expr::FloatLit(s) => {
            let raw = s.trim_end_matches(['d', 'D', 'f', 'F']);
            let v: f64 = raw
                .parse()
                .map_err(|_| CompileError { msg: format!("浮点字面量 `{s}` 解析失败") })?;
            if v == 0.0 && !v.is_sign_negative() {
                em.op(0x0E); // dconst_0
            } else if v == 1.0 {
                em.op(0x0F); // dconst_1
            } else {
                let idx = cp.double(v);
                em.op_u2(0x14, idx); // ldc2_w
            }
            em.push(2);
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
                let kind = *ctx.local_kind.get(name).unwrap_or(&K_INT);
                em.load_slot(slot, kind)?;
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
                        // 按实参类型选重载：int → (I)V，long → (J)V，
                        // double → (D)V，引用 → (Ljava/lang/String;)V
                        let (desc, width) = match expr_kind(&args[0], ctx) {
                            K_LONG => ("(J)V", 2u16),
                            K_DOUBLE => ("(D)V", 2),
                            K_REF => (ref_println_desc(&args[0], ctx), 1),
                            K_BOOL => ("(Z)V", 1),
                            _ => ("(I)V", 1),
                        };
                        let println = cp.method_ref("java/io/PrintStream", "println", desc);
                        em.op_u2(0xB6, println);
                        em.pop(1 + width); // 接收者 + 实参占的槽数
                        return Ok(());
                    }
                }
            }
            err(format!("不支持的 println 形态: {name}"))
        }
        // 调用对象方法 a.get()
        Expr::Call { recv: Some(r), name, args, .. } => {
            let desc = ctx
                .instance_methods
                .get(name.as_str())
                .ok_or_else(|| CompileError { msg: format!("未找到实例方法 `{name}`") })?
                .clone();
            compile_expr(r, em, ctx, cp)?;
            for a in args {
                compile_expr(a, em, ctx, cp)?;
            }
            let owner = ctx.class_name.clone();
            let idx = cp.method_ref(&owner, name, &desc);
            em.op_u2(0xB6, idx); // invokevirtual
            let n = args.len() as u16 + 1;
            em.pop(n);
            // 返回值占的槽数：long/double 占 2 槽
            if !desc.ends_with(")V") {
                em.push(if kind_wide(kind_from_ret(&desc)) { 2 } else { 1 });
            }
            Ok(())
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
            if !desc.ends_with(")V") {
                // 返回值占的槽数：long/double 占 2 槽
                em.push(if kind_wide(kind_from_ret(&desc)) { 2 } else { 1 });
            }
            Ok(())
        }
        Expr::Binary { op, lhs, rhs } => {
            let kl = expr_kind(lhs, ctx);
            let kr = expr_kind(rhs, ctx);
            // 比较作为**表达式**（`boolean f = a < b;`）：产出 0/1
            if let Some((jump_false, _)) = compile_cmp_operands(e, em, ctx, cp)? {
                em.bool_from_branch(jump_false)?;
                return Ok(());
            }
            // 移位：右操作数恒为 int，左操作数决定是 int 还是 long 版本
            if matches!(op, BinOp::Shl | BinOp::Shr | BinOp::Ushr) {
                let k = if kl == K_LONG { K_LONG } else { K_INT };
                compile_expr(lhs, em, ctx, cp)?;
                coerce(kl, k, em)?;
                compile_expr(rhs, em, ctx, cp)?;
                coerce(kr, K_INT, em)?;
                let code = match (op, k) {
                    (BinOp::Shl, K_LONG) => 0x79u8, // lshl
                    (BinOp::Shr, K_LONG) => 0x7B,   // lshr
                    (BinOp::Ushr, K_LONG) => 0x7D,  // lushr
                    (BinOp::Shl, _) => 0x78,        // ishl
                    (BinOp::Shr, _) => 0x7A,        // ishr
                    _ => 0x7C,                      // iushr
                };
                em.op(code);
                em.pop(if kind_wide(k) { 2 } else { 1 });
                return Ok(());
            }
            let k = promote(kl, kr);
            compile_expr(lhs, em, ctx, cp)?;
            coerce(kl, k, em)?;
            compile_expr(rhs, em, ctx, cp)?;
            coerce(kr, k, em)?;
            let code = match (op, k) {
                (BinOp::Add, K_LONG) => 0x61u8,
                (BinOp::Sub, K_LONG) => 0x65,
                (BinOp::Mul, K_LONG) => 0x69,
                (BinOp::Div, K_LONG) => 0x6D,
                (BinOp::Rem, K_LONG) => 0x71,
                (BinOp::Add, K_DOUBLE) => 0x63,
                (BinOp::Sub, K_DOUBLE) => 0x67,
                (BinOp::Mul, K_DOUBLE) => 0x6B,
                (BinOp::Div, K_DOUBLE) => 0x6F,
                (BinOp::Rem, K_DOUBLE) => 0x73,
                (BinOp::Add, _) => 0x60u8,
                (BinOp::Sub, _) => 0x64,
                (BinOp::Mul, _) => 0x68,
                (BinOp::Div, _) => 0x6C,
                (BinOp::Rem, _) => 0x70,
                (BinOp::BitAnd, K_LONG) => 0x7F, // land
                (BinOp::BitOr, K_LONG) => 0x81,  // lor
                (BinOp::BitXor, K_LONG) => 0x83, // lxor
                (BinOp::BitAnd, _) => 0x7E,      // iand
                (BinOp::BitOr, _) => 0x80,       // ior
                (BinOp::BitXor, _) => 0x82,      // ixor
                (other, _) => return err(format!("暂不支持的二元运算: {other:?}")),
            };
            em.op(code);
            em.pop(if kind_wide(k) { 2 } else { 1 });
            Ok(())
        }
        Expr::Unary { op: UnOp::Neg, expr } => {
            let k = expr_kind(expr, ctx);
            compile_expr(expr, em, ctx, cp)?;
            match k {
                K_LONG => em.op(0x75),   // lneg
                K_DOUBLE => em.op(0x77), // dneg
                _ => em.op(0x74),        // ineg
            }
            Ok(())
        }
        // ~x：int 走 iconst_m1 + ixor，long 走 ldc2_w -1 + lxor（JVM 没有按位取反指令）
        Expr::Unary { op: UnOp::BitNot, expr } => {
            let k = expr_kind(expr, ctx);
            compile_expr(expr, em, ctx, cp)?;
            if k == K_LONG {
                let idx = cp.long(-1);
                em.op_u2(0x14, idx);
                em.push(2);
                em.op(0x83); // lxor
                em.pop(3);
            } else {
                em.op(0x02); // iconst_m1
                em.push(1);
                em.op(0x82); // ixor
                em.pop(2);
            }
            Ok(())
        }
        // +x：无操作
        Expr::Unary { op: UnOp::Pos, expr } => compile_expr(expr, em, ctx, cp),
        other => err(format!("暂不支持的表达式: {other:?}")),
    }
}

/// 编译一个比较表达式（`a < b`、`o == null`、`p != null`），
/// 返回 `(条件为假时跳的指令, 条件为真时跳的指令)`；操作数已被消费。
///
/// 不是比较表达式时返回 `None`（调用方按「求出 0/1 再 ifeq」兜底）。
fn compile_cmp_operands(
    e: &Expr,
    em: &mut Emit,
    ctx: &Ctx,
    cp: &mut CpBuilder,
) -> Result<Option<(u8, u8)>, CompileError> {
    let Expr::Binary { op, lhs, rhs } = e else { return Ok(None) };
    if !matches!(
        op,
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
    ) {
        return Ok(None);
    }
    let kl = expr_kind(lhs, ctx);
    let kr = expr_kind(rhs, ctx);
    // 和 null 比较：ifnull / ifnonnull
    let l_null = matches!(**lhs, Expr::Null);
    let r_null = matches!(**rhs, Expr::Null);
    if l_null || r_null {
        if !(matches!(op, BinOp::Eq | BinOp::Ne)) {
            return err("null 只能做 == / != 比较");
        }
        let other = if l_null { rhs } else { lhs };
        compile_expr(other, em, ctx, cp)?;
        em.pop(1);
        return Ok(Some(if matches!(op, BinOp::Eq) {
            (0xC7, 0xC6) // 假 = 非空(ifnonnull)；真 = 空(ifnull)
        } else {
            (0xC6, 0xC7)
        }));
    }
    // 引用比较：if_acmpeq / if_acmpne（只有 == / != 有意义）
    if kl == K_REF || kr == K_REF {
        if !(matches!(op, BinOp::Eq | BinOp::Ne)) {
            return err("引用类型只能比较 == / !=");
        }
        compile_expr(lhs, em, ctx, cp)?;
        compile_expr(rhs, em, ctx, cp)?;
        em.pop(2);
        return Ok(Some(if matches!(op, BinOp::Eq) {
            (0xA6, 0xA5) // 假 = if_acmpne；真 = if_acmpeq
        } else {
            (0xA5, 0xA6)
        }));
    }
    let k = promote(kl, kr);
    compile_expr(lhs, em, ctx, cp)?;
    coerce(kl, k, em)?;
    compile_expr(rhs, em, ctx, cp)?;
    coerce(kr, k, em)?;
    // long/double 要先 lcmp / dcmpl(dcmpg) 压成 int，再用单值比较
    let widen = match k {
        K_LONG => Some(0x94u8), // lcmp
        // NaN 语义：< 和 <= 用 dcmpg，其余用 dcmpl
        K_DOUBLE => Some(if matches!(op, BinOp::Lt | BinOp::Le) { 0x98 } else { 0x97 }),
        _ => None,
    };
    if let Some(c) = widen {
        em.op(c);
        em.pop(3); // 两个宽值 4 槽 → 1 槽 int
        let ops = match op {
            BinOp::Lt => (0x9C, 0x9B), // ifge / iflt
            BinOp::Ge => (0x9B, 0x9C), // iflt / ifge
            BinOp::Gt => (0x9E, 0x9D), // ifle / ifgt
            BinOp::Le => (0x9D, 0x9E), // ifgt / ifle
            BinOp::Eq => (0x9A, 0x99), // ifne / ifeq
            _ => (0x99, 0x9A),         // ifeq / ifne
        };
        em.pop(1);
        return Ok(Some(ops));
    }
    let ops = match op {
        BinOp::Lt => (0xA2, 0xA1), // if_icmpge / if_icmplt
        BinOp::Ge => (0xA1, 0xA2), // if_icmplt / if_icmpge
        BinOp::Gt => (0xA4, 0xA3), // if_icmple / if_icmpgt
        BinOp::Le => (0xA3, 0xA4), // if_icmpgt / if_icmple
        BinOp::Eq => (0xA0, 0x9F), // if_icmpne / if_icmpeq
        _ => (0x9F, 0xA0),         // if_icmpeq / if_icmpne
    };
    em.pop(2);
    Ok(Some(ops))
}

/// 把 `a < b` 这类条件编成「条件为假时跳到 label」。
fn compile_cond_false(
    e: &Expr,
    em: &mut Emit,
    ctx: &Ctx,
    cp: &mut CpBuilder,
    label: usize,
) -> Result<(), CompileError> {
    if let Some((jump_false, _)) = compile_cmp_operands(e, em, ctx, cp)? {
        em.branch(jump_false, label);
        return Ok(());
    }
    // 兜底：任何产出 0/1 的表达式（instanceof、boolean 局部变量、boolean 方法返回值）
    // 都编成「求值 → ifeq 跳到 label」。跳转点上操作数栈是空的。
    let kind = expr_kind(e, ctx);
    if kind_wide(kind) {
        return err("条件表达式不能是 long/double");
    }
    compile_expr(e, em, ctx, cp)?;
    em.pop(1);
    em.branch(0x99, label); // ifeq
    Ok(())
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
            let kind = expr_kind(e, ctx);
            compile_expr(e, em, ctx, cp)?;
            match kind {
                K_LONG => {
                    em.op(0xAD); // lreturn
                    em.pop(2);
                }
                K_DOUBLE => {
                    em.op(0xAF); // dreturn
                    em.pop(2);
                }
                K_REF => {
                    em.op(0xB0); // areturn
                    em.pop(1);
                }
                _ => {
                    em.op(0xAC); // ireturn
                    em.pop(1);
                }
            }
            Ok(true)
        }
        Stmt::Expr(e) if is_println_call(e) => {
            compile_expr(e, em, ctx, cp)?;
            Ok(false)
        }
        Stmt::Expr(e) if !matches!(e, Expr::Assign { .. }) => {
            let kind = expr_kind(e, ctx);
            compile_expr(e, em, ctx, cp)?;
            if is_void_call(e, ctx) {
                return Ok(false); // void 方法调用没有返回值可弹
            }
            if kind_wide(kind) {
                em.op(0x58); // pop2
                em.pop(2);
            } else {
                em.op(0x57); // pop
                em.pop(1);
            }
            Ok(false)
        }
        Stmt::LocalVar { ty, name, init } => {
            let slot = ctx.next_slot;
            if slot >= SCRATCH_SLOT {
                return err(format!(
                    "局部变量太多：槽位已用到 {slot}，最高可用 {}（{} 号槽留给比较结果）",
                    SCRATCH_SLOT - 1,
                    SCRATCH_SLOT
                ));
            }
            let kind = kind_of(ty);
            let wide_local = kind_wide(kind);
            ctx.next_slot += if wide_local { 2 } else { 1 };
            if em.max_locals < ctx.next_slot as u16 {
                em.max_locals = ctx.next_slot as u16;
            }
            ctx.locals.insert(name.clone(), slot);
            ctx.local_kind.insert(name.clone(), kind);
            ctx.local_ty.insert(name.clone(), ty.clone());
            if let Some(e) = init {
                compile_expr(e, em, ctx, cp)?;
            }
            // 槽的验证类型必须在**初始化表达式编译完之后**才登记：
            // 初始化式里如果带跳转（如 `boolean f = x == null;`），那时候这个槽还没赋值，
            // 帧里抢先写 ITEM_Integer 会被 HotSpot 判成 VerifyError（top 不可赋给 integer）。
            let vt = ver_type(&mut *cp, ty);
            em.set_local_type(slot as usize, vt);
            if wide_local {
                // 第 2 槽没有自己的类型，写「空」占位
                em.set_local_type(slot as usize + 1, u16::MAX);
            }
            if let Some(e) = init {
                let _ = e;
                em.store_slot(slot, kind)?;
            }
            Ok(false)
        }
        Stmt::Expr(Expr::Assign { op: None, lhs, rhs }) if matches!(&**lhs, Expr::ArrayAccess { .. }) => {
            let (arr, idx) = match &**lhs {
                Expr::ArrayAccess { array, index } => (array.clone(), index.clone()),
                _ => unreachable!(),
            };
            let k = elem_kind(&arr, ctx);
            compile_expr(&arr, em, ctx, cp)?;
            compile_expr(&idx, em, ctx, cp)?;
            compile_expr(rhs, em, ctx, cp)?;
            em.op(match k {
                K_LONG => 0x50,   // lastore
                K_DOUBLE => 0x52, // dastore
                K_REF => 0x53,    // aastore
                _ => 0x4F,        // iastore
            });
            em.pop(if kind_wide(k) { 4 } else { 3 });
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
            em.pop(if kind_wide(kind_of(&ctx.field_tys[idx])) { 3 } else { 2 });
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
            let kind = *ctx.local_kind.get(&name).unwrap_or(&K_INT);
            compile_expr(rhs, em, ctx, cp)?;
            em.store_slot(slot, kind)?;
            Ok(false)
        }
        Stmt::Switch { selector, cases } => {
            // 用一个临时槽保存选择子
            let sel_slot = ctx.next_slot;
            ctx.next_slot += 1;
            if em.max_locals < ctx.next_slot as u16 {
                em.max_locals = ctx.next_slot as u16;
            }
            compile_expr(selector, em, ctx, cp)?;
            em.istore(sel_slot)?;
            em.set_local_type(sel_slot as usize, 0); // int

            // 为每个 case 建标签；线性比较链 + goto
            let mut body_labels = Vec::new();
            for _ in cases {
                body_labels.push(em.new_label());
            }
            let end_label = em.new_label();
            // default 体在哪里？没有 default 就跳到结尾
            let default_target = cases
                .iter()
                .position(|c| c.labels.iter().any(|l| l.is_none()))
                .map(|i| body_labels[i])
                .unwrap_or(end_label);

            for (i, c) in cases.iter().enumerate() {
                for label in &c.labels {
                    match label {
                        Some(v) => {
                            em.iload(sel_slot)?;
                            compile_expr(v, em, ctx, cp)?;
                            let next = em.new_label();
                            // 不相等就继续比下一个
                            em.branch(0xa0, next); // if_icmpne
                            em.pop(2);
                            em.goto(body_labels[i]);
                            em.bind(next);
                        }
                        None => {
                            // default 不参与比较链：链条走完自然落到它
                        }
                    }
                }
            }
            em.goto(default_target);

            // 依次发射各 case 体（自然贯穿）
            for (i, c) in cases.iter().enumerate() {
                em.bind(body_labels[i]);
                for s in &c.body {
                    compile_stmt(s, em, ctx, cp)?;
                }
            }
            if default_target == end_label {
                em.bind(end_label);
            }
            // 视为已终结：避免在末尾再补一条不可达的默认 return（HotSpot 要求死代码也有栈图帧）
            Ok(true)
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

    // 实例方法：名字 → 描述符
    let mut inst_methods: HashMap<String, String> = HashMap::new();
    for m in &t.members {
        if let Member::Method(md) = m {
            if !md.modifiers.iter().any(|x| x == "static") && !(md.ret.is_none() && md.name == t.name) {
                inst_methods.insert(md.name.clone(), method_descriptor(&md.params, &md.ret));
            }
        }
    }

    // 实例字段：名字 → 下标，以及每个下标的描述符
    let mut field_index: HashMap<String, usize> = HashMap::new();
    let mut field_desc_list: Vec<String> = Vec::new();
    let mut field_ty_list: Vec<Type> = Vec::new();
    for m in &t.members {
        if let Member::Field(f) = m {
            if f.modifiers.iter().any(|x| x == "static") {
                continue;
            }
            field_index.insert(f.name.clone(), field_desc_list.len());
            field_desc_list.push(descriptor(&f.ty));
            field_ty_list.push(f.ty.clone());
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

        let param_slots: u16 = md.params.iter().map(|p| if matches!(p.ty, Type::Prim("long") | Type::Prim("double")) { 2 } else { 1 }).sum();
        let mut em = Emit::new(if is_static { param_slots } else { param_slots + 1 });
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
            instance_methods: inst_methods.clone(),
            fields: field_index.clone(),
            field_descs: field_desc_list.clone(),
            field_tys: field_ty_list.clone(),
            local_kind: HashMap::new(),
            local_ty: HashMap::new(),
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
            let wide = matches!(p.ty, Type::Prim("long") | Type::Prim("double"));
            if wide {
                em.set_local_type(slot as usize + 1, u16::MAX);
            }
            ctx.locals.insert(p.name.clone(), slot);
            ctx.local_kind.insert(p.name.clone(), kind_of(&p.ty));
            ctx.local_ty.insert(p.name.clone(), p.ty.clone());
            ctx.next_slot += if wide { 2 } else { 1 };
            slot += if wide { 2 } else { 1 };
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
        Type::Prim("long") => 0xFFFE, // 栈图里要写 ITEM_Long
        Type::Prim("double") => 0xFFFD, // 栈图里要写 ITEM_Double
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

/// 把栈顶值从 `from` 种类转换成 `to` 种类（只做数值拓宽/收窄）
fn coerce(from: u8, to: u8, em: &mut Emit) -> Result<(), CompileError> {
    if from == to {
        return Ok(());
    }
    // boolean 与 int 在字节码层面完全相同，不需要任何转换指令
    if (from == K_BOOL && to == K_INT) || (from == K_INT && to == K_BOOL) {
        return Ok(());
    }
    match (from, to) {
        (K_INT, K_LONG) => {
            em.op(0x85); // i2l
            em.push(1);
        }
        (K_INT, K_DOUBLE) => {
            em.op(0x87); // i2d
            em.push(1);
        }
        (K_LONG, K_DOUBLE) => {
            em.op(0x8A); // l2d
            em.push(0);
        }
        (K_LONG, K_INT) => {
            em.op(0x88); // l2i
            em.pop(1);
        }
        (K_DOUBLE, K_INT) => {
            em.op(0x8E); // d2i
            em.pop(1);
        }
        (K_DOUBLE, K_LONG) => {
            em.op(0x8F); // d2l
            em.push(0);
        }
        _ => return err(format!("暂不支持的类型转换（种类 {from} → {to}）")),
    }
    Ok(())
}

/// 表达式 → 声明类型。能静态推出来的才给（数组元素宽度、anewarray/checkcast 的元素类要用）。
///
/// 推不出来返回 `None`：调用方按「int」这一老行为兜底，不要在这里猜。
fn expr_type(e: &Expr, ctx: &Ctx) -> Option<Type> {
    match e {
        Expr::Ident(n) => ctx.local_ty.get(n).cloned(),
        Expr::Field(_, n) => ctx.fields.get(n).and_then(|i| ctx.field_tys.get(*i)).cloned(),
        Expr::Cast { ty, .. } => Some(ty.clone()),
        Expr::New { ty, .. } => Some(ty.clone()),
        Expr::ArrayNew { elem, .. } => Some(Type::Array(Box::new(elem.clone()))),
        Expr::ArrayAccess { array, .. } => match expr_type(array, ctx)? {
            Type::Array(inner) => Some(*inner),
            _ => None,
        },
        _ => None,
    }
}

/// 数组表达式的元素种类（决定 iaload/laload/daload/aaload）
fn elem_kind(array: &Expr, ctx: &Ctx) -> u8 {
    match expr_type(array, ctx) {
        Some(Type::Array(inner)) => kind_of(&inner),
        _ => K_INT,
    }
}

/// 表达式 → 类型种类（只用于选字节码，不做严格类型检查）
fn expr_kind(e: &Expr, ctx: &Ctx) -> u8 {
    match e {
        Expr::IntLit(s) => {
            if s.ends_with('L') || s.ends_with('l') { K_LONG } else { K_INT }
        }
        Expr::FloatLit(_) => K_DOUBLE,
        Expr::Cast { ty, .. } => kind_of(ty),
        Expr::Ident(n) => ctx.local_kind.get(n).copied().unwrap_or(K_INT),
        Expr::Binary { op, lhs, rhs } => match op {
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Eq | BinOp::Ne | BinOp::And
            | BinOp::Or => K_BOOL,
            _ => promote(expr_kind(lhs, ctx), expr_kind(rhs, ctx)),
        },
        Expr::Unary { expr, .. } => expr_kind(expr, ctx),
        Expr::ArrayAccess { array, .. } => elem_kind(array, ctx),
        Expr::Call { recv, name, .. } => {
            let desc = if recv.is_some() {
                ctx.instance_methods.get(name.as_str())
            } else {
                ctx.static_methods.get(name.as_str())
            };
            desc.map(|d| kind_from_ret(d)).unwrap_or(K_INT)
        }
        Expr::StrLit(_) | Expr::This | Expr::New { .. } | Expr::ArrayNew { .. } | Expr::Null => K_REF,
        Expr::InstanceOf { .. } | Expr::BoolLit(_) => K_BOOL,
        Expr::Field(_, name) => ctx
            .fields
            .get(name)
            .and_then(|i| ctx.field_descs.get(*i))
            .map(|d| kind_from_desc(d))
            .unwrap_or(K_INT),
        _ => K_INT,
    }
}

/// 粗略判断表达式是不是 long 类型（用于选择 ladd/lsub/... 与 lreturn）
fn expr_is_long(e: &Expr, ctx: &Ctx) -> bool {
    expr_kind(e, ctx) == K_LONG
}
