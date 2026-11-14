//! `rustjava` 的最小解释器：不依赖任何 JVM，直接执行 class 文件。
//!
//! 覆盖范围（够跑我们自己编译器产出的东西）：
//! int/long/double 的常量、局部变量与算术，lcmp/dcmpl/dcmpg，数值转换，
//! 比较与跳转、goto、pop/pop2、对象与 int 数组、
//! `System.out.println(int|long|double|String)`、invokestatic、各 xreturn。

use crate::java_double_to_string;
use crate::jit::param_slots;
use crate::{parse, ClassFile, CpEntry};
use std::collections::HashMap;

#[derive(Debug)]
pub struct VmError {
    pub msg: String,
}

fn err<T>(msg: impl Into<String>) -> Result<T, VmError> {
    Err(VmError { msg: msg.into() })
}

/// 解释器里的一个值。int/long/double 共用一种表示，用完再按字节码的语义取用。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Slot {
    I(i32),
    L(i64),
    F(f64),
}

impl Slot {
    fn is_wide(self) -> bool {
        matches!(self, Slot::L(_) | Slot::F(_))
    }
    fn as_i32(self) -> i32 {
        match self {
            Slot::I(v) => v,
            Slot::L(v) => v as i32,
            Slot::F(v) => v as i32,
        }
    }
    fn as_i64(self) -> i64 {
        match self {
            Slot::I(v) => v as i64,
            Slot::L(v) => v,
            Slot::F(v) => v as i64,
        }
    }
    fn as_f64(self) -> f64 {
        match self {
            Slot::I(v) => v as f64,
            Slot::L(v) => v as f64,
            Slot::F(v) => v,
        }
    }
}

/// 执行结果
pub struct ExecResult {
    pub return_value: i32,
    pub output: Vec<String>,
}

struct Frame {
    /// 局部变量表：按**JVM 槽**编号。long/double 占两槽，第 2 槽写 `I(0)` 当无效占位
    /// （与 max_locals、与 JIT 的 locals 布局保持一致）。
    locals: Vec<Slot>,
    /// 操作数栈：一个宽值只占**一个**条目（解释器按值而不是按槽建模）。
    stack: Vec<Slot>,
}

impl Frame {
    fn new(max_locals: usize) -> Self {
        Frame { locals: vec![Slot::I(0); max_locals.max(1)], stack: Vec::new() }
    }
    fn push(&mut self, v: Slot) {
        self.stack.push(v);
    }
    fn pop(&mut self) -> Result<Slot, VmError> {
        self.stack.pop().ok_or_else(|| VmError { msg: "操作数栈下溢".into() })
    }
    fn pop_i(&mut self) -> Result<i32, VmError> {
        Ok(self.pop()?.as_i32())
    }
    fn load(&self, slot: usize) -> Slot {
        *self.locals.get(slot).unwrap_or(&Slot::I(0))
    }
    fn store(&mut self, slot: usize, v: Slot) {
        if slot + 1 >= self.locals.len() {
            self.locals.resize(slot + 2, Slot::I(0));
        }
        self.locals[slot] = v;
        if v.is_wide() {
            self.locals[slot + 1] = Slot::I(0); // 宽值的第 2 槽无效
        }
    }
}

struct MethodRefInfo {
    class: String,
    name: String,
    desc: String,
}

/// 字符串句柄的基址：`handle >= STR_BASE` 就是字符串堆里的下标。
/// 对象句柄是正数（1 起）、数组句柄是负数、0 是 null，四者不重叠。
const STR_BASE: i32 = 1 << 24;

/// 一个引用句柄的运行时类型
enum RefKind {
    Null,
    Str,
    Obj(String),
    Arr(String),
}

pub struct Vm<'a> {
    class: &'a ClassFile,
    methods: HashMap<(String, String), &'a crate::Member>,
    output: Vec<String>,
    depth: usize,
    /// 对象堆：每个对象是一组字段槽（Slot 自带宽度，long/double 不会被截断）；
    /// 引用 = 下标 + 1（0 表示 null）
    objects: Vec<Vec<Slot>>,
    /// 对象堆的运行时类型名（instanceof / checkcast 用）
    object_class: Vec<String>,
    /// 实例字段名 → 下标
    field_index: HashMap<String, usize>,
    /// 本类实例字段个数
    field_count: usize,
    /// 数组堆：每个数组是一组元素槽；句柄 = -(下标 + 1)
    arrays: Vec<Vec<Slot>>,
    /// 数组堆的类描述符，如 `[I` / `[Ljava/lang/String;`
    array_class: Vec<String>,
    /// 字符串常量堆；句柄 = STR_BASE + 下标
    strings: Vec<String>,
    /// 常量池索引 → 字符串句柄（同一常量必须拿到同一句柄，否则引用相等会错）
    string_handles: HashMap<u16, i32>,
}

