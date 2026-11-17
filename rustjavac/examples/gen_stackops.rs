//! 临时工具：手搓一个只用 dup_x1 / dup_x2 / dup2 / dup2_x1 / dup2_x2 / swap / pop2
//! 的类（我们的编译器不产这些指令，javac 也几乎不产），用来实测三路引擎的语义。
//! 每段都是「[ref] + 操作数 → 算成一个 int 再 println(I)V」，三路输出必须一致。
use classfile::codegen::{generate_in, ClassSpec, Code, MethodSpec, CpBuilder};
use classfile::codegen::{ACC_PUBLIC, ACC_STATIC};

/// `ldc2_w <idx>`
fn ldc2(idx: u16) -> Vec<u8> {
    let mut v = vec![0x14];
    v.extend_from_slice(&idx.to_be_bytes());
    v
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    let mut cp = CpBuilder::new();
    let out_fld = cp.field_ref("java/lang/System", "out", "Ljava/io/PrintStream;");
    let println = cp.method_ref("java/io/PrintStream", "println", "(I)V");
    let obj_init = cp.method_ref("java/lang/Object", "<init>", "()V");
    let l3 = cp.long(3);
    let l4 = cp.long(4);
    let l6 = cp.long(6);
    let l1 = cp.long(1);

    let mut c = Code::new(1);
    c.max_stack = 12;
    let mut seg = |c: &mut Code, code: Vec<u8>| {
        c.bytes.push(0xB2);
        c.bytes.extend_from_slice(&out_fld.to_be_bytes());
        c.bytes.extend_from_slice(&code);
        c.bytes.push(0xB6);
        c.bytes.extend_from_slice(&println.to_be_bytes());
    };
    // 1) swap：10,20 → 20-10 = 10
    seg(&mut c, vec![0x10, 10, 0x10, 20, 0x5f, 0x64]);
    // 2) dup_x1：[3,4] → [4,3,4] → (3+4)*4 = 28
    seg(&mut c, vec![0x06, 0x07, 0x5a, 0x60, 0x68]);
    // 3) dup2（两个 1 槽）：[3,4] → [3,4,3,4] → 14
    seg(&mut c, vec![0x06, 0x07, 0x5c, 0x60, 0x60, 0x60]);
    // 4) dup2（宽值）：[0L] → [0L,0L] → 0
    seg(&mut c, vec![0x0a, 0x5c, 0x61, 0x88]);
    // 5) dup2_x1（形式 1）：[3,4,5] → [4,5,3,4,5] → 21
    seg(&mut c, vec![0x06, 0x07, 0x08, 0x5d, 0x60, 0x60, 0x60, 0x60]);
    // 6) dup2_x1（形式 2，栈顶是宽值）：[2, 3L] → [3L, 2, 3L] → l2i → [3L,2,3]
    //    → iadd → [3L,5] → iadd → 8
    let mut s6 = vec![0x05];
    s6.extend(ldc2(l3));
    // [3L,2,3] → l2i → [3L,2,3] → iadd → [3L,5] → pop → [3L] → l2i → 3
    s6.extend_from_slice(&[0x5d, 0x88, 0x60, 0x57, 0x88]);
    seg(&mut c, s6);
    // 7) dup2_x2（形式 1）：[1,2,3,4] → [3,4,1,2,3,4] → 17
    seg(&mut c, vec![0x04, 0x05, 0x06, 0x07, 0x5e, 0x60, 0x60, 0x60, 0x60, 0x60]);
    // 8) dup2_x2（形式 4，两个宽值）：[3L,4L] → [4L,3L,4L] → ladd → [4L,7L] → ladd → 11
    let mut s8 = ldc2(l3);
    s8.extend(ldc2(l4));
    s8.extend_from_slice(&[0x5e, 0x61, 0x61, 0x88]);
    seg(&mut c, s8);
    // 9) dup_x2（形式 1）：[1,2,3] → [3,1,2,3] → 6
    seg(&mut c, vec![0x04, 0x05, 0x06, 0x5b, 0x60, 0x60, 0x60]);
    // 10) dup_x2（形式 2，下面那个宽）：[6L,4] → [4,6L,4] → i2l → ladd → l2i → 14
    let mut s10 = ldc2(l6);
    s10.extend_from_slice(&[0x08, 0x5b, 0x85, 0x61, 0x88, 0x60]);
    seg(&mut c, s10);
    // 11) pop2：宽值一个 + 两个 1 槽各弹一次 → 42
    let mut s11 = ldc2(l1);
    s11.extend_from_slice(&[0x58, 0x04, 0x05, 0x58, 0x11, 0x00, 0x2a]);
    seg(&mut c, s11);
    c.bytes.push(0xB1); // return

    let mut ctor = Code::new(1);
    ctor.aload_0();
    ctor.bytes.push(0xB7);
    ctor.bytes.extend_from_slice(&obj_init.to_be_bytes());
    ctor.bytes.push(0xB1);

    let spec = ClassSpec {
        name: "StackOps".into(),
        super_name: "java/lang/Object".into(),
        access_flags: ACC_PUBLIC,
        methods: vec![
            MethodSpec { access_flags: ACC_PUBLIC, name: "<init>".into(), descriptor: "()V".into(), code: ctor },
            MethodSpec {
                access_flags: ACC_PUBLIC | ACC_STATIC,
                name: "main".into(),
                descriptor: "([Ljava/lang/String;)V".into(),
                code: c,
            },
        ],
    };
    let bytes = generate_in(&mut cp, &spec);
    let p = std::path::Path::new(&out).join("StackOps.class");
    std::fs::write(&p, &bytes).expect("write");
    println!("{} 字节 → {}", bytes.len(), p.display());
}
