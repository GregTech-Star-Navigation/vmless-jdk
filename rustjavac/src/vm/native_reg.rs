//! native 方法注册表：`(类内部名, 方法名, 描述符) → Rust 实现`。
//!
//! 这是「真 java.base 字节码」能跑起来的前提：OpenJDK 里有一批方法是
//! `native` 的（`Object.hashCode`、`System.arraycopy`、`Unsafe.*` …），
//! 它们**本身没有 Code 属性**，只能由 VM 提供实现。
//!
//! 与 `jdk.rs` / `natives.rs` 的分工（三者不要混）：
//!
//! | 位置 | 是什么 | 谁用 |
//! | --- | --- | --- |
//! | `jdk.rs::VIRTUALS/STATICS` | **内建替身**：没有 `.class` 时顶替整个 java.* 类 | 编译器取签名、JIT 的 op 号、无 java.base 模式的解释器 |
//! | `natives.rs::builtin_*` | 上面那张表的实现（op 号分发） | 同上 |
//! | 本文件 | **真 native 方法**的实现（按 `(类,名,描述符)` 精确匹配） | `exec` 遇到 `ACC_NATIVE` 时 |
//!
//! 判据一句话：**这个方法在 OpenJDK 源码里带 `native` 关键字吗**？带 → 进本表；
//! 不带 → 它有字节码，就该由解释器执行，不许在这里写一份。

use super::frame::Frame;
use super::heap::RefKind;
use super::slot::Slot;
use super::{Vm, VmError};
use std::collections::HashMap;

/// native 方法的 Rust 实现。参数在 `frame.locals` 里按 JVM 槽位落位
/// （实例方法 `this` 在槽 0），返回值由 `Result<Slot, VmError>` 给出（`void` 返回 `Slot::I(0)`）。
pub(crate) type NativeFn = fn(&mut Vm, &mut Frame) -> Result<Slot, VmError>;

pub(crate) struct NativeRegistry {
    map: HashMap<(&'static str, &'static str, &'static str), NativeFn>,
}

impl NativeRegistry {
    pub(super) fn new() -> Self {
        let mut r = NativeRegistry { map: HashMap::new() };
        register_all(&mut r);
        r
    }

    fn add(&mut self, class: &'static str, name: &'static str, desc: &'static str, f: NativeFn) {
        self.map.insert((class, name, desc), f);
    }

    pub(crate) fn lookup(&self, class: &str, name: &str, desc: &str) -> Option<NativeFn> {
        self.map
            .get(&(class.to_string().as_str(), name, desc))
            .copied()
            .or_else(|| self.map.get(&("", name, desc)).copied())
    }