impl<'a> Vm<'a> {
    pub fn new(class: &'a ClassFile) -> Self {
        let mut methods = HashMap::new();
        for m in &class.methods {
            let name = String::from_utf8_lossy(class.utf8(m.name_index).unwrap_or_default()).into_owned();
            let desc =
                String::from_utf8_lossy(class.utf8(m.descriptor_index).unwrap_or_default()).into_owned();
            methods.insert((name, desc), m);
        }
        let mut field_index = HashMap::new();
        let mut idx = 0usize;
        for f in &class.fields {
            let name = String::from_utf8_lossy(class.utf8(f.name_index).unwrap_or_default()).into_owned();
            let is_static = f.access_flags & 0x0008 != 0;
            if !is_static {
                field_index.insert(name, idx);
                idx += 1;
            }
        }
        let n_fields = idx;
        Vm {
            class,
            methods,
            output: Vec::new(),
            depth: 0,
            objects: Vec::new(),
            object_class: Vec::new(),
            field_index,
            field_count: n_fields,
            arrays: Vec::new(),
            array_class: Vec::new(),
            strings: Vec::new(),
            string_handles: HashMap::new(),
        }
    }

    fn method_ref(&self, index: u16) -> Result<MethodRefInfo, VmError> {
        let e = self
            .class
            .constant_pool
            .get(index as usize)
            .and_then(|e| e.as_ref())
            .ok_or_else(|| VmError { msg: format!("常量池索引 {index} 为空") })?;
        let (class_idx, nat_idx) = match e {
            CpEntry::MethodRef(c, n) | CpEntry::InterfaceMethodRef(c, n) => (*c, *n),
            CpEntry::FieldRef(c, n) => (*c, *n),
            other => return err(format!("索引 {index} 不是方法/字段引用: {other:?}")),
        };
        let class_name = match self.class.constant_pool.get(class_idx as usize).and_then(|e| e.as_ref()) {
            Some(CpEntry::Class(n)) => {
                String::from_utf8_lossy(self.class.utf8(*n).unwrap_or_default()).into_owned()
            }
            _ => return err("方法引用的类名解析失败"),
        };
        let (name, desc) = match self.class.constant_pool.get(nat_idx as usize).and_then(|e| e.as_ref()) {
            Some(CpEntry::NameAndType(n, d)) => (
                String::from_utf8_lossy(self.class.utf8(*n).unwrap_or_default()).into_owned(),
                String::from_utf8_lossy(self.class.utf8(*d).unwrap_or_default()).into_owned(),
            ),
            _ => return err("NameAndType 解析失败"),
        };
        Ok(MethodRefInfo { class: class_name, name, desc })
    }

