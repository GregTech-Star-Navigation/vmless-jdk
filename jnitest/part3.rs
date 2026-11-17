
// ============================================================ 调用 Java 方法

/// 方法描述符 → 参数类型序列（每个元素是描述符里的那个类型：`I` / `L…;` / `[…`）
pub(crate) fn split_params(desc: &str) -> Vec<String> {
    let b = desc.as_bytes();
    let mut i = 1;
    let mut out = Vec::new();
    while i < b.len() && b[i] != b')' {
        let start = i;
        while i < b.len() && b[i] == b'[' {
            i += 1;
        }
        if i < b.len() && b[i] == b'L' {
            while i < b.len() && b[i] != b';' {
                i += 1;
            }
            i += 1;
        } else {
            i += 1;
        }
        out.push(desc[start..i.min(desc.len())].to_string());
    }
    out
}

/// 描述符 → 返回类型（`V` / `I` / `L…;` …）
pub(crate) fn ret_type(desc: &str) -> String {
    let i = desc.rfind(')').map(|i| i + 1).unwrap_or(desc.len());
    desc[i.min(desc.len())..].to_string()
}

/// 调用一个 Java 方法。`args` **不含** receiver（实例方法由 `obj_or_class` 提供）。
pub(crate) unsafe fn call_java(
    recv: i32,
    m: &MethodId,
    mut args: Vec<Slot>,
    nonvirtual: bool,
) -> Result<Slot, VmError> {
    let class = if m.is_static {
        m.class.clone()
    } else if nonvirtual {
        m.class.clone()
    } else {
        with_vm(|vm| runtime_class(vm, recv)).flatten().unwrap_or_else(|| m.class.clone())
    };
    let target = with_vm(|vm| {
        if m.is_static || nonvirtual {
            vm.store.method_in_class(&class, &m.name, &m.desc)
        } else {
            vm.store.resolve_method(&class, &m.name, &m.desc)
        }
    })
    .flatten();
    let Some(target) = target else {
        let h = with_vm(|vm| {
            throw_new(vm, "java/lang/NoSuchMethodError", &format!("{}.{}{}", class, m.name, m.desc))
        })
        .unwrap_or(0);
        set_pending(h);
        return Err(VmError::thrown(h));
    };
    if !m.is_static {
        args.insert(0, Slot::I(recv));
    }
    match with_vm(|vm| vm.execute(target.0, target.1, &m.desc, &args, !m.is_static)) {
        Some(Ok(v)) => Ok(v),
        Some(Err(e)) => {
            let h = match e.exc {
                Some(h) => h,
                None => with_vm(|vm| throw_new(vm, "java/lang/RuntimeException", &e.msg)).unwrap_or(0),
            };
            set_pending(h);
            Err(VmError::thrown(h))
        }
        None => Err(VmError { msg: "没有活动的 native 调用上下文".into(), exc: None }),
    }
}

/// `jvalue[]`（`CallXMethodA` 的 `const jvalue*`）→ 槽位。
///
/// `jvalue` 是 8 字节的 union，所以一律按 8 字节读，再按描述符的类型解释。
pub(crate) unsafe fn jvalues_to_slots(args: *const u64, types: &[String]) -> Vec<Slot> {
    let mut out = Vec::with_capacity(types.len());
    for (i, t) in types.iter().enumerate() {
        let raw = if args.is_null() { 0 } else { *args.add(i) };
        out.push(bits_to_slot(raw, t));
    }
    out
}

/// 8 字节位模式 + 类型 → 槽位
pub(crate) fn bits_to_slot(raw: u64, t: &str) -> Slot {
    match t.as_bytes().first().copied() {
        Some(b'J') => Slot::L(raw as i64),
        Some(b'F') => Slot::Fl(f32::from_bits(raw as u32)),
        Some(b'D') => Slot::F(f64::from_bits(raw)),
        _ => Slot::I(raw as i32),
    }
}