    /// 已注册的全部条目（按类名排序）—— 报告/`docs` 用
    pub(crate) fn entries(&self) -> Vec<(&'static str, &'static str, &'static str)> {
        let mut v: Vec<_> = self.map.keys().copied().collect();
        v.sort();
        v
    }
}

// ---------------------------------------------------------------- 参数读取

/// 第 `i` 个参数所在的槽位（`i` 从 0 开始，含 `this`）
pub(super) fn arg_slot(desc: &str, i: usize) -> usize {
    let slots = crate::jit::param_slots(desc);
    slots.get(i).copied().unwrap_or(i)
}

/// 把 `args` 按 JVM 槽位落进一帧（解释器与 native 共用，规则只能有一份）
pub(super) fn place_args(frame: &mut Frame, desc: &str, args: &[Slot], receiver: bool) {
    let slots = crate::jit::param_slots(desc);
    let off = usize::from(receiver);
    frame.arg_base = off;
    frame.arg_slots = slots.clone();
    for (i, a) in args.iter().enumerate() {
        let slot = if i < off { 0 } else { slots.get(i - off).copied().unwrap_or(i - off) + off };
        frame.store(slot, *a);
    }
}

fn si(f: &Frame, i: usize) -> i32 {
    f.load(i).as_i32()
}
fn sl(f: &Frame, i: usize) -> i64 {
    f.load(i).as_i64()
}
fn sf(f: &Frame, i: usize) -> f32 {
    f.load(i).as_f32()
}
fn sd(f: &Frame, i: usize) -> f64 {
    f.load(i).as_f64()
}
fn void() -> Result<Slot, VmError> {
    Ok(Slot::I(0))
}

// ---------------------------------------------------------------- 注册

fn register_all(r: &mut NativeRegistry) {

    // ---- java.lang.Object ----
    // `public final native Class<?> getClass()` / `public native int hashCode()` / `clone()`
    r.add("java/lang/Object", "getClass", "()Ljava/lang/Class;", obj_get_class);
    r.add("java/lang/Object", "hashCode", "()I", obj_hash_code);
    r.add("java/lang/Object", "clone", "()Ljava/lang/Object;", obj_clone);
    r.add("java/lang/Object", "notify", "()V", no_op);
    r.add("java/lang/Object", "notifyAll", "()V", no_op);
    r.add("java/lang/Object", "wait", "(J)V", no_op);
    r.add("java/lang/Object", "wait", "(JI)V", no_op);

    // ---- java.lang.Class ----
    // `<clinit>` 第一句就是 `registerNatives()`，不接住它连类都初始化不了
    r.add("java/lang/Class", "registerNatives", "()V", no_op);
    r.add("java/lang/Class", "initClassName", "()Ljava/lang/String;", cls_init_name);
    r.add(
        "java/lang/Class",
        "getPrimitiveClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        cls_primitive,
    );
    r.add("java/lang/Class", "isInstance", "(Ljava/lang/Object;)Z", cls_is_instance);
    r.add("java/lang/Class", "isAssignableFrom", "(Ljava/lang/Class;)Z", cls_is_assignable);
    r.add("java/lang/Class", "getSuperclass", "()Ljava/lang/Class;", cls_super);
    r.add("java/lang/Class", "isHidden", "()Z", |_, _| Ok(Slot::I(0)));
    // `assert` 语句的 `$assertionsDisabled = !X.class.desiredAssertionStatus()`：
    // 与 `java` 不带 `-ea` 启动一致 —— 断言关闭
    r.add("java/lang/Class", "desiredAssertionStatus0", "(Ljava/lang/Class;)Z", |_, _| Ok(Slot::I(0)));


    // ---- java.lang.System ----
    r.add("java/lang/System", "registerNatives", "()V", no_op);
    r.add("java/lang/System", "setIn0", "(Ljava/io/InputStream;)V", no_op);
    r.add("java/lang/System", "setOut0", "(Ljava/io/PrintStream;)V", no_op);
    r.add("java/lang/System", "setErr0", "(Ljava/io/PrintStream;)V", no_op);
    r.add("java/lang/System", "currentTimeMillis", "()J", sys_millis);
    r.add("java/lang/System", "nanoTime", "()J", sys_nanos);
    r.add(
        "java/lang/System",
        "arraycopy",
        "(Ljava/lang/Object;ILjava/lang/Object;II)V",
        sys_arraycopy,
    );
    r.add("java/lang/System", "identityHashCode", "(Ljava/lang/Object;)I", sys_identity_hash);

    // ---- java.lang.String ----
    r.add("java/lang/String", "intern", "()Ljava/lang/String;", |_, f| Ok(Slot::I(f.this())));

    // ---- java.lang.Throwable ----
    // 我们没有调用栈模型：`fillInStackTrace` 返回 this 即可（栈轨迹永远是空数组）
    r.add(
        "java/lang/Throwable",
        "fillInStackTrace",
        "(I)Ljava/lang/Throwable;",
        |_, f| Ok(Slot::I(f.this())),
    );
    r.add("java/lang/Throwable", "getStackTraceDepth", "()I", |_, _| Ok(Slot::I(0)));

    // ---- java.lang.Double / Float 的位级转换（真 java.base 的 equals/hashCode 全靠它们）----
    r.add("java/lang/Double", "doubleToRawLongBits", "(D)J", |_, f| Ok(Slot::L(f.arg_d(0).to_bits() as i64)));
    r.add("java/lang/Double", "longBitsToDouble", "(J)D", |_, f| Ok(Slot::F(f64::from_bits(f.arg_l(0) as u64))));
    r.add("java/lang/Float", "floatToRawIntBits", "(F)I", |_, f| Ok(Slot::I(f.arg_f(0).to_bits() as i32)));
    r.add("java/lang/Float", "intBitsToFloat", "(I)F", |_, f| Ok(Slot::Fl(f32::from_bits(f.arg_i(0) as u32))));

    // ---- java.util.concurrent.atomic.AtomicLong 的 CAS 底层（HashMap 扩容计数用不到，别的类会碰）----

    // ---- jdk.internal.misc.VM ----
    // `VM.<clinit>` 第一句是 `initialize()`。HotSpot 里它建好 `savedProps`，
    // 而 `VM.getSavedProperty` 在它为 null 时**直接抛** IllegalStateException
    // ("Not yet initialized")：`Integer.IntegerCache.<clinit>` 就读这个属性，
    // 于是 `Integer.valueOf(1)` 一调用就炸。所以这里必须真给一个空 Map。
    r.add("jdk/internal/misc/VM", "initialize", "()V", vm_initialize);
    r.add("jdk/internal/misc/VM", "getNanoTimeAdjustment", "(J)J", |_, f| Ok(Slot::L(f.arg_l(0))));
    // CDS（类数据共享）：`String`/`Integer` 之类的 `<clinit>` 会问一句「有没有 CDS 镜像」。
    // 我们从来不使用 CDS 归档 → 0（= 没有）。`isDumpingClassList0` 之类同理返回 false。
    r.add("jdk/internal/misc/CDS", "getCDSConfigStatus", "()I", |_, _| Ok(Slot::I(0)));
    r.add("jdk/internal/misc/CDS", "isDumpingClassList0", "()Z", |_, _| Ok(Slot::I(0)));
    r.add("jdk/internal/misc/CDS", "isDumpingArchive0", "()Z", |_, _| Ok(Slot::I(0)));
    r.add("jdk/internal/misc/CDS", "isSharingEnabled0", "()Z", |_, _| Ok(Slot::I(0)));
    r.add("jdk/internal/misc/CDS", "getCorchestra", "()Ljava/lang/Object;", |_, _| Ok(Slot::I(0)));
    r.add("jdk/internal/misc/CDS", "initializeFromArchive", "(Ljava/lang/Class;)V", no_op);

    // ---- jdk.internal.misc.Unsafe ----
    register_unsafe(r);
}

/// `jdk/internal/misc/Unsafe`：java.base 的 `<clinit>` 与 `StringUTF16`/`Class` 大量依赖它。
///
/// 注意名字：**真正带 `native` 关键字的是那些 `*0` 后缀的私有方法**
/// （`arrayBaseOffset0` / `objectFieldOffset0` / `staticFieldOffset0` …），
/// `arrayBaseOffset(Class)` 之类是 Java 包装。注册错名字 = 白注册。
///
/// 我们的堆是 `Vec<Slot>`，没有「裸地址」概念，所以偏移量一律当**元素下标**用。
fn register_unsafe(r: &mut NativeRegistry) {
    let u = "jdk/internal/misc/Unsafe";
    r.add(u, "registerNatives", "()V", no_op);
    // 字段/数组布局：`objectFieldOffset0` 要真偏移（`String.value`/`coder` 靠它）；
    // 我们的「偏移」= 字段槽下标，只能按名字从类的布局里查
    r.add(u, "objectFieldOffset0", "(Ljava/lang/reflect/Field;)J", unsafe_field_offset);
    r.add(u, "staticFieldOffset0", "(Ljava/lang/reflect/Field;)J", |_, _| Ok(Slot::L(0)));
    r.add(u, "staticFieldBase0", "(Ljava/lang/reflect/Field;)Ljava/lang/Object;", |_, _| Ok(Slot::I(0)));
    r.add(u, "shouldBeInitialized0", "(Ljava/lang/Class;)Z", |_, _| Ok(Slot::I(1)));
    r.add(u, "ensureClassInitialized0", "(Ljava/lang/Class;)V", unsafe_ensure_init);
    // 数组元素基址/步长：我们的数组没有对象头，base = 0；**步长必须是真值**（见 `elem_width`）
    r.add(u, "arrayBaseOffset0", "(Ljava/lang/Class;)I", |_, _| Ok(Slot::I(0)));
    r.add(u, "arrayIndexScale0", "(Ljava/lang/Class;)I", unsafe_array_scale);
    r.add(u, "addressSize", "()I", |_, _| Ok(Slot::I(8)));

    // 读（宽度决定打包几个字节：`[B` 的机器字比较靠它）
    r.add(u, "getBoolean", "(Ljava/lang/Object;J)Z", unsafe_get8);
    r.add(u, "getByte", "(Ljava/lang/Object;J)B", unsafe_get8);
    r.add(u, "getChar", "(Ljava/lang/Object;J)C", unsafe_get16);
    r.add(u, "getShort", "(Ljava/lang/Object;J)S", unsafe_get16);
    r.add(u, "getInt", "(Ljava/lang/Object;J)I", unsafe_get32);
    r.add(u, "getLong", "(Ljava/lang/Object;J)J", unsafe_get64);
    r.add(u, "getFloat", "(Ljava/lang/Object;J)F", unsafe_get_float);
    r.add(u, "getDouble", "(Ljava/lang/Object;J)D", unsafe_get_double);
    r.add(u, "getReference", "(Ljava/lang/Object;J)Ljava/lang/Object;", unsafe_get);
    // 写
    r.add(u, "putBoolean", "(Ljava/lang/Object;JZ)V", unsafe_put8);
    r.add(u, "putByte", "(Ljava/lang/Object;JB)V", unsafe_put8);
    r.add(u, "putChar", "(Ljava/lang/Object;JC)V", unsafe_put16);
    r.add(u, "putShort", "(Ljava/lang/Object;JS)V", unsafe_put16);
    r.add(u, "putInt", "(Ljava/lang/Object;JI)V", unsafe_put32);
    r.add(u, "putLong", "(Ljava/lang/Object;JJ)V", unsafe_put64);
    r.add(u, "putFloat", "(Ljava/lang/Object;JF)V", unsafe_put_float);
    r.add(u, "putDouble", "(Ljava/lang/Object;JD)V", unsafe_put_double);
    r.add(u, "putReference", "(Ljava/lang/Object;JLjava/lang/Object;)V", unsafe_put);
    // volatile / ordered 变体：单线程下与普通读写同义
    r.add(u, "getBooleanVolatile", "(Ljava/lang/Object;J)Z", unsafe_get8);
    r.add(u, "getByteVolatile", "(Ljava/lang/Object;J)B", unsafe_get8);
    r.add(u, "getCharVolatile", "(Ljava/lang/Object;J)C", unsafe_get16);
    r.add(u, "getShortVolatile", "(Ljava/lang/Object;J)S", unsafe_get16);
    r.add(u, "getIntVolatile", "(Ljava/lang/Object;J)I", unsafe_get32);
    r.add(u, "getLongVolatile", "(Ljava/lang/Object;J)J", unsafe_get64);
    r.add(u, "getFloatVolatile", "(Ljava/lang/Object;J)F", unsafe_get_float);
    r.add(u, "getDoubleVolatile", "(Ljava/lang/Object;J)D", unsafe_get_double);
    r.add(u, "getReferenceVolatile", "(Ljava/lang/Object;J)Ljava/lang/Object;", unsafe_get);
    r.add(u, "putBooleanVolatile", "(Ljava/lang/Object;JZ)V", unsafe_put8);
    r.add(u, "putByteVolatile", "(Ljava/lang/Object;JB)V", unsafe_put8);
    r.add(u, "putCharVolatile", "(Ljava/lang/Object;JC)V", unsafe_put16);
    r.add(u, "putShortVolatile", "(Ljava/lang/Object;JS)V", unsafe_put16);
    r.add(u, "putIntVolatile", "(Ljava/lang/Object;JI)V", unsafe_put32);
    r.add(u, "putLongVolatile", "(Ljava/lang/Object;JJ)V", unsafe_put64);
    r.add(u, "putFloatVolatile", "(Ljava/lang/Object;JF)V", unsafe_put_float);
    r.add(u, "putDoubleVolatile", "(Ljava/lang/Object;JD)V", unsafe_put_double);
    r.add(u, "putReferenceVolatile", "(Ljava/lang/Object;JLjava/lang/Object;)V", unsafe_put);
    r.add(u, "putIntRelease", "(Ljava/lang/Object;JI)V", unsafe_put32);
    r.add(u, "putReferenceRelease", "(Ljava/lang/Object;JLjava/lang/Object;)V", unsafe_put);
    r.add(u, "putIntOrdered", "(Ljava/lang/Object;JI)V", unsafe_put32);
    r.add(u, "putIntOrdered", "(Ljava/lang/Object;JI)V", unsafe_put);
    // CAS：单线程下就是「比较 + 写」
    r.add(u, "compareAndSetInt", "(Ljava/lang/Object;JII)Z", unsafe_cas);
    r.add(u, "compareAndSetLong", "(Ljava/lang/Object;JJJ)Z", unsafe_cas);
    r.add(u, "compareAndSetReference", "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z", unsafe_cas);
    r.add(u, "compareAndExchangeInt", "(Ljava/lang/Object;JII)I", unsafe_cas_exchange);
    r.add(u, "getAndAddInt", "(Ljava/lang/Object;JI)I", unsafe_get_and_add);
    r.add(u, "getAndAddLong", "(Ljava/lang/Object;JJ)J", unsafe_get_and_add);
    // 对象/类
    r.add(u, "allocateInstance", "(Ljava/lang/Class;)Ljava/lang/Object;", unsafe_allocate);
    r.add(u, "throwException", "(Ljava/lang/Throwable;)V", unsafe_throw);
    // 线程/屏障：这个 VM 是单线程的，全是空操作
    r.add(u, "park", "(ZJ)V", no_op);
    r.add(u, "unpark", "(Ljava/lang/Object;)V", no_op);
    r.add(u, "fullFence", "()V", no_op);
    r.add(u, "acquireFence", "()V", no_op);
    r.add(u, "releaseFence", "()V", no_op);
    r.add(u, "loadFence", "()V", no_op);
    r.add(u, "storeFence", "()V", no_op);
}
// ---------------------------------------------------------------- 实现

fn no_op(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    void()
}

/// `jdk.internal.misc.VM.initialize()`：建好 `savedProps`（空 Map）。
/// 不建的话 `VM.getSavedProperty` 抛 IllegalStateException("Not yet initialized")，
/// `Integer.IntegerCache.<clinit>` 就挂在里面。
fn vm_initialize(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    let map = vm.new_instance("java/util/HashMap", "()V")?;
    vm.statics.insert(("jdk/internal/misc/VM".to_string(), "savedProps".to_string()), Slot::I(map));
    void()
}

fn obj_hash_code(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    Ok(Slot::I(vm.identity_hash(this)))
}

fn obj_get_class(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let desc = vm.class_of(this).unwrap_or_else(|| "java/lang/Object".to_string());
    Ok(Slot::I(vm.class_obj(&desc)))
}

/// 浅拷贝：对象整块复制字段槽；数组复制元素；字符串直接返回自己（不可变）
fn obj_clone(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    match vm.ref_kind(this) {
        RefKind::Null => Err(vm.throw("java/lang/NullPointerException")),
        RefKind::Str => Ok(Slot::I(this)),
        RefKind::Obj(_) => {
            let slots = vm.objects.get(this as usize - 1).cloned().unwrap_or_default();
            let cls = vm.object_class.get(this as usize - 1).cloned().unwrap_or_default();
            vm.objects.push(slots);
            vm.object_class.push(cls);
            Ok(Slot::I(vm.objects.len() as i32))
        }
        RefKind::Arr(_) => {
            let slots = vm.arrays.get((-this - 1) as usize).cloned().unwrap_or_default();
            let cls = vm.array_class.get((-this - 1) as usize).cloned().unwrap_or_default();
            vm.arrays.push(slots);
            vm.array_class.push(cls);
            Ok(Slot::I(-(vm.arrays.len() as i32)))
        }
    }
}

fn cls_init_name(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    // `Class.getName()` 是 Java 代码，它把这里返回的东西当**二进制名**（点号）
    let this = f.this();
    let name = vm.class_obj_name(this).unwrap_or_default();
    let text = binary_name(&name);
    Ok(Slot::I(vm.str_new(text)))
}

/// 内部名 → `Class.getName()` 的二进制名：`java/lang/String` → `java.lang.String`，
/// 数组原样保留形状只换分隔符：`[Ljava/lang/String;` → `[Ljava.lang.String;`
fn binary_name(internal: &str) -> String {
    internal.replace('/', ".")
}

fn cls_primitive(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    // `getPrimitiveClass("int")` → 代表基本类型的类对象，用描述符记
    let text = vm.text_of(f.arg_i(0));
    let desc = match text.as_str() {
        "int" => "I",
        "long" => "J",
        "double" => "D",
        "float" => "F",
        "boolean" => "Z",
        "char" => "C",
        "byte" => "B",
        "short" => "S",
        "void" => "V",
        other => other,
    };
    Ok(Slot::I(vm.class_obj(desc)))
}

fn cls_is_instance(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let target = vm.class_obj_name(f.this()).unwrap_or_default();
    let v = f.arg_i(0);
    Ok(Slot::I(if v != 0 && vm.is_instance(v, &target) { 1 } else { 0 }))
}

fn cls_is_assignable(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let me = vm.class_obj_name(f.this()).unwrap_or_default();
    let other = match vm.class_obj_name(f.arg_i(0)) {
        Some(o) => o,
        None => return Ok(Slot::I(0)),
    };
    Ok(Slot::I(if vm.is_subclass(&other, &me) { 1 } else { 0 }))
}

fn cls_super(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let me = vm.class_obj_name(f.this()).unwrap_or_default();
    match vm.store.super_of(&me) {
        Some(s) => Ok(Slot::I(vm.class_obj(&s))),
        None => Ok(Slot::I(0)),
    }
}

fn sys_millis(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::L(now_millis()))
}

