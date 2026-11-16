//! 常量池构建器 + 字节码发射 + class 文件生成。
//!
//! 目标：从"一个类的描述"直接产出 HotSpot 能加载执行的 `.class`。
//! 目前覆盖直线代码（无分支），因此不需要 StackMapTable。

use crate::{write, Attribute, ClassFile, Member};


use super::class::{ACC_PUBLIC, ACC_STATIC, ACC_SUPER};
use super::constpool::CpBuilder;

/// 字节码发射器
#[derive(Default)]
pub struct Code {
    pub bytes: Vec<u8>,
    pub max_stack: u16,
    pub max_locals: u16,
}

impl Code {
    pub fn new(max_locals: u16) -> Self {
        Code { bytes: Vec::new(), max_stack: 0, max_locals }
    }

    fn bump(&mut self, s: u16) {
        if s > self.max_stack {
            self.max_stack = s;
        }
    }

    pub fn aload_0(&mut self) -> &mut Self {
        self.bytes.push(0x2A);
        self.bump(1);
        self
    }

    pub fn invokespecial(&mut self, idx: u16) -> &mut Self {
        self.bytes.push(0xB7);
        self.bytes.extend_from_slice(&idx.to_be_bytes());
        self.bump(1);
        self
    }

    pub fn getstatic(&mut self, idx: u16) -> &mut Self {
        self.bytes.push(0xB2);
        self.bytes.extend_from_slice(&idx.to_be_bytes());
        self.bump(1);
        self
    }

    pub fn invokevirtual(&mut self, idx: u16) -> &mut Self {
        self.bytes.push(0xB6);
        self.bytes.extend_from_slice(&idx.to_be_bytes());
        self.bump(2);
        self
    }

    pub fn ldc_string(&mut self, idx: u16) -> &mut Self {
        if idx <= 255 {
            self.bytes.push(0x12);
            self.bytes.push(idx as u8);
        } else {
            self.bytes.push(0x13);
            self.bytes.extend_from_slice(&idx.to_be_bytes());
        }
        self.bump(1);
        self
    }

    pub fn iadd(&mut self) -> &mut Self {
        self.bytes.push(0x60);
        self
    }

    pub fn ireturn(&mut self) -> &mut Self {
        self.bytes.push(0xAC);
        self
    }

    pub fn return_(&mut self) -> &mut Self {
        self.bytes.push(0xB1);
        self
    }

    pub fn iload_0(&mut self) -> &mut Self {
        self.bytes.push(0x1A);
        self.bump(1);
        self
    }

    pub fn iload_1(&mut self) -> &mut Self {
        self.bytes.push(0x1B);
        self.bump(1);
        self
    }

    pub fn to_code_attribute(&self, name_index: u16) -> Attribute {
        let mut info = Vec::new();
        info.extend_from_slice(&self.max_stack.to_be_bytes());
        info.extend_from_slice(&self.max_locals.to_be_bytes());
        info.extend_from_slice(&(self.bytes.len() as u32).to_be_bytes());
        info.extend_from_slice(&self.bytes);
        info.extend_from_slice(&0u16.to_be_bytes()); // exception_table_length
        info.extend_from_slice(&0u16.to_be_bytes()); // attributes_count
        Attribute { name_index, info }
    }
}

pub struct MethodSpec {
    pub access_flags: u16,
    pub name: String,
    pub descriptor: String,
    pub code: Code,
}

pub struct ClassSpec {
    pub name: String,
    pub super_name: String,
    pub access_flags: u16,
    pub methods: Vec<MethodSpec>,
}


/// 生成 `.class` 字节（自建常量池）
pub fn generate(spec: &ClassSpec) -> Vec<u8> {
    let mut cp = CpBuilder::new();
    generate_in(&mut cp, spec)
}