/// 槽位 → 8 字节位模式（反向：给 `jvalue[]` 与 `...` 用）
fn slot_to_bits(v: Slot) -> u64 {
    match v {
        Slot::I(x) => x as u32 as u64,
        Slot::L(x) => x as u64,
        Slot::Fl(x) => x.to_bits() as u64,
        Slot::F(x) => x.to_bits(),
    }
}

// ------------------------------------------------------------ `...` 形态的 ABI 垫片
//
// `CallVoidMethod(env, obj, mid, ...)` 是**可变参数**函数：整数/指针走 rcx,rdx,r8,r9
// 然后压栈，浮点走 xmm0..3（位置按第几个参数算，不是按"第几个浮点参数"）。
// Rust 不能定义可变参数函数，所以这里用一段裸汇编（naked）+ Rust 核心函数：
//
//   * 裸汇编只做一件事：把 rcx/rdx/r8/r9 + xmm0..3 + 调用方的栈参数区交出来；
//   * 参数怎么解释（哪个是 int、哪个是 double）由描述符在 Rust 侧决定。
//
// 这是唯一能同时支持 `glfwSetCursorPosCallback`（两个 jdouble）和普通 `(I)V`
// 回调的写法。别的架构上没有这个垫片（见文件末尾的 fallback 说明）。

/// `...` 调用的核心：`spill` = [gpr0..gpr3, xmm0..xmm3]（8 个 u64），
/// `stack_args` = 调用方压栈的第 5、6… 个参数的起点，`out_xmm` 写浮点返回值。
///
/// `op` 编码：bit0..4 = 返回类型，bit8..10 = 模式（0 实例 / 1 静态 / 2 nonvirtual），
/// bit16..18 = 变参起始位置（`NewObject` 是 2，普通 Call 是 3，nonvirtual 是 4）。
pub(crate) unsafe extern "C" fn va_core(
    op: u64,
    spill: *const u64,
    stack_args: *const u64,
    out_xmm: *mut u64,
) -> u64 {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| va_core_inner(op, spill, stack_args, out_xmm)));
    match r {
        Ok(v) => v,
        Err(_) => {
            note_panic();
            0
        }
    }
}

unsafe fn va_core_inner(op: u64, spill: *const u64, stack_args: *const u64, out_xmm: *mut u64) -> u64 {
    let ret = (op & 0x1F) as u32;
    let mode = ((op >> 8) & 0x3) as u32;
    let base = ((op >> 16) & 0x7) as usize;
    let env = *spill;
    let _ = env;
    let arg1 = *spill.add(1);
    let arg2 = *spill.add(2);
    let mid: jmethodID = *spill.add(2) as jmethodID;
    let Some(m) = method_id(mid) else { return 0 };
    let types = split_params(&m.desc);

    // 变参：位置 pos = base + i
    let mut slot_args = Vec::with_capacity(types.len());
    for (i, t) in types.iter().enumerate() {
        let pos = base + i;
        let (fp, raw) = if pos < 4 { (is_fp_type(t), *spill.add(pos)) } else { (is_fp_type(t), *stack_args.add(pos - 4)) };
        // 浮点参数在寄存器里时，值在 xmm(位置) 里 —— 位置 pos<4 的那 4 个槽
        let raw = if pos < 4 && fp { *spill.add(4 + pos) } else { raw };
        slot_args.push(bits_to_slot(raw, t));
    }
    // `NewObject`：先分配对象，再调 <init>，返回值就是新对象
    let (recv, is_new) = if mode == 3 {
        // NewObject(env, clazz, mid, ...)
        let h = with_vm(|vm| {
            let Some(name) = class_of_class_obj(vm, arg1 as jclass) else { return 0 };
            let n = vm.store.field_count(&name).max(1);
            vm.sh.heap.new_object(vec![Slot::I(0); n], &name).unwrap_or(0)
        })
        .unwrap_or(0);
        (h, true)
    } else {
        (arg1 as i32, false)
    };

    let r = call_java(recv, m, slot_args, mode == 2);
    let v = match r {
        Ok(v) => v,
        Err(_) => return 0,
    };
    if is_new {
        // NewObject 的返回值是对象，不是 <init> 的 void
        return recv as u64;
    }
    if ret == 8 || ret == 9 {
        if !out_xmm.is_null() {
            *out_xmm = match ret {
                8 => v.as_f32().to_bits() as u64,
                _ => v.as_f64().to_bits(),
            };
        }
        return 0;
    }
    let _ = arg2;
    match ret {
        1 => v.as_i32() as u64,          // Object
        7 => v.as_i64() as u64,          // Long
        _ => v.as_i32() as u64,          // int/boolean/byte/char/short/void
    }
}