fn sys_nanos(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::L(now_nanos()))
}

fn sys_arraycopy(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (src, sp, dst, dp, n) = (f.arg_i(0), f.arg_i(1), f.arg_i(2), f.arg_i(3), f.arg_i(4));
    vm.array_copy(src, sp, dst, dp, n)?;
    void()
}

fn sys_identity_hash(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.arg_i(0);
    Ok(Slot::I(if h == 0 { 0 } else { vm.identity_hash(h) }))
}

/// 数组元素的**字节宽度**（与 HotSpot 的 `ARRAY_*_INDEX_SCALE` 一致）。
///
/// 必须返回真值，不能一律返回 1：`ArraysSupport.vectorizedMismatch` 用
/// `log2(ARRAY_LONG_INDEX_SCALE) - log2(ARRAY_xxx_INDEX_SCALE)` 当移位量，
/// 全返回 1 会让它按 1 字节步长比较 8 字节窗口 —— `String.startsWith` 会在
/// 「前缀确实相同」时误报不同（实测）。
fn elem_width(desc: &str) -> usize {
    match desc {
        "[B" | "[Z" => 1,
        "[C" | "[S" => 2,
        "[I" | "[F" => 4,
        "[J" | "[D" => 8,
        _ => 8,
    }
}

/// `Unsafe` 的 `offset`（字节偏移）→ 我们数组堆里的**元素下标**
fn slot_index(vm: &Vm, obj: i32, off: i64) -> Option<usize> {
    let off = usize::try_from(off).ok()?;
    match vm.ref_kind(obj) {
        RefKind::Arr(ref d) => {
            let w = elem_width(d);
            Some(off / w)
        }
        _ => Some(off),
    }
}

