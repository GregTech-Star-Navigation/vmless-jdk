//! `rustjava` 的最小解释器：不依赖任何 JVM，直接执行 class 文件。
//!
//! 覆盖范围（够跑我们自己编译器产出的东西）：
//! int 常量、iload/istore、算术、比较与跳转、goto、pop、
//! `System.out.println(int)`、`invokestatic`、`return`/`ireturn`。

use crate::{parse, CpEntry, ClassFile};
use std::collections::HashMap;

#[derive(Debug)]
pub struct VmError {
    pub msg: String,
}

fn err<T>(msg: impl Into<String>) -> Result<T, VmError> {
    Err(VmError { msg: msg.into() })
}

/// 执行结果
pub struct ExecResult {
    pub return_value: i32,
    pub output: Vec<String>,
}

struct Frame {
    locals: Vec<i32>,
    stack: Vec<i32>,
}

impl Frame {
    fn new(max_locals: usize) -> Self {
        Frame { locals: vec![0; max_locals.max(1)], stack: Vec::new() }
    }
    fn push(&mut self, v: i32) {
        self.stack.push(v);
    }
    fn pop(&mut self) -> Result<i32, VmError> {
        self.stack.pop().ok_or_else(|| VmError { msg: "操作数栈下溢".into() })
    }
}

struct MethodRefInfo {
    class: String,
    name: String,
    desc: String,
}

pub struct Vm<'a> {
    class: &'a ClassFile,
    methods: HashMap<(String, String), &'a crate::Member>,
    output: Vec<String>,
    depth: usize,
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
        Vm { class, methods, output: Vec::new(), depth: 0 }
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

    fn ldc_int(&self, index: u16) -> Result<i32, VmError> {
        match self.class.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
            Some(CpEntry::Integer(v)) => Ok(*v as i32),
            Some(CpEntry::String(_)) => Ok(index as i32), // 句柄就是常量池索引
            other => err(format!("ldc 不支持的类型: {other:?}")),
        }
    }

    fn string_at(&self, index: u16) -> Option<String> {
        match self.class.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
            Some(CpEntry::String(u)) => self
                .class
                .utf8(*u)
                .map(|b| String::from_utf8_lossy(b).into_owned()),
            _ => None,
        }
    }

    /// 执行一个方法：desc 形如 `(II)I`
    fn execute(&mut self, m: &'a crate::Member, args: &[i32]) -> Result<i32, VmError> {
        if self.depth > 64 {
            return err("调用深度超限（可能递归过深）");
        }
        let code = self.code_of(m)?;
        let (max_locals, body) = code;

        let mut frame = Frame::new(max_locals.max(args.len()));
        for (i, a) in args.iter().enumerate() {
            frame.locals[i] = *a;
        }

        let mut pc = 0usize;
        while pc < body.len() {
            let op = body[pc];
            if std::env::var("VMLESS_TRACE").is_ok() {
                eprintln!("  pc={pc} op={op:02x} stack={:?} locals={:?}", frame.stack, frame.locals);
            }
            pc += 1;
            match op {
                0x00 => {}
                0x03..=0x08 => frame.push(op as i32 - 0x03),
                0x02 => frame.push(-1),
                0x10 => {
                    let v = body[pc] as i8 as i32;
                    pc += 1;
                    frame.push(v);
                }
                0x11 => {
                    let v = i16::from_be_bytes([body[pc], body[pc + 1]]) as i32;
                    pc += 2;
                    frame.push(v);
                }
                0x12 => {
                    let idx = body[pc] as u16;
                    pc += 1;
                    let v = self.ldc_int(idx)?;
                    frame.push(v);
                }
                0x13 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let v = self.ldc_int(idx)?;
                    frame.push(v);
                }
                0x15 => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    frame.push(*frame.locals.get(slot).unwrap_or(&0));
                }
                0x1a..=0x1d => {
                    let slot = (op - 0x1a) as usize;
                    frame.push(*frame.locals.get(slot).unwrap_or(&0));
                }
                0x36 => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = frame.pop()?;
                    if slot >= frame.locals.len() {
                        frame.locals.resize(slot + 1, 0);
                    }
                    frame.locals[slot] = v;
                }
                0x3b..=0x3e => {
                    let slot = (op - 0x3b) as usize;
                    let v = frame.pop()?;
                    if slot >= frame.locals.len() {
                        frame.locals.resize(slot + 1, 0);
                    }
                    frame.locals[slot] = v;
                }
                0x57 => {
                    frame.pop()?;
                }
                0x60 | 0x64 | 0x68 | 0x6c | 0x70 => {
                    let b = frame.pop()?;
                    let a = frame.pop()?;
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
                    frame.push(r);
                }
                0x74 => {
                    let a = frame.pop()?;
                    frame.push(a.wrapping_neg());
                }
                // 一元条件跳转（偏移相对操作码地址）
                0x99 | 0x9a | 0x9b | 0x9c | 0x9d | 0x9e => {
                    let op_pc = pc - 1;
                    let off = i16::from_be_bytes([body[pc], body[pc + 1]]) as isize;
                    pc += 2;
                    let v = frame.pop()?;
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
                // 二元整数比较跳转
                0x9f..=0xa4 => {
                    let op_pc = pc - 1;
                    let off = i16::from_be_bytes([body[pc], body[pc + 1]]) as isize;
                    pc += 2;
                    let b = frame.pop()?;
                    let a = frame.pop()?;
                    let take = match op {
                        0x9f => a == b,
                        0xa0 => a != b,
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
                    pc += 2;
                    pc = (op_pc as isize + off) as usize;
                }
                0xac => {
                    return frame.pop();
                }
                0xb1 => return Ok(0),
                // getstatic：目前只支持 System.out
                0xb2 => {
                    let _idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    frame.push(0);
                }
                // invokevirtual：只处理 PrintStream.println
                0xb6 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let info = self.method_ref(idx)?;
                    if info.name == "println" {
                        let arg = frame.pop()?;
                        let _recv = frame.pop()?;
                        let text = if info.desc.contains("String") {
                            self.string_at(arg as u16).unwrap_or_default()
                        } else {
                            arg.to_string()
                        };
                        self.output.push(text);
                    } else {
                        return err(format!("不支持的 invokevirtual: {}.{}", info.class, info.name));
                    }
                }
                // invokespecial：构造器调用，直接当空操作
                0xb7 => {
                    let _idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let n = frame.pop()?;
                    let _ = n;
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
                    let r = self.execute(target, &args)?;
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
        Ok(0)
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
        let r = self.execute(m, &[0])?;
        Ok(ExecResult { return_value: r, output: std::mem::take(&mut self.output) })
    }
}

/// `(II)I` → 2
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
    fn counts_descriptor_args() {
        assert_eq!(count_args("()V").unwrap(), 0);
        assert_eq!(count_args("(II)I").unwrap(), 2);
        assert_eq!(count_args("([Ljava/lang/String;)V").unwrap(), 1);
        assert_eq!(count_args("(ILjava/lang/String;)V").unwrap(), 2);
    }
}