fn is_fp_type(t: &str) -> bool {
    matches!(t.as_bytes().first().copied(), Some(b'F') | Some(b'D'))
}

/// 生成一个裸汇编垫片：把寄存器交出去 → 调 `va_core` → 把浮点返回值放进 xmm0
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
macro_rules! va_shims {
    ($(($name:ident, $op:expr);)*) => {
        $(
            #[unsafe(naked)]
            pub(crate) unsafe extern "C" fn $name() {
                core::arch::naked_asm!(
                    "sub rsp, 0x108",
                    "mov qword ptr [rsp + 0x20], rcx",
                    "mov qword ptr [rsp + 0x28], rdx",
                    "mov qword ptr [rsp + 0x30], r8",
                    "mov qword ptr [rsp + 0x38], r9",
                    "movaps xmmword ptr [rsp + 0x40], xmm0",
                    "movaps xmmword ptr [rsp + 0x50], xmm1",
                    "movaps xmmword ptr [rsp + 0x60], xmm2",
                    "movaps xmmword ptr [rsp + 0x70], xmm3",
                    "mov qword ptr [rsp + 0x80], 0",
                    "mov qword ptr [rsp + 0x88], 0",
                    "mov rcx, {op}",
                    "lea rdx, [rsp + 0x20]",
                    "lea r8, [rsp + 0x130]",
                    "lea r9, [rsp + 0x80]",
                    "call {core}",
                    "movq xmm0, qword ptr [rsp + 0x88]",
                    "add rsp, 0x108",
                    "ret",
                    op = const $op,
                    core = sym va_core,
                )
            }
        )*
    };
}

/// `...` 版 JNI 入口（x86_64 Windows 走裸汇编；其它架构先记一条"不支持"）
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
va_shims! {
    (va_new_object, 0u64 | (3u64 << 8) | (2u64 << 16));
    (va_call_object, 1u64 | (0u64 << 8) | (3u64 << 16));
    (va_call_boolean, 2u64 | (0u64 << 8) | (3u64 << 16));
    (va_call_byte, 3u64 | (0u64 << 8) | (3u64 << 16));
    (va_call_char, 4u64 | (0u64 << 8) | (3u64 << 16));
    (va_call_short, 5u64 | (0u64 << 8) | (3u64 << 16));
    (va_call_int, 6u64 | (0u64 << 8) | (3u64 << 16));
    (va_call_long, 7u64 | (0u64 << 8) | (3u64 << 16));
    (va_call_float, 8u64 | (0u64 << 8) | (3u64 << 16));
    (va_call_double, 9u64 | (0u64 << 8) | (3u64 << 16));
    (va_call_void, 0u64 | (0u64 << 8) | (3u64 << 16));
    (va_call_static_object, 1u64 | (1u64 << 8) | (3u64 << 16));
    (va_call_static_boolean, 2u64 | (1u64 << 8) | (3u64 << 16));
    (va_call_static_byte, 3u64 | (1u64 << 8) | (3u64 << 16));
    (va_call_static_char, 4u64 | (1u64 << 8) | (3u64 << 16));
    (va_call_static_short, 5u64 | (1u64 << 8) | (3u64 << 16));
    (va_call_static_int, 6u64 | (1u64 << 8) | (3u64 << 16));
    (va_call_static_long, 7u64 | (1u64 << 8) | (3u64 << 16));
    (va_call_static_float, 8u64 | (1u64 << 8) | (3u64 << 16));
    (va_call_static_double, 9u64 | (1u64 << 8) | (3u64 << 16));
    (va_call_static_void, 0u64 | (1u64 << 8) | (3u64 << 16));
    (va_call_nonvirtual_object, 1u64 | (2u64 << 8) | (4u64 << 16));
    (va_call_nonvirtual_boolean, 2u64 | (2u64 << 8) | (4u64 << 16));
    (va_call_nonvirtual_byte, 3u64 | (2u64 << 8) | (4u64 << 16));
    (va_call_nonvirtual_char, 4u64 | (2u64 << 8) | (4u64 << 16));
    (va_call_nonvirtual_short, 5u64 | (2u64 << 8) | (4u64 << 16));
    (va_call_nonvirtual_int, 6u64 | (2u64 << 8) | (4u64 << 16));
    (va_call_nonvirtual_long, 7u64 | (2u64 << 8) | (4u64 << 16));
    (va_call_nonvirtual_float, 8u64 | (2u64 << 8) | (4u64 << 16));
    (va_call_nonvirtual_double, 9u64 | (2u64 << 8) | (4u64 << 16));
    (va_call_nonvirtual_void, 0u64 | (2u64 << 8) | (4u64 << 16));
}

