//! 指令分派主循环：`Vm::execute`（含进入方法时的参数落位）。

use crate::jit::param_slots;
use crate::CpEntry;

use super::call::count_args;
use super::class::class_name_of;
use super::err;
use super::heap::RefKind;
use super::exception::thrown;
use super::frame::Frame;
use super::slot::Slot;
use super::Vm;
use super::VmError;

impl Vm {
    pub(super) fn ldc_int(&mut self, class: usize, index: u16) -> Result<i32, VmError> {
        let cf = self.store.classes[class].clone();
        match cf.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
            Some(CpEntry::Integer(v)) => Ok(*v as i32),
            Some(CpEntry::String(_)) => Ok(self.string_handle(class, index)), // 句柄进字符串堆
            other => err(format!("ldc 不支持的类型: {other:?}")),
        }
    }

    /// `ldc2_w`：long / double 常量
    pub(super) fn ldc2_w(&self, class: usize, index: u16) -> Result<Slot, VmError> {
        let cf = &self.store.classes[class];
        match cf.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
            Some(CpEntry::Long(v)) => Ok(Slot::L(*v as i64)),
            Some(CpEntry::Double(bits)) => Ok(Slot::F(f64::from_bits(*bits))),
            other => err(format!("ldc2_w 不支持的类型: {other:?}")),
        }
    }

    /// 执行一个方法：`class`/`method` 是它在类表里的位置；desc 形如 `(II)I`；
    /// `receiver` 表示 args[0] 是 this（实例方法）
    pub(super) fn execute(
        &mut self,
        class: usize,
        method: usize,
        desc: &str,
        args: &[Slot],
        receiver: bool,
    ) -> Result<Slot, VmError> {
        if self.depth > 64 {
            return err("调用深度超限（可能递归过深）");
        }
        // 每次调用先把「当前类」钉住：常量池、Code 属性都按它取（继承后方法来自不同类）
        let cf = self.store.classes[class].clone();
        let m = &cf.methods[method];
        let (max_locals, body, exc_table) = code_of(&cf, m)?;

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

        // 调用点上的异常：被调方抛出来的 Java 异常不能直接往上冒 ——
        // 先看本方法的异常表接不接得住（接住就按 handler 入口继续跑）。
        enum CallOutcome {
            Value(Slot),
            /// 交给本方法的 handler：(handler 偏移, 异常对象句柄)
            Handler(usize, i32),
        }
        macro_rules! call_or_handle {
            ($res:expr, $op_pc:expr) => {{
                match $res {
                    Ok(v) => CallOutcome::Value(v),
                    Err(e) => match e.exc {
                        Some(h) => match self.find_handler(&cf, &exc_table, $op_pc, h)? {
                            Some(t) => CallOutcome::Handler(t, h),
                            None => return Err(e),
                        },
                        None => return Err(e),
                    },
                }
            }};
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
                    let v = self.ldc_int(class, idx)?;
                    frame.push(Slot::I(v));
                }
                0x13 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let v = self.ldc_int(class, idx)?;
                    frame.push(Slot::I(v));
                }
                0x14 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let v = self.ldc2_w(class, idx)?;
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
                // athrow：沿**本方法**的异常表找 handler；本帧没有就带着异常对象往上抛
                0xbf => {
                    let op_pc = pc - 1;
                    let h = frame.pop_i()?;
                    match self.find_handler(&cf, &exc_table, op_pc, h)? {
                        Some(target) => {
                            pc = target;
                            // handler 入口的约定：操作数栈上就是那个异常对象
                            frame.stack.clear();
                            frame.push(Slot::I(h));
                        }
                        None => return Err(thrown(h)),
                    }
                }
                // iaload / laload / faload / daload / aaload / baload / caload / saload
                // （元素槽本身带宽度，读出来再按指令语义归一）
                0x2e | 0x2f | 0x30 | 0x31 | 0x32 | 0x33 | 0x34 | 0x35 => {
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
                // iastore / lastore / fastore / dastore / aastore / bastore / castore / sastore
                0x4f | 0x50 | 0x51 | 0x52 | 0x53 | 0x54 | 0x55 | 0x56 => {
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
                        4 => ("[Z", Slot::I(0)),
                        5 => ("[C", Slot::I(0)),
                        8 => ("[B", Slot::I(0)),
                        9 => ("[S", Slot::I(0)),
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
                    let comp = class_name_of(&cf, idx)?;
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
                // new：字段个数按**被创建的那个类**的布局（含父类字段）算
                0xbb => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let cls = class_name_of(&cf, idx)?;
                    // 首次 new 这个类时先跑它的 <clinit>
                    self.ensure_init(&cls)?;
                    let n = self.store.field_count(&cls);
                    self.objects.push(vec![Slot::I(0); n.max(1)]);
                    self.object_class.push(cls);
                    let handle = self.objects.len() as i32;
                    frame.push(Slot::I(handle));
                }
                // getfield：字段槽自带宽度，直接按槽取
                0xb4 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let info = self.method_ref(&cf, idx)?;
                    let obj = frame.pop_i()?;
                    // 偏移沿引用类的继承链解析；布局是「父类在前」，子类偏移与父类一致
                    let f = self.field_offset(obj, &info.class, &info.name)?;
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
                    let info = self.method_ref(&cf, idx)?;
                    let value = frame.pop()?;
                    let obj = frame.pop_i()?;
                    let f = self.field_offset(obj, &info.class, &info.name)?;
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
                    let target = class_name_of(&cf, idx)?;
                    let v = frame.pop_i()?;
                    frame.push(Slot::I(if self.is_instance(v, &target) { 1 } else { 0 }));
                }
                // checkcast：不匹配就按 ClassCastException 退出（本 VM 还没有异常机制）
                0xc0 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let target = class_name_of(&cf, idx)?;
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
                // getstatic：静态字段（System.out 仍是 PrintStream 占位，println 走内建）
                0xb2 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let info = self.method_ref(&cf, idx)?;
                    if info.class == "java/lang/System" && info.name == "out" {
                        frame.push(Slot::I(0));
                    } else {
                        // 首次访问这个类时先跑它的 <clinit>
                        self.ensure_init(&info.class)?;
                        let v = self
                            .statics
                            .get(&(info.class.clone(), info.name.clone()))
                            .copied()
                            .unwrap_or(Slot::I(0));
                        frame.push(v);
                    }
                }
                // putstatic
                0xb3 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let info = self.method_ref(&cf, idx)?;
                    let value = frame.pop()?;
                    self.ensure_init(&info.class)?;
                    self.statics.insert((info.class, info.name), value);
                }
                // invokevirtual / invokeinterface：println 走内建；
                // 其余按**接收者的运行时类**虚分派（接口与类走同一条路，只是指令编码多两字节）
                0xb6 | 0xb9 => {
                    let op_pc = pc - 1;
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    if op == 0xb9 {
                        pc += 2; // count + 保留字节
                    }
                    let info = self.method_ref(&cf, idx)?;
                    if info.name == "println" {
                        let arg = frame.pop()?;
                        let _recv = frame.pop()?;
                        let text = self.println_text(&info.desc, arg);
                        self.output.push(text);
                    } else if info.class == "java/lang/String"
                        && info.name == "equals"
                        && info.desc == "(Ljava/lang/Object;)Z"
                    {
                        // 内置：按内容比（`String` 没有 .class，走不到 resolve_method）
                        let arg = frame.pop()?;
                        let recv = frame.pop()?;
                        let a = match self.ref_kind(recv.as_i32()) {
                            RefKind::Str => Some(self.string_text(recv.as_i32())),
                            _ => None,
                        };
                        let b = match self.ref_kind(arg.as_i32()) {
                            RefKind::Str => Some(self.string_text(arg.as_i32())),
                            _ => None,
                        };
                        frame.push(Slot::I(if a.is_some() && a == b { 1 } else { 0 }));
                    } else if info.class == "java/lang/StringBuilder" {
                        // 内置：字符串拼接（`StringBuilder` 没有 .class，走不到 resolve_method）
                        if info.name == "append" {
                            let arg = frame.pop()?;
                            let recv = frame.pop()?.as_i32();
                            let v = self.sb_append(recv, &info.desc, arg)?;
                            frame.push(v); // append 返回 this，调用点会 pop 掉
                        } else if info.name == "toString" {
                            let recv = frame.pop()?.as_i32();
                            let h = self.sb_finish(recv);
                            frame.push(Slot::I(h));
                        } else {
                            return Err(VmError {
                                msg: format!("java.lang.StringBuilder 没有方法 `{}`", info.name),
                                exc: None,
                            });
                        }
                    } else {
                        let argc = count_args(&info.desc)?;
                        let mut args = Vec::with_capacity(argc + 1);
                        for _ in 0..argc {
                            args.push(frame.pop()?);
                        }
                        let recv = frame.pop()?;
                        args.reverse();
                        let mut all = vec![recv];
                        all.extend(args);
                        // 从对象的**实际类**开始解析（这就是覆写的分派点）；
                        // 拿不到运行时类才退回引用类，再不行才报错。
                        let actual = match self.ref_kind(recv.as_i32()) {
                            RefKind::Obj(c) => Some(c),
                            _ => None,
                        };
                        let target = actual
                            .as_deref()
                            .and_then(|c| self.store.resolve_method(c, &info.name, &info.desc))
                            .or_else(|| self.store.resolve_method(&info.class, &info.name, &info.desc))
                            .ok_or_else(|| VmError {
                                msg: format!("找不到方法 {}.{}{}", info.class, info.name, info.desc),
                                exc: None,
                            })?;
                        self.depth += 1;
                        let r = call_or_handle!(self.execute(target.0, target.1, &info.desc, &all, true), op_pc);
                        self.depth -= 1;
                        match r {
                            CallOutcome::Value(v) => {
                                if !info.desc.ends_with(")V") {
                                    frame.push(v);
                                }
                            }
                            CallOutcome::Handler(t, h) => {
                                pc = t;
                                frame.stack.clear();
                                frame.push(Slot::I(h));
                                continue;
                            }
                        }
                    }
                }
                // invokespecial：构造器 / super.m()，从**引用类**（不是运行时类）开始解析
                0xb7 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let info = self.method_ref(&cf, idx)?;
                    let argc = count_args(&info.desc)?;
                    let mut args = Vec::with_capacity(argc + 1);
                    for _ in 0..argc {
                        args.push(frame.pop()?);
                    }
                    let obj = frame.pop()?;
                    args.reverse();
                    let mut all = vec![obj];
                    all.extend(args);
                    match self.store.resolve_method(&info.class, &info.name, &info.desc) {
                        Some((ci, mi)) => {
                            self.depth += 1;
                            let r = self.execute(ci, mi, &info.desc, &all, true);
                            self.depth -= 1;
                            // super.m() 有返回值（构造器没有）：要压回栈上
                            if info.name != "<init>" && !info.desc.ends_with(")V") {
                                frame.push(r?);
                            }
                        }
                        None if info.desc == "(Ljava/lang/String;)V" => {
                            // java.* 的构造器（`new RuntimeException("boom")`）我们没有字节码：
                            // 只把消息记在异常对象上，打印未捕获异常时才用得到。
                            if let Some(msg) = all.get(1) {
                                self.exc_msgs.insert(all[0].as_i32(), msg.as_i32());
                            }
                        }
                        None if info.name == "<init>" => {}
                        None => {
                            return Err(VmError {
                                msg: format!("找不到方法 {}.{}{}", info.class, info.name, info.desc),
                                exc: None,
                            })
                        }
                    }
                }
                // invokestatic：沿引用类的继承链解析（静态方法也算继承来的）
                0xb8 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let info = self.method_ref(&cf, idx)?;
                    // 首次调用这个类的静态方法时先跑它的 <clinit>
                    self.ensure_init(&info.class)?;
                    let argc = count_args(&info.desc)?;
                    let mut args = Vec::with_capacity(argc);
                    for _ in 0..argc {
                        args.push(frame.pop()?);
                    }
                    args.reverse();
                    let target = self
                        .store
                        .resolve_method(&info.class, &info.name, &info.desc)
                        .ok_or_else(|| VmError { msg: format!("找不到方法 {}{}", info.name, info.desc), exc: None })?;
                    self.depth += 1;
                    let op_pc = pc - 3;
                    let r = call_or_handle!(self.execute(target.0, target.1, &info.desc, &args, false), op_pc);
                    self.depth -= 1;
                    match r {
                        CallOutcome::Value(v) => {
                            if !info.desc.ends_with(")V") {
                                frame.push(v);
                            }
                        }
                        CallOutcome::Handler(t, h) => {
                            pc = t;
                            frame.stack.clear();
                            frame.push(Slot::I(h));
                            continue;
                        }
                    }
                }
                // multianewarray：一次建出多维数组（内层数组递归建）
                0xc5 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let ndims = body[pc] as usize;
                    pc += 1;
                    let desc = class_name_of(&cf, idx)?;
                    let mut counts = Vec::with_capacity(ndims);
                    for _ in 0..ndims {
                        counts.push(frame.pop_i()?);
                    }
                    counts.reverse();
                    let h = self.new_multi(&desc, &counts)?;
                    frame.push(Slot::I(h));
                }
                other => {
                    return err(format!("未实现的字节码 0x{other:02x} @pc={}", pc - 1));
                }
            }
        }
        Ok(Slot::I(0))
    }

    /// 多维数组：`desc` 是完整描述符（`[[I`），`counts` 每维一个长度。
    /// 内层数组先建，再塞进外层（和 JVM 的 multianewarray 语义一致）。
    fn new_multi(&mut self, desc: &str, counts: &[i32]) -> Result<i32, VmError> {
        let n = counts.first().copied().unwrap_or(0);
        if n < 0 {
            return err("数组长度为负");
        }
        self.arrays.push(vec![Slot::I(0); n as usize]);
        self.array_class.push(desc.to_string());
        let handle = -(self.arrays.len() as i32);
        if counts.len() > 1 {
            let elem_desc = &desc[1..];
            for i in 0..n as usize {
                let sub = self.new_multi(elem_desc, &counts[1..])?;
                self.arrays[(handle.abs() - 1) as usize][i] = Slot::I(sub);
            }
        }
        Ok(handle)
    }

    /// 字段偏移：先按引用类的继承链找，找不到再按对象的**运行时类**找
    fn field_offset(&self, obj: i32, owner: &str, name: &str) -> Result<usize, VmError> {
        self.field_index_of_object(obj, name)
            .or_else(|| self.store.field_index(owner, name))
            .ok_or_else(|| VmError { msg: format!("未知字段 {owner}.{name}"), exc: None })
    }

    fn field_index_of_object(&self, obj: i32, name: &str) -> Option<usize> {
        if obj <= 0 {
            return None;
        }
        let c = self.object_class.get(obj as usize - 1)?;
        self.store.field_index(c, name)
    }
}

/// 返回 (max_locals, 字节码, 异常表)。异常表每项 (start, end, handler, catch_type)，
/// catch_type = 0 表示 catch-all（finally）。
pub(super) fn code_of<'a>(
    cf: &crate::ClassFile,
    m: &'a crate::Member,
) -> Result<(usize, &'a [u8], Vec<(u16, u16, u16, u16)>), VmError> {
    let code_name = cf.constant_pool.iter().enumerate().find_map(|(i, e)| match e {
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
            // 异常表紧跟在字节码后面：u2 个数 + 每项 4 个 u2
            let mut table = Vec::new();
            let mut q = end;
            if q + 2 <= a.info.len() {
                let n = u16::from_be_bytes([a.info[q], a.info[q + 1]]) as usize;
                q += 2;
                for _ in 0..n {
                    if q + 8 > a.info.len() {
                        break;
                    }
                    let rd = |i: usize| u16::from_be_bytes([a.info[i], a.info[i + 1]]);
                    table.push((rd(q), rd(q + 2), rd(q + 4), rd(q + 6)));
                    q += 8;
                }
            }
            return Ok((max_locals, &a.info[start..end], table));
        }
    }
    err("方法没有 Code 属性（抽象方法？）")
}