/// `[B` 数组在内存里是**连续字节**，而 `Unsafe.getLong/getInt` 按 HotSpot 的语义要
/// 一次读 8 / 4 个字节并打包（小端）。`ArraysSupport.vectorizedMismatch` 正是靠它
/// 按机器字比较。
fn read_mem(vm: &Vm, obj: i32, i: usize, width: usize) -> u64 {
    let is_bytes = matches!(vm.ref_kind(obj), RefKind::Arr(ref d) if d == "[B");
    if is_bytes && width > 1 {
        let mut out = 0u64;
        if let Some(a) = vm.arrays.get((-obj - 1) as usize) {
            for k in 0..width {
                out |= ((a.get(i + k).map(|s| s.as_i32() as u8).unwrap_or(0) as u64) & 0xFF) << (8 * k);
            }
        }
        return out;
    }
    let s = unsafe_slot(vm, obj, i as i64).unwrap_or(Slot::I(0));
    let raw = match width {
        8 => s.as_i64() as u64,
        _ => s.as_i32() as u32 as u64,
    };
    let mask = if width >= 8 { u64::MAX } else { (1u64 << (8 * width)) - 1 };
    raw & mask
}

fn write_mem(vm: &mut Vm, obj: i32, i: usize, width: usize, v: u64) {
    let is_bytes = matches!(vm.ref_kind(obj), RefKind::Arr(ref d) if d == "[B");
    if is_bytes && width > 1 {
        if let Some(a) = vm.arrays.get_mut((-obj - 1) as usize) {
            for k in 0..width {
                if i + k < a.len() {
                    a[i + k] = Slot::I(((v >> (8 * k)) & 0xFF) as i32);
                }
            }
        }
        return;
    }
    let slot = match width {
        8 => Slot::L(v as i64),
        _ => Slot::I(v as u32 as i32),
    };
    unsafe_put_impl(vm, obj, i as i64, slot);
}