/// 用调用方提供的常量池生成（保证字节码里的索引与之一致）
pub fn generate_in(cp: &mut CpBuilder, spec: &ClassSpec) -> Vec<u8> {

    let this_class = cp.class(&spec.name);
    let super_class = cp.class(&spec.super_name);
    let code_name = cp.utf8("Code");

    let mut methods = Vec::new();
    for m in &spec.methods {
        let name_index = cp.utf8(&m.name);
        let descriptor_index = cp.utf8(&m.descriptor);
        let attr = m.code.to_code_attribute(code_name);
        methods.push(Member {
            access_flags: m.access_flags,
            name_index,
            descriptor_index,
            attributes: vec![attr],
        });
    }

    let cf = ClassFile {
        minor_version: 0,
        major_version: 69, // Java 25
        constant_pool: cp.snapshot(),
        access_flags: spec.access_flags | ACC_SUPER,
        this_class,
        super_class,
        interfaces: Vec::new(),
        fields: Vec::new(),
        methods,
        attributes: Vec::new(),
    };
    write(&cf)
}

/// 生成一个"打印一行字"的 main 方法（直线代码）
pub fn simple_main(class_name: &str, message: &str) -> Vec<u8> {
    let mut cp = CpBuilder::new();

    let this_class = cp.class(class_name);
    let super_class = cp.class("java/lang/Object");
    let code_name = cp.utf8("Code");

    let obj_init = cp.method_ref("java/lang/Object", "<init>", "()V");
    let system_out = cp.field_ref("java/lang/System", "out", "Ljava/io/PrintStream;");
    let msg_idx = cp.string(message);
    let println = cp.method_ref("java/io/PrintStream", "println", "(Ljava/lang/String;)V");

    let mut ctor = Code::new(1);
    ctor.aload_0().invokespecial(obj_init).return_();

    let mut main = Code::new(1);
    main.getstatic(system_out).ldc_string(msg_idx).invokevirtual(println).return_();

    let ctor_name = cp.utf8("<init>");
    let ctor_desc = cp.utf8("()V");
    let main_name = cp.utf8("main");
    let main_desc = cp.utf8("([Ljava/lang/String;)V");

    let methods = vec![
        Member {
            access_flags: ACC_PUBLIC,
            name_index: ctor_name,
            descriptor_index: ctor_desc,
            attributes: vec![ctor.to_code_attribute(code_name)],
        },
        Member {
            access_flags: ACC_PUBLIC | ACC_STATIC,
            name_index: main_name,
            descriptor_index: main_desc,
            attributes: vec![main.to_code_attribute(code_name)],
        },
    ];

    let cf = ClassFile {
        minor_version: 0,
        major_version: 69,
        constant_pool: cp.entries(),
        access_flags: ACC_PUBLIC | ACC_SUPER,
        this_class,
        super_class,
        interfaces: Vec::new(),
        fields: Vec::new(),
        methods,
        attributes: Vec::new(),
    };
    write(&cf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    #[test]
    fn generates_parsable_class() {
        let bytes = simple_main("Hello", "hello from rustjavac");
        let cf = parse(&bytes).expect("生成的字节能被自己的解析器读回");
        assert_eq!(cf.major_version, 69);
        assert_eq!(cf.this_class_name().as_deref(), Some("Hello"));
        assert_eq!(cf.methods.len(), 2);
        let names: Vec<_> = cf
            .methods
            .iter()
            .map(|m| String::from_utf8_lossy(cf.utf8(m.name_index).unwrap()).into_owned())
            .collect();
        assert_eq!(names, vec!["<init>", "main"]);
    }

    #[test]
    fn constant_pool_dedupes() {
        let mut cp = CpBuilder::new();
        let a = cp.utf8("Hello");
        let b = cp.utf8("Hello");
        assert_eq!(a, b);
        let c1 = cp.class("Hello");
        let c2 = cp.class("Hello");
        assert_eq!(c1, c2);
    }

    #[test]
    fn code_attribute_is_well_formed() {
        let mut cp = CpBuilder::new();
        let code_name = cp.utf8("Code");
        let mut code = Code::new(1);
        code.aload_0().return_();
        let attr = code.to_code_attribute(code_name);
        // max_stack(2) + max_locals(2) + code_length(4) + bytes(2) + exc(2) + attrs(2)
        assert_eq!(attr.info.len(), 2 + 2 + 4 + 2 + 2 + 2);
    }
}