/// nonvirtual 形态的 `clazz` 参数：`(env, obj, clazz, mid, ...)`，
/// 但 `va_core` 的 arg1 就是 obj、mid 在 spill[2] —— 所以这里按位置挪一下。
/// （`CallNonvirtualXMethod` 的 `clazz` 只用于选实现，我们的 `MethodId` 里已经带了声明类）
fn _nonvirtual_layout_note() {}

/// `A`（`jvalue[]`）形态的 20 个入口 + 静态 `A`
macro_rules! call_a_ops {
    ($(($ca:ident, $csa:ident, $ty:ty, $conv:expr);)*) => {
        $(
            jni!($ca, (env: *mut JNIEnv, obj: jobject, mid: jmethodID, args: *const u64) => $ty, 0 as $ty, {
                let _ = env;
                let Some(m) = method_id(mid) else { return 0 as $ty };
                let types = split_params(&m.desc);
                let s = jvalues_to_slots(args, &types);
                match call_java(p2h(obj), m, s, false) {
                    Ok(v) => { let f: fn(Slot) -> $ty = $conv; f(v) }
                    Err(_) => 0 as $ty,
                }
            });

            jni!($csa, (env: *mut JNIEnv, clazz: jclass, mid: jmethodID, args: *const u64) => $ty, 0 as $ty, {
                let _ = env;
                let Some(m) = method_id(mid) else { return 0 as $ty };
                let types = split_params(&m.desc);
                let s = jvalues_to_slots(args, &types);
                match call_java(p2h(clazz), m, s, false) {
                    Ok(v) => { let f: fn(Slot) -> $ty = $conv; f(v) }
                    Err(_) => 0 as $ty,
                }
            });
        )*
    };
}

call_a_ops! {
    (CallObjectMethodA, CallStaticObjectMethodA, jobject, |v: Slot| v.as_i32() as jobject);
    (CallBooleanMethodA, CallStaticBooleanMethodA, jboolean, |v: Slot| u8::from(v.as_i32() != 0));
    (CallByteMethodA, CallStaticByteMethodA, jbyte, |v: Slot| v.as_i32() as i8);
    (CallCharMethodA, CallStaticCharMethodA, jchar, |v: Slot| v.as_i32() as u16);
    (CallShortMethodA, CallStaticShortMethodA, jshort, |v: Slot| v.as_i32() as i16);
    (CallIntMethodA, CallStaticIntMethodA, jint, |v: Slot| v.as_i32());
    (CallLongMethodA, CallStaticLongMethodA, jlong, |v: Slot| v.as_i64());
    (CallFloatMethodA, CallStaticFloatMethodA, jfloat, |v: Slot| v.as_f32());
    (CallDoubleMethodA, CallStaticDoubleMethodA, jdouble, |v: Slot| v.as_f64());
    (CallVoidMethodA, CallStaticVoidMethodA, (), |_v: Slot| ());
}