/// `Unsafe.get*` 的 `(Object o, long offset)`：`offset` 在我们的模型里就是**槽下标**
fn unsafe_slot(vm: &Vm, obj: i32, off: i64) -> Option<Slot> {
    let i = slot_index(vm, obj, off)?;
    match vm.ref_kind(obj) {
        RefKind::Arr(_) => vm.arrays.get((-obj - 1) as usize)?.get(i).copied(),
        // 字符串常量的字段视图（材料化过才有；没材料化就当成 0，调用方自己会重新取）
        RefKind::Str => vm.str_fields.get(&obj)?.get(i).copied(),
        RefKind::Obj(_) => vm.objects.get(obj as usize - 1)?.get(i).copied(),
        _ => None,
    }
}

fn unsafe_put_impl(vm: &mut Vm, obj: i32, off: i64, v: Slot) {
    let Some(i) = slot_index(vm, obj, off) else { return };
    match vm.ref_kind(obj) {
        RefKind::Arr(_) => {
            if let Some(a) = vm.arrays.get_mut((-obj - 1) as usize) {
                if i < a.len() {
                    a[i] = v;
                }
            }
        }
        RefKind::Str => {
            let slots = vm.str_field_slots(obj);
            if i < slots.len() {
                slots[i] = v;
            }
        }
        RefKind::Obj(_) => {
            if let Some(slots) = vm.objects.get_mut(obj as usize - 1) {
                if i < slots.len() {
                    slots[i] = v;
                }
            }
        }
        _ => {}
    }
}