    fn ldc_int(&mut self, index: u16) -> Result<i32, VmError> {
        match self.class.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
            Some(CpEntry::Integer(v)) => Ok(*v as i32),
            Some(CpEntry::String(_)) => Ok(self.string_handle(index)), // 句柄进字符串堆
            other => err(format!("ldc 不支持的类型: {other:?}")),
        }
    }

    /// `ldc2_w`：long / double 常量
    fn ldc2_w(&self, index: u16) -> Result<Slot, VmError> {
        match self.class.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
            Some(CpEntry::Long(v)) => Ok(Slot::L(*v as i64)),
            Some(CpEntry::Double(bits)) => Ok(Slot::F(f64::from_bits(*bits))),
            other => err(format!("ldc2_w 不支持的类型: {other:?}")),
        }
    }

    /// 本类常量池里某个 Class 项的类名（instanceof / checkcast / anewarray 用）
    fn class_name_of(&self, index: u16) -> Result<String, VmError> {
        match self.class.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
            Some(CpEntry::Class(n)) => Ok(String::from_utf8_lossy(self.class.utf8(*n).unwrap_or_default()).into_owned()),
            other => err(format!("索引 {index} 不是 Class 项: {other:?}")),
        }
    }

    /// 字符串常量 → 句柄（同一常量池项复用同一句柄）
    fn string_handle(&mut self, cp_index: u16) -> i32 {
        if let Some(h) = self.string_handles.get(&cp_index) {
            return *h;
        }
        let text = self.string_at(cp_index).unwrap_or_default();
        self.strings.push(text);
        let h = STR_BASE + self.strings.len() as i32 - 1;
        self.string_handles.insert(cp_index, h);
        h
    }

    fn string_text(&self, handle: i32) -> String {
        self.strings.get((handle - STR_BASE) as usize).cloned().unwrap_or_default()
    }

    fn ref_kind(&self, handle: i32) -> RefKind {
        if handle == 0 {
            RefKind::Null
        } else if handle >= STR_BASE {
            RefKind::Str
        } else if handle > 0 {
            RefKind::Obj(self.object_class.get(handle as usize - 1).cloned().unwrap_or_default())
        } else {
            RefKind::Arr(self.array_class.get((-handle - 1) as usize).cloned().unwrap_or_default())
        }
    }

    /// `instanceof` 的判定：null 对任何类型都是 false（含 Object）
    fn is_instance(&self, handle: i32, target: &str) -> bool {
        match self.ref_kind(handle) {
            RefKind::Null => false,
            _ if target == "java/lang/Object" => true,
            RefKind::Str => target == "java/lang/String",
            RefKind::Obj(c) => c == target,
            RefKind::Arr(d) => d == target,
        }
    }

    /// 出错信息里用的可读类型名
    fn type_name(&self, handle: i32) -> String {
        match self.ref_kind(handle) {
            RefKind::Null => "null".into(),
            RefKind::Str => "java.lang.String".into(),
            RefKind::Obj(c) => c.replace('/', "."),
            RefKind::Arr(d) => d.replace('/', "."),
        }
    }


    fn string_at(&self, index: u16) -> Option<String> {
        match self.class.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
            Some(CpEntry::String(u)) => {
                self.class.utf8(*u).map(|b| String::from_utf8_lossy(b).into_owned())
            }
            _ => None,
        }
    }

    /// `System.out.println(...)` 的文本：按重载（参数类型）决定怎么格式化
    fn println_text(&self, desc: &str, v: Slot) -> String {
        match desc {
            "(J)V" => v.as_i64().to_string(),
            "(D)V" | "(F)V" => java_double_to_string(v.as_f64()),
            "(Z)V" => (if v.as_i32() != 0 { "true" } else { "false" }).to_string(),
            "(Ljava/lang/String;)V" | "(Ljava/lang/Object;)V" => {
                let h = v.as_i32();
                if h == 0 {
                    "null".to_string() // Java 打 println((String) null) 也是 "null"
                } else if h >= STR_BASE {
                    self.string_text(h)
                } else {
                    self.type_name(h) // 对象/数组：给个类型名，别静默打成空
                }
            }
            _ => v.as_i32().to_string(),
        }
    }

    /// 执行一个方法：desc 形如 `(II)I`；`receiver` 表示 args[0] 是 this（实例方法）
    fn execute(
        &mut self,
        m: &'a crate::Member,
        desc: &str,
        args: &[Slot],
        receiver: bool,
    ) -> Result<Slot, VmError> {
        if self.depth > 64 {
            return err("调用深度超限（可能递归过深）");
        }
        let code = self.code_of(m)?;
        let (max_locals, body) = code;

        let mut frame = Frame::new(max_locals);
        // 参数按 JVM 槽位落位：this 在 0，long/double 占两槽，后面的参数要跳过一槽
        let arg_slots = param_slots(desc);
        let offset = usize::from(receiver);
        for (i, a) in args.iter().enumerate() {
            let slot = if i < offset {
                0
            } else {
                arg_slots.get(i - offset).copied().unwrap_or(i - offset) + offset
            };
            frame.store(slot, *a);
        }

        let trace = std::env::var_os("VMLESS_TRACE").is_some();
        let mut pc = 0usize;
        while pc < body.len() {
            let op = body[pc];
            if trace {
                eprintln!("  pc={pc} op={op:02x} stack={:?} locals={:?}", frame.stack, frame.locals);
            }
            pc += 1;
            match op {
                0x00 => {}
                0x01 => frame.push(Slot::I(0)),             // aconst_null
                0x02 => frame.push(Slot::I(-1)),
                0x03..=0x08 => frame.push(Slot::I(op as i32 - 0x03)),
                0x09 | 0x0a => frame.push(Slot::L(op as i64 - 0x09)), // lconst_0/1
                0x0e | 0x0f => frame.push(Slot::F(if op == 0x0e { 0.0 } else { 1.0 })),
                0x10 => {
                    let v = body[pc] as i8 as i32;
                    pc += 1;
                    frame.push(Slot::I(v));
                }
                0x11 => {
                    let v = i16::from_be_bytes([body[pc], body[pc + 1]]) as i32;
                    pc += 2;
                    frame.push(Slot::I(v));
                }
                0x12 => {
                    let idx = body[pc] as u16;
                    pc += 1;
                    let v = self.ldc_int(idx)?;
                    frame.push(Slot::I(v));
                }
                0x13 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let v = self.ldc_int(idx)?;
                    frame.push(Slot::I(v));
                }
                0x14 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let v = self.ldc2_w(idx)?;
                    frame.push(v);
                }
                // 局部变量读取（显式下标）
                0x15..=0x19 => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    frame.push(frame.load(slot));
                }
                // 局部变量读取（隐式下标 iload_0.., lload_0.., fload_0.., dload_0.., aload_0..）
                0x1a..=0x1d => {
                    let slot = (op - 0x1a) as usize;
                    frame.push(frame.load(slot));
                }
                0x1e..=0x21 => {
                    let slot = (op - 0x1e) as usize;
                    frame.push(frame.load(slot));
                }
                0x22..=0x25 => {
                    let slot = (op - 0x22) as usize;
                    frame.push(frame.load(slot));
                }
                0x26..=0x29 => {
                    let slot = (op - 0x26) as usize;
                    frame.push(frame.load(slot));
                }
                0x2a..=0x2d => {
                    let slot = (op - 0x2a) as usize;
                    frame.push(frame.load(slot));
                }
                // 局部变量存储（istore/lstore/fstore/dstore/astore + _n 形式）
                0x36..=0x3a => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = frame.pop()?;
                    frame.store(slot, v);
                }
                0x3b..=0x3e => {
                    let slot = (op - 0x3b) as usize;
                    let v = frame.pop()?;
                    frame.store(slot, v);
                }
                0x3f..=0x42 => {
                    let slot = (op - 0x3f) as usize;
                    let v = frame.pop()?;
                    frame.store(slot, v);
                }
                0x43..=0x46 => {
                    let slot = (op - 0x43) as usize;
                    let v = frame.pop()?;
                    frame.store(slot, v);
                }
                0x47..=0x4a => {
                    let slot = (op - 0x47) as usize;
                    let v = frame.pop()?;
                    frame.store(slot, v);
                }
                0x4b..=0x4e => {
                    let slot = (op - 0x4b) as usize;
                    let v = frame.pop()?;
                    frame.store(slot, v);
                }
                0x57 => {
                    frame.pop()?;
                }
                // pop2：弹掉一个宽值，或两个普通值
                0x58 => {
                    let v = frame.pop()?;
                    if !v.is_wide() {
                        frame.pop()?;
                    }
                }
                0x59 => {
                    let v = *frame.stack.last().unwrap_or(&Slot::I(0));
                    frame.push(v);
                }
                // dup2：复制一个宽值，或栈顶两个普通值
                0x5c => {
                    let n = frame.stack.len();
                    let top = *frame.stack.last().unwrap_or(&Slot::I(0));
                    if top.is_wide() {
                        frame.push(top);
                    } else if n >= 2 {
                        let a = frame.stack[n - 2];
                        frame.push(a);
                        frame.push(top);
                    }
                }
                // int 算术
                0x60 | 0x64 | 0x68 | 0x6c | 0x70 => {
                    let b = frame.pop_i()?;
                    let a = frame.pop_i()?;
                    let r = match op {
                        0x60 => a.wrapping_add(b),
                        0x64 => a.wrapping_sub(b),
                        0x68 => a.wrapping_mul(b),
                        0x6c => {
                            if b == 0 {
                                return err("整数除零");
                            }
                            a.wrapping_div(b)
                        }
                        _ => {
                            if b == 0 {
                                return err("整数取余除零");
                            }
                            a.wrapping_rem(b)
                        }
                    };
                    frame.push(Slot::I(r));
                }
                // long 算术
                0x61 | 0x65 | 0x69 | 0x6d | 0x71 => {
                    let b = frame.pop()?.as_i64();
                    let a = frame.pop()?.as_i64();
                    let r = match op {
                        0x61 => a.wrapping_add(b),
                        0x65 => a.wrapping_sub(b),
                        0x69 => a.wrapping_mul(b),
                        0x6d => {
                            if b == 0 {
                                return err("long 除零");
                            }
                            a.wrapping_div(b)
                        }
                        _ => {
                            if b == 0 {
                                return err("long 取余除零");
                            }
                            a.wrapping_rem(b)
                        }
                    };
                    frame.push(Slot::L(r));
                }
                // double 算术
                0x63 | 0x67 | 0x6b | 0x6f | 0x73 => {
                    let b = frame.pop()?.as_f64();
                    let a = frame.pop()?.as_f64();
                    let r = match op {
                        0x63 => a + b,
                        0x67 => a - b,
                        0x6b => a * b,
                        0x6f => a / b,
                        _ => a % b, // drem：与 fmod 一致（NaN 有 NaN 语义）
                    };
                    frame.push(Slot::F(r));
                }
                0x74 => {
                    let a = frame.pop_i()?;
                    frame.push(Slot::I(a.wrapping_neg()));
                }
                0x75 => {
                    let a = frame.pop()?.as_i64();
                    frame.push(Slot::L(a.wrapping_neg()));
                }
                0x77 => {
                    let a = frame.pop()?.as_f64();
                    frame.push(Slot::F(-a));
                }
                // 数值转换
                0x85 => {
                    let a = frame.pop_i()?;
                    frame.push(Slot::L(a as i64)); // i2l
                }
                0x86 => {
                    let a = frame.pop_i()?;
                    frame.push(Slot::F(a as f32 as f64)); // i2f
                }
                0x87 => {
                    let a = frame.pop_i()?;
                    frame.push(Slot::F(a as f64)); // i2d
                }
                0x88 => {
                    let a = frame.pop()?.as_i64();
                    frame.push(Slot::I(a as i32)); // l2i
                }
                0x89 => {
                    let a = frame.pop()?.as_i64();
                    frame.push(Slot::F(a as f32 as f64)); // l2f
                }
                0x8a => {
                    let a = frame.pop()?.as_i64();
                    frame.push(Slot::F(a as f64)); // l2d
                }
                0x8b => {
                    let a = frame.pop()?.as_f64();
                    frame.push(Slot::I(a as i32)); // f2i（Rust 的 as 与 Java 一样饱和）
                }
                0x8c => {
                    let a = frame.pop()?.as_f64();
                    frame.push(Slot::L(a as i64)); // f2l
                }
                0x8d => {
                    let a = frame.pop()?.as_f64();
                    frame.push(Slot::F(a)); // f2d
                }
                0x8e => {
                    let a = frame.pop()?.as_f64();
                    frame.push(Slot::I(a as i32)); // d2i
                }
                0x8f => {
                    let a = frame.pop()?.as_f64();
                    frame.push(Slot::L(a as i64)); // d2l
                }
                0x90 => {
                    let a = frame.pop()?.as_f64();
                    frame.push(Slot::F(a as f32 as f64)); // d2f
                }
                0x91 => {
                    let a = frame.pop_i()?;
                    frame.push(Slot::I(a as i8 as i32)); // i2b
                }
                0x92 => {
                    let a = frame.pop_i()?;
                    frame.push(Slot::I(a as u16 as i32)); // i2c
                }
                0x93 => {
                    let a = frame.pop_i()?;
                    frame.push(Slot::I(a as i16 as i32)); // i2s
                }
                // 比较：lcmp / fcmpl / fcmpg / dcmpl / dcmpg 都压成 int
                0x94 | 0x95 | 0x96 | 0x97 | 0x98 => {
                    let nan = if matches!(op, 0x95 | 0x97) { -1 } else { 1 };
                    let b = frame.pop()?.as_f64();
                    let a = frame.pop()?.as_f64();
                    let r = if a > b {
                        1
                    } else if a < b {
                        -1
                    } else if a == b {
                        0
                    } else {
                        nan // NaN
                    };
                    frame.push(Slot::I(r));
                }
                // 一元条件跳转（偏移相对操作码地址）
                0x99 | 0x9a | 0x9b | 0x9c | 0x9d | 0x9e => {
                    let op_pc = pc - 1;
                    let off = i16::from_be_bytes([body[pc], body[pc + 1]]) as isize;
                    pc += 2;
                    let v = frame.pop_i()?;
                    let take = match op {
                        0x99 => v == 0,
                        0x9a => v != 0,
                        0x9b => v < 0,
                        0x9c => v >= 0,
                        0x9d => v > 0,
                        _ => v <= 0,
                    };
                    if take {
                        pc = (op_pc as isize + off) as usize;
                    }
                }
                // 二元整数/引用比较跳转
                0x9f..=0xa4 | 0xa5 | 0xa6 => {
                    let op_pc = pc - 1;
                    let off = i16::from_be_bytes([body[pc], body[pc + 1]]) as isize;
                    pc += 2;
                    let b = frame.pop_i()?;
                    let a = frame.pop_i()?;
                    let take = match op {
                        0x9f | 0xa5 => a == b,
                        0xa0 | 0xa6 => a != b,
                        0xa1 => a < b,
                        0xa2 => a >= b,
                        0xa3 => a > b,
                        _ => a <= b,
                    };
                    if take {
                        pc = (op_pc as isize + off) as usize;
                    }
                }
                0xa7 => {
                    let op_pc = pc - 1;
                    let off = i16::from_be_bytes([body[pc], body[pc + 1]]) as isize;
                    pc = (op_pc as isize + off) as usize;
                }
                0xac => return Ok(Slot::I(frame.pop_i()?)),
                0xad => return Ok(Slot::L(frame.pop()?.as_i64())),
                0xae => return Ok(Slot::F(frame.pop()?.as_f64())),
                0xaf => return Ok(Slot::F(frame.pop()?.as_f64())),
                0xb0 => return Ok(Slot::I(frame.pop_i()?)),
                0xb1 => return Ok(Slot::I(0)),
                // iaload / laload / faload / daload / aaload：按指令宽度取元素
                // （元素槽本身带宽度，读出来再按指令语义归一）
                0x2e | 0x2f | 0x30 | 0x31 | 0x32 => {
                    let idx = frame.pop_i()?;
                    let arr = frame.pop_i()?;
                    let v = if arr < 0 {
                        let a = &self.arrays[(-arr - 1) as usize];
                        *a.get(idx as usize).unwrap_or(&Slot::I(0))
                    } else {
                        Slot::I(0)
                    };
                    frame.push(match op {
                        0x2f => Slot::L(v.as_i64()),   // laload
                        0x31 => Slot::F(v.as_f64()),   // daload
                        0x30 => Slot::F(v.as_f64()),   // faload
                        _ => Slot::I(v.as_i32()),      // iaload / aaload
                    });
                }
                // iastore / lastore / fastore / dastore / aastore
                0x4f | 0x50 | 0x51 | 0x52 | 0x53 => {
                    let v = frame.pop()?;
                    let idx = frame.pop_i()?;
                    let arr = frame.pop_i()?;
                    if arr < 0 && idx >= 0 {
                        let a = &mut self.arrays[(-arr - 1) as usize];
                        if (idx as usize) < a.len() {
                            a[idx as usize] = match op {
                                0x50 => Slot::L(v.as_i64()),  // lastore
                                0x52 => Slot::F(v.as_f64()),  // dastore
                                0x51 => Slot::F(v.as_f64()),  // fastore
                                _ => Slot::I(v.as_i32()),     // iastore / aastore
                            };
                        }
                    }
                }
                // arraylength
                0xbe => {
                    let arr = frame.pop_i()?;
                    let n = if arr < 0 { self.arrays[(-arr - 1) as usize].len() as i32 } else { 0 };
                    frame.push(Slot::I(n));
                }
                // newarray：操作数是元素类型码（10 = int / 11 = long / 7 = double ...）
                0xbc => {
                    let atype = body[pc];
                    pc += 1;
                    let n = frame.pop_i()?;
                    if n < 0 {
                        return err("数组长度为负");
                    }
                    let (desc, zero) = match atype {
                        11 => ("[J", Slot::L(0)),
                        7 => ("[D", Slot::F(0.0)),
                        6 => ("[F", Slot::F(0.0)),
                        _ => ("[I", Slot::I(0)),
                    };
                    self.arrays.push(vec![zero; n as usize]);
                    self.array_class.push(desc.to_string());
                    let handle = -((self.arrays.len() as i32));
                    frame.push(Slot::I(handle));
                }
                // anewarray：操作数是**元素类**的常量池索引
                0xbd => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let comp = self.class_name_of(idx)?;
                    let desc = if comp.starts_with('[') {
                        format!("[{comp}") // 元素本身是数组：[[I 之类
                    } else {
                        format!("[L{comp};")
                    };
                    let n = frame.pop_i()?;
                    if n < 0 {
                        return err("数组长度为负");
                    }
                    self.arrays.push(vec![Slot::I(0); n as usize]); // 引用元素默认 null
                    self.array_class.push(desc);
                    let handle = -((self.arrays.len() as i32));
                    frame.push(Slot::I(handle));
                }
                // new：按本类字段个数分配，并记下运行时类型名
                0xbb => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    self.objects.push(vec![Slot::I(0); self.field_count.max(1)]);
                    self.object_class.push(self.class_name_of(idx)?);
                    let handle = self.objects.len() as i32;
                    frame.push(Slot::I(handle));
                }
                // getfield：字段槽自带宽度，直接按槽取
                0xb4 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let info = self.method_ref(idx)?;
                    let obj = frame.pop_i()?;
                    let f = *self.field_index.get(&info.name).unwrap_or(&0);
                    let v = if obj > 0 {
                        *self.objects[obj as usize - 1].get(f).unwrap_or(&Slot::I(0))
                    } else {
                        Slot::I(0)
                    };
                    frame.push(v);
                }
                // putfield
                0xb5 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let info = self.method_ref(idx)?;
                    let value = frame.pop()?;
                    let obj = frame.pop_i()?;
                    let f = *self.field_index.get(&info.name).unwrap_or(&0);
                    if obj > 0 {
                        let slots = &mut self.objects[obj as usize - 1];
                        if f < slots.len() {
                            slots[f] = value;
                        }
                    }
                }
                // instanceof：压 0/1
                0xc1 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let target = self.class_name_of(idx)?;
                    let v = frame.pop_i()?;
                    frame.push(Slot::I(if self.is_instance(v, &target) { 1 } else { 0 }));
                }
                // checkcast：不匹配就按 ClassCastException 退出（本 VM 还没有异常机制）
                0xc0 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let target = self.class_name_of(idx)?;
                    let v = frame.pop_i()?;
                    if v != 0 && !self.is_instance(v, &target) {
                        return err(format!(
                            "ClassCastException: {} cannot be cast to {}",
                            self.type_name(v),
                            target.replace('/', ".")
                        ));
                    }
                    frame.push(Slot::I(v));
                }
                // ifnull / ifnonnull
                0xc6 | 0xc7 => {
                    let op_pc = pc - 1;
                    let off = i16::from_be_bytes([body[pc], body[pc + 1]]) as isize;
                    pc += 2;
                    let v = frame.pop_i()?;
                    let take = if op == 0xc6 { v == 0 } else { v != 0 };
                    if take {
                        pc = (op_pc as isize + off) as usize;
                    }
                }
                // 移位：int 版本右操作数取低 5 位、long 版本取低 6 位（与 JVM 一致）
                0x78 | 0x7a | 0x7c => {
                    let n = frame.pop_i()? & 31;
                    let a = frame.pop_i()?;
                    let r = match op {
                        0x78 => a.wrapping_shl(n as u32),                  // ishl
                        0x7a => a.wrapping_shr(n as u32),                  // ishr
                        _ => ((a as u32) >> n) as i32,                     // iushr
                    };
                    frame.push(Slot::I(r));
                }
                0x79 | 0x7b | 0x7d => {
                    let n = frame.pop_i()? & 63;
                    let a = frame.pop()?.as_i64();
                    let r = match op {
                        0x79 => a.wrapping_shl(n as u32),                  // lshl
                        0x7b => a.wrapping_shr(n as u32),                  // lshr
                        _ => ((a as u64) >> n) as i64,                     // lushr
                    };
                    frame.push(Slot::L(r));
                }
                // 位运算
                0x7e | 0x80 | 0x82 => {
                    let b = frame.pop_i()?;
                    let a = frame.pop_i()?;
                    let r = match op {
                        0x7e => a & b, // iand
                        0x80 => a | b, // ior
                        _ => a ^ b,    // ixor
                    };
                    frame.push(Slot::I(r));
                }
                0x7f | 0x81 | 0x83 => {
                    let b = frame.pop()?.as_i64();
                    let a = frame.pop()?.as_i64();
                    let r = match op {
                        0x7f => a & b, // land
                        0x81 => a | b, // lor
                        _ => a ^ b,    // lxor
                    };
                    frame.push(Slot::L(r));
                }
                // getstatic：目前只支持 System.out
                0xb2 => {
                    let _idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    frame.push(Slot::I(0));
                }
                // invokevirtual：只处理 PrintStream.println
                0xb6 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let info = self.method_ref(idx)?;
                    if info.name == "println" {
                        let arg = frame.pop()?;
                        let _recv = frame.pop()?;
                        let text = self.println_text(&info.desc, arg);
                        self.output.push(text);
                    } else if let Some(target) = self
                        .methods
                        .get(&(info.name.clone(), info.desc.clone()))
                        .copied()
                    {
                        // 本类方法的虚调用：接收者当第 0 个参数
                        let argc = count_args(&info.desc)?;
                        let mut args = Vec::with_capacity(argc + 1);
                        for _ in 0..argc {
                            args.push(frame.pop()?);
                        }
                        let recv = frame.pop()?;
                        args.reverse();
                        let mut all = vec![recv];
                        all.extend(args);
                        self.depth += 1;
                        let r = self.execute(target, &info.desc, &all, true)?;
                        self.depth -= 1;
                        if !info.desc.ends_with(")V") {
                            frame.push(r);
                        }
                    } else {
                        return err(format!("不支持的 invokevirtual: {}.{}", info.class, info.name));
                    }
                }
                // invokespecial：本类构造器真的执行
                0xb7 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let info = self.method_ref(idx)?;
                    if info.name == "<init>" {
                        let argc = count_args(&info.desc)?;
                        let mut args = Vec::with_capacity(argc + 1);
                        for _ in 0..argc {
                            args.push(frame.pop()?);
                        }
                        let obj = frame.pop()?;
                        args.reverse();
                        let mut all = vec![obj];
                        all.extend(args);
                        if let Some(target) = self.methods.get(&(info.name.clone(), info.desc.clone())).copied() {
                            self.depth += 1;
                            let _ = self.execute(target, &info.desc, &all, true);
                            self.depth -= 1;
                        }
                    } else {
                        let argc = count_args(&info.desc)?;
                        for _ in 0..argc + 1 {
                            frame.pop()?;
                        }
                    }
                }
                // invokestatic
                0xb8 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let info = self.method_ref(idx)?;
                    let argc = count_args(&info.desc)?;
                    let mut args = Vec::with_capacity(argc);
                    for _ in 0..argc {
                        args.push(frame.pop()?);
                    }
                    args.reverse();
                    let target = self
                        .methods
                        .get(&(info.name.clone(), info.desc.clone()))
                        .copied()
                        .ok_or_else(|| VmError { msg: format!("找不到方法 {}{}", info.name, info.desc) })?;
                    self.depth += 1;
                    let r = self.execute(target, &info.desc, &args, false)?;
                    self.depth -= 1;
                    if !info.desc.ends_with(")V") {
                        frame.push(r);
                    }
                }
                other => {
                    return err(format!("未实现的字节码 0x{other:02x} @pc={}", pc - 1));
                }
            }
        }
        Ok(Slot::I(0))
    }

    fn code_of(&self, m: &'a crate::Member) -> Result<(usize, &'a [u8]), VmError> {
        let code_name = self.class.constant_pool.iter().enumerate().find_map(|(i, e)| match e {
            Some(CpEntry::Utf8(b)) if b == b"Code" => Some(i as u16),
            _ => None,
        });
        let code_name = match code_name {
            Some(c) => c,
            None => return err("常量池里没有 Code"),
        };
        for a in &m.attributes {
            if a.name_index == code_name && a.info.len() >= 8 {
                let max_locals = u16::from_be_bytes([a.info[2], a.info[3]]) as usize;
                let code_len = u32::from_be_bytes([a.info[4], a.info[5], a.info[6], a.info[7]]) as usize;
                let start = 8;
                let end = (start + code_len).min(a.info.len());
                return Ok((max_locals, &a.info[start..end]));
            }
        }
        err("方法没有 Code 属性（抽象方法？）")
    }

    /// 跑 main([Ljava/lang/String;)V
    pub fn run_main(&mut self) -> Result<ExecResult, VmError> {
        let m = *self
            .methods
            .get(&("main".to_string(), "([Ljava/lang/String;)V".to_string()))
            .ok_or_else(|| VmError { msg: "找不到 main([Ljava/lang/String;)V".into() })?;
        let r = self.execute(m, "([Ljava/lang/String;)V", &[Slot::I(0)], false)?;
        Ok(ExecResult { return_value: r.as_i32(), output: std::mem::take(&mut self.output) })
    }
}