/// `NewObjectA`：分配 + 调 `<init>`，返回值是新对象
jni!(NewObjectA, (env: *mut JNIEnv, clazz: jclass, mid: jmethodID, args: *const u64) => jobject, std::ptr::null_mut(), {
    let _ = env;
    let Some(m) = method_id(mid) else { return std::ptr::null_mut() };
    let Some(name) = with_vm(|vm| class_of_class_obj(vm, clazz)).flatten() else {
        return std::ptr::null_mut();
    };
    let h = with_vm(|vm| {
        let n = vm.store.field_count(&name).max(1);
        vm.sh.heap.new_object(vec![Slot::I(0); n], &name)
    })
    .unwrap_or(None)
    .unwrap_or(0);
    if h == 0 {
        return std::ptr::null_mut();
    }
    let types = split_params(&m.desc);
    let s = jvalues_to_slots(args, &types);
    match call_java(h, m, s, false) {
        Ok(_) => h2p(h),
        Err(_) => std::ptr::null_mut(),
    }
});

/// `CallNonvirtualXMethodA`
macro_rules! call_nonvirt_a_ops {
    ($(($c:ident, $ty:ty, $conv:expr);)*) => {
        $(
            jni!($c, (env: *mut JNIEnv, obj: jobject, clazz: jclass, mid: jmethodID, args: *const u64) => $ty,
                 0 as $ty, {
                let _ = (env, clazz);
                let Some(m) = method_id(mid) else { return 0 as $ty };
                let types = split_params(&m.desc);
                let s = jvalues_to_slots(args, &types);
                match call_java(p2h(obj), m, s, true) {
                    Ok(v) => { let f: fn(Slot) -> $ty = $conv; f(v) }
                    Err(_) => 0 as $ty,
                }
            });
        )*
    };
}

call_nonvirt_a_ops! {
    (CallNonvirtualObjectMethodA, jobject, |v: Slot| v.as_i32() as jobject);
    (CallNonvirtualBooleanMethodA, jboolean, |v: Slot| u8::from(v.as_i32() != 0));
    (CallNonvirtualByteMethodA, jbyte, |v: Slot| v.as_i32() as i8);
    (CallNonvirtualCharMethodA, jchar, |v: Slot| v.as_i32() as u16);
    (CallNonvirtualShortMethodA, jshort, |v: Slot| v.as_i32() as i16);
    (CallNonvirtualIntMethodA, jint, |v: Slot| v.as_i32());
    (CallNonvirtualLongMethodA, jlong, |v: Slot| v.as_i64());
    (CallNonvirtualFloatMethodA, jfloat, |v: Slot| v.as_f32());
    (CallNonvirtualDoubleMethodA, jdouble, |v: Slot| v.as_f64());
    (CallNonvirtualVoidMethodA, (), |_v: Slot| ());
}

/// `V`（`va_list`）形态：Win64 上 `va_list` 就是"指向参数区的指针"，语义和 `A`
/// 并不相同，硬猜会读到垃圾。这里明确不支持：抛 `UnsupportedOperationException`
/// 而不是静默返回 0（静默返回 0 会让 native 库拿到假数据，比报错难查得多）。
unsafe extern "C" fn va_list_unsupported() -> u64 {
    let h = with_vm(|vm| throw_new(vm, "java/lang/UnsupportedOperationException", "JNI va_list 形态（CallXMethodV）未实现"))
        .unwrap_or(0);
    set_pending(h);
    0
}