/// `(o, offset)` —— 全部 `Unsafe` 访问器的前两个参数
fn target(f: &Frame) -> (i32, usize) {
    let off = f.arg_l(1);
    (f.arg_i(0), off.max(0) as usize)
}

fn unsafe_get8(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, i) = target(f);
    Ok(Slot::I((read_mem(vm, obj, i, 1) as u8) as i8 as i32))
}

fn unsafe_get16(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, i) = target(f);
    Ok(Slot::I((read_mem(vm, obj, i, 2) as u16) as i16 as i32))
}

fn unsafe_get32(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, i) = target(f);
    Ok(Slot::I(read_mem(vm, obj, i, 4) as u32 as i32))
}

fn unsafe_get64(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, i) = target(f);
    Ok(Slot::L(read_mem(vm, obj, i, 8) as i64))
}

fn unsafe_get_float(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, i) = target(f);
    Ok(Slot::Fl(f32::from_bits(read_mem(vm, obj, i, 4) as u32)))
}

fn unsafe_get_double(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, i) = target(f);
    Ok(Slot::F(f64::from_bits(read_mem(vm, obj, i, 8))))
}

/// `getReference` 不打包：直接返回槽里的句柄
fn unsafe_get(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let off = f.arg_l(1);
    Ok(unsafe_slot(vm, f.arg_i(0), off).unwrap_or(Slot::I(0)))
}