/// `(II)I` → 2（数的是**参数个数**，long/double 各算一个）
fn count_args(desc: &str) -> Result<usize, VmError> {
    let bytes = desc.as_bytes();
    let mut i = 1;
    let mut n = 0;
    while i < bytes.len() && bytes[i] != b')' {
        match bytes[i] {
            b'[' => {
                i += 1;
                while i < bytes.len() && bytes[i] == b'[' {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'L' {
                    while i < bytes.len() && bytes[i] != b';' {
                        i += 1;
                    }
                }
                i += 1;
                n += 1;
            }
            b'L' => {
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1;
                n += 1;
            }
            b'J' | b'D' => {
                i += 1;
                n += 1;
            }
            _ => {
                i += 1;
                n += 1;
            }
        }
    }
    Ok(n)
}

/// 从字节加载并执行 main
pub fn run_class_bytes(bytes: &[u8]) -> Result<ExecResult, VmError> {
    let cf = parse(bytes).map_err(|e| VmError { msg: format!("class 解析失败: {e}") })?;
    let mut vm = Vm::new(&cf);
    vm.run_main()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::compile;
    use crate::parser::parse_compilation_unit;

    fn compile_and_run(src: &str) -> Vec<String> {
        let cu = parse_compilation_unit(src).expect("parse");
        let classes = compile(&cu).expect("compile");
        let (_name, bytes) = &classes[0];
        run_class_bytes(bytes).expect("run").output
    }

    #[test]
    fn runs_arithmetic() {
        let out = compile_and_run(
            r#"
            class A {
                public static void main(String[] args) {
                    System.out.println(1 + 2 * 3);
                }
            }
        "#,
        );
        assert_eq!(out, vec!["7"]);
    }

    #[test]
    fn runs_while_loop() {
        let out = compile_and_run(
            r#"
            class B {
                static int sum(int n) {
                    int i = 0; int s = 0;
                    while (i < n) { s = s + i; i = i + 1; }
                    return s;
                }
                public static void main(String[] args) { System.out.println(sum(5)); }
            }
        "#,
        );
        assert_eq!(out, vec!["10"]);
    }

    #[test]
    fn runs_if_else() {
        let out = compile_and_run(
            r#"
            class C {
                static int max(int a, int b) { if (a > b) { return a; } return b; }
                public static void main(String[] args) {
                    System.out.println(max(7, 3));
                    System.out.println(max(2, 9));
                }
            }
        "#,
        );
        assert_eq!(out, vec!["7", "9"]);
    }

    #[test]
    fn runs_objects_and_fields() {
        let out = compile_and_run(
            r#"
            class Point {
                int x;
                Point(int v) { this.x = v; }
                static int twice(Point p) { return p.x + p.x; }
                public static void main(String[] args) {
                    Point p = new Point(21);
                    System.out.println(twice(p));
                }
            }
        "#,
        );
        assert_eq!(out, vec!["42"]);
    }

    #[test]
    fn runs_long_and_double() {
        let out = compile_and_run(
            r#"
            class LD {
                static long sum(long n) {
                    long s = 0L; long i = 0L;
                    while (i < n) { s = s + i; i = i + 1L; }
                    return s;
                }
                static double avg(double a, double b) { return (a + b) / 2.0; }
                public static void main(String[] args) {
                    System.out.println(sum(100000L));
                    System.out.println(avg(3.5, 0.1 + 0.2));
                    System.out.println(1.0e7);
                    System.out.println(-0.0);
                }
            }
        "#,
        );
        assert_eq!(out, vec!["4999950000", "1.9", "1.0E7", "-0.0"]);
    }

    #[test]
    fn counts_descriptor_args() {
        assert_eq!(count_args("()V").unwrap(), 0);
        assert_eq!(count_args("(II)I").unwrap(), 2);
        assert_eq!(count_args("([Ljava/lang/String;)V").unwrap(), 1);
        assert_eq!(count_args("(ILjava/lang/String;)V").unwrap(), 2);
        assert_eq!(count_args("(JD)D").unwrap(), 2);
    }
}