fn unsafe_put8(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, i) = target(f);
    write_mem(vm, obj, i, 1, f.arg_i(2) as u32 as u64);
    void()
}

fn unsafe_put16(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, i) = target(f);
    write_mem(vm, obj, i, 2, f.arg_i(2) as u32 as u64);
    void()
}

fn unsafe_put32(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, i) = target(f);
    write_mem(vm, obj, i, 4, f.arg_i(2) as u32 as u64);
    void()
}

fn unsafe_put64(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, i) = target(f);
    write_mem(vm, obj, i, 8, f.arg_l(2) as u64);
    void()
}

fn unsafe_put_float(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, i) = target(f);
    write_mem(vm, obj, i, 4, f.arg_f(2).to_bits() as u64);
    void()
}

fn unsafe_put_double(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, i) = target(f);
    write_mem(vm, obj, i, 8, f.arg_d(2).to_bits());
    void()
}

/// `putReference` 一族：写引用，最后一个参数是一个槽的句柄
fn unsafe_put(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, off, v) = (f.arg_i(0), f.arg_l(1), Slot::I(f.arg_i(2)));
    unsafe_put_impl(vm, obj, off, v);
    void()
}


/// `compareAndSet{Int,Long,Reference}(Object o, long off, <expected>, <x>)` —— 单线程下就是比较再写
fn unsafe_cas(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, off) = (f.arg_i(0), f.arg_l(1));
    let cur = unsafe_slot(vm, obj, off).unwrap_or(Slot::I(0));
    if cur != f.arg(2) {
        return Ok(Slot::I(0));
    }
    unsafe_put_impl(vm, obj, off, f.arg(3));
    Ok(Slot::I(1))
}

/// `compareAndExchange*(Object o, long off, <expected>, <x>)` → 返回**旧值**
fn unsafe_cas_exchange(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, off) = (f.arg_i(0), f.arg_l(1));
    let cur = unsafe_slot(vm, obj, off).unwrap_or(Slot::I(0));
    if cur == f.arg(2) {
        unsafe_put_impl(vm, obj, off, f.arg(3));
    }
    Ok(cur)
}

/// `getAndAdd{Int,Long}` → 返回旧值
fn unsafe_get_and_add(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, off, delta) = (f.arg_i(0), f.arg_l(1), f.arg(2));
    let cur = unsafe_slot(vm, obj, off).unwrap_or(Slot::I(0));
    let next = match (cur, delta) {
        (Slot::L(a), _) => Slot::L(a.wrapping_add(delta.as_i64())),
        _ => Slot::I(cur.as_i32().wrapping_add(delta.as_i32())),
    };
    unsafe_put_impl(vm, obj, off, next);
    Ok(cur)
}

/// `objectFieldOffset0(Field)`：`java.lang.reflect.Field` 里有 `clazz` + `name`
/// `arrayIndexScale0(Class)` → 元素字节宽度。**必须返回真值**：`ArraysSupport` 用它算
/// `LOG2_ARRAY_*_INDEX_SCALE`（移位量），一律返回 1 会让机器字比较按 1 字节步长跑 8 字节
/// 窗口，`String.startsWith` 就会在「前缀确实相同」时误报不同（实测踩过）。
fn unsafe_array_scale(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let desc = vm.class_obj_name(f.arg_i(0)).unwrap_or_default();
    Ok(Slot::I(elem_width(&desc) as i32))
}

fn unsafe_field_offset(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (cls, name) = vm.reflect_field_parts(f.arg_i(0));
    match (cls, name) {
        (Some(c), Some(n)) => Ok(Slot::L(vm.store.field_index(&c, &n).map(|i| i as i64).unwrap_or(-1))),
        _ => Ok(Slot::L(-1)),
    }
}

fn unsafe_ensure_init(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let cls = vm.class_obj_name(f.arg_i(0)).unwrap_or_default();
    vm.ensure_init(&cls)?;
    void()
}

fn unsafe_throw(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.arg_i(0);
    Err(VmError { msg: format!("Unsafe.throwException({})", vm.type_name(h)), exc: Some(h) })
}

fn unsafe_allocate(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let cls = vm.class_obj_name(f.arg_i(0)).unwrap_or_default();
    let n = vm.store.field_count(&cls);
    vm.objects.push(vec![Slot::I(0); n.max(1)]);
    vm.object_class.push(cls);
    Ok(Slot::I(vm.objects.len() as i32))
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn now_nanos() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}
