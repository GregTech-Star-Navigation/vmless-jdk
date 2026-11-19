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

use super::shared::lk;
use super::frame::Frame;
use super::heap::{RefKind, STR_BASE};
use super::slot::Slot;
use super::{Vm, VmError};
use std::collections::HashMap;

// JNI 的两个子模块挂在这里（`#[path]`），不是挂在 `vm/mod.rs` 上：
// `mod.rs` 是几条并行工作线都会碰的文件，少改一行就少一次冲突。
// 路径相对本文件所在目录（`src/vm/`），所以这里写的还是 `jni.rs` / `jni_lib.rs`。
#[path = "jni.rs"]
pub(crate) mod jni;
#[path = "jni_lib.rs"]
pub(crate) mod jni_lib;

// native 实现的分组文件（按「java.base native 的领域」切）：
// 每条并行工作线只碰自己那一份，`native_reg.rs` 只在 `register_all` 末尾各调一次。
#[path = "nat_intrinsics.rs"]
pub(crate) mod nat_intrinsics;
#[path = "nat_os.rs"]
pub(crate) mod nat_os;
#[path = "nat_util.rs"]
pub(crate) mod nat_util;
#[path = "nat_nio.rs"]
pub(crate) mod nat_nio;
#[path = "nat_misc.rs"]
pub(crate) mod nat_misc;
#[path = "nat_desktop.rs"]
pub(crate) mod nat_desktop;

/// native 方法的 Rust 实现。参数在 `frame.locals` 里按 JVM 槽位落位
/// （实例方法 `this` 在槽 0），返回值由 `Result<Slot, VmError>` 给出（`void` 返回 `Slot::I(0)`）。
pub(crate) type NativeFn = fn(&mut Vm, &mut Frame) -> Result<Slot, VmError>;

pub(crate) struct NativeRegistry {
    map: HashMap<(&'static str, &'static str, &'static str), NativeFn>,
    /// **VM 接管的 JDK 方法**：这些方法在 OpenJDK 里不是 `native`（有字节码），
    /// 但它们的字节码要实现的是 VM 内部语义（线程/监视器/内存序），
    /// 只能由 VM 提供 —— 与 HotSpot 里「VM 内建 + 内联」的位置等价。
    /// 查表的时机在**执行字节码之前**（见 `exec::execute_body`）。
    overrides: HashMap<(&'static str, &'static str, &'static str), NativeFn>,
}

impl NativeRegistry {
    pub(super) fn new() -> Self {
        let mut r = NativeRegistry { map: HashMap::new(), overrides: HashMap::new() };
        register_all(&mut r);
        super::reflect::register(&mut r);
        // 帧栈/栈回溯这条线自己的 native（Throwable / Thread / StackWalker）。
        // 放最后：注册表是 insert，后登记的覆盖先登记的 —— 顺便把上面几条「一条假帧」
        // 的老实现摘掉（见 stacktrace::register）。
        super::stacktrace::register(&mut r);
        // `Class.newInstance()` / `ReflectAccess.newInstance(...)` 那条**内部反射**路径
        // 不走 `Constructor.newInstance`（reflect.rs 接管的是后者），它在
        // `Constructor.newInstanceWithCaller` → `DirectConstructorHandleAccessor$NativeAccessor`
        // → 这个 native 上。`nat_misc` 里把它登记成「死代码」是个假设，实际会被
        // `System.initPhase1` → `Charset.isSupported` → `Class.newInstance` 走到
        // （`StandardCharsets` 用 `Class.newInstance` 造 charset 实例）。
        // 直接复用 reflect.rs 的构造逻辑，不再返回 UnsupportedOperationException。
        r.add(
            "jdk/internal/reflect/DirectConstructorHandleAccessor$NativeAccessor",
            "newInstance0",
            "(Ljava/lang/reflect/Constructor;[Ljava/lang/Object;)Ljava/lang/Object;",
            |vm, f| vm.ctor_new_instance(f.arg_i(0), f.arg_i(1)),
        );
        r
    }

    pub(super) fn add(&mut self, class: &'static str, name: &'static str, desc: &'static str, f: NativeFn) {
        self.map.insert((class, name, desc), f);
    }

    /// 摘掉一条登记（重建/接管用；帧栈那条线用它给旧假实现退场）
    pub(super) fn remove(&mut self, class: &'static str, name: &'static str, desc: &'static str) {
        self.map.remove(&(class, name, desc));
    }

    /// 登记一个「VM 接管」的方法（非 native，执行前拦下来）
    pub(super) fn add_override(&mut self, class: &'static str, name: &'static str, desc: &'static str, f: NativeFn) {
        self.overrides.insert((class, name, desc), f);
    }

    /// 摘掉一条「VM 接管」（`Thread.getStackTrace` 现在由 JDK 真字节码跑，不再接管）
    pub(super) fn remove_override(&mut self, class: &'static str, name: &'static str, desc: &'static str) {
        self.overrides.remove(&(class, name, desc));
    }

    pub(super) fn has_overrides(&self) -> bool {
        !self.overrides.is_empty()
    }

    pub(super) fn lookup_override(&self, class: &str, name: &str, desc: &str) -> Option<NativeFn> {
        self.overrides.get(&(class, name, desc)).copied()
    }

    /// 只查**静态注册表**（不含 JNI 动态绑定）。
    ///
    /// 之所以单独提出来：这一路的返回值是**纯 `fn` 指针**，同一个方法永远是同一个指针，
    /// 所以可以按调用点缓存（见 `exec.rs` 的 `NatSite`）。JNI 那条路**不能**缓存 ——
    /// `jni_lib::lookup` 返回的是共享跳板，真目标放在线程本地 pending 槽里，缓存了跳板，
    /// 下次别的 JNI 查表之后就会跳到别人的函数上。
    pub(crate) fn lookup_static(&self, class: &str, name: &str, desc: &str) -> Option<NativeFn> {
        self.map
            .get(&(class.to_string().as_str(), name, desc))
            .copied()
            .or_else(|| self.map.get(&("", name, desc)).copied())
    }

    pub(crate) fn lookup(&self, class: &str, name: &str, desc: &str) -> Option<NativeFn> {
        if let Some(f) = self.lookup_static(class, name, desc) {
            return Some(f);
        }
        // 静态表里没有 → 问 JNI 的动态绑定表：
        //   * `System.loadLibrary` 装进来的库里 `RegisterNatives` 注册过的函数指针；
        //   * 库里按 JNI 短名（`Java_pkg_Class_method`）能 `GetProcAddress` 到的符号。
        // 命中时 `jni_lib::lookup` 把目标放在线程本地，返回的跳板紧接着就会被调用。
        jni_lib::lookup(class, name, desc)
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
    frame.place_args_slots(&slots, args, receiver);
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
    // ---- 对象监视器（`wait`/`notify` 是 `synchronized` 的语义本体，见 `vm/thread.rs`）----
    // JDK 21+ 里 `wait()`/`wait(long)` 是 Java 包装，真正的 native 是 `wait0(J)V`。
    r.add("java/lang/Object", "notify", "()V", obj_notify);
    r.add("java/lang/Object", "notifyAll", "()V", obj_notify_all);
    r.add("java/lang/Object", "wait0", "(J)V", obj_wait0);
    r.add("java/lang/Object", "wait", "(J)V", obj_wait0);
    r.add("java/lang/Object", "wait", "(JI)V", obj_wait_millis_nanos);

    // ---- java.lang.Class ----
    // `<clinit>` 第一句就是 `registerNatives()`，不接住它连类都初始化不了
    r.add("java/lang/Class", "registerNatives", "()V", no_op);
    // `ClassLoader.<clinit>` 第一步就调它（HotSpot 里同样是空的 native 注册点）
    r.add("java/lang/ClassLoader", "registerNatives", "()V", no_op);
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

    // ---- JNI：动态库装载 ----
    //
    // 真 java.base 里 `System.loadLibrary` / `System.load` 是**普通方法**（→
    // `Runtime.loadLibrary0` → `ClassLoader.loadLibrary`），那两条链最后由
    // `jdk.internal.loader.NativeLibraries` 的 native 落地。这两行则覆盖
    // 「类自己把 loadLibrary 声明成 native」的写法（自造的 System 替身、嵌入式场景）。
    r.add("java/lang/System", "loadLibrary", "(Ljava/lang/String;)V", jni_lib::system_load_library);
    r.add("java/lang/System", "load", "(Ljava/lang/String;)V", jni_lib::system_load);

    // ---- JNI：真 java.base 的装载链落地（`NativeLibraries.load` 是唯一的 native）----
    r.add(
        "jdk/internal/loader/NativeLibraries",
        "load",
        "(Ljdk/internal/loader/NativeLibraries$NativeLibraryImpl;Ljava/lang/String;ZZ)Z",
        jni_lib::native_libraries_load,
    );
    // 内置库（libjava/libzip…）我们没有；返回 null = "不是内置库"
    r.add("jdk/internal/loader/NativeLibraries", "findBuiltinLib", "(Ljava/lang/String;)Ljava/lang/String;", |_, _| {
        Ok(Slot::I(0))
    });
    r.add("jdk/internal/loader/NativeLibraries", "unload", "(Ljava/lang/String;ZJ)V", no_op);
    r.add("jdk/internal/loader/NativeLibraries", "findEntry0", "(JLjava/lang/String;)J", |_, _| Ok(Slot::L(0)));
    // `java.io.File` 依赖它：`WinNTFileSystem.<clinit>` 里 native 侧缓存字段 ID。
    // 我们的文件操作不走那条路（JNI 也不碰），所以是空的。
    r.add("java/io/WinNTFileSystem", "initIDs", "()V", no_op);

    // ---- 系统属性（真 java.base 的 `System.getProperties()` 靠这两个 native 落地）----
    // `System.loadLibrary` 的整条 Java 链都要读 `java.home` / `os.name`，缺了它
    // 第一步就 `InternalError: null property: java.home`。
    r.add(
        "jdk/internal/util/SystemProps$Raw",
        "platformProperties",
        "()[Ljava/lang/String;",
        sysprops_platform,
    );
    r.add("jdk/internal/util/SystemProps$Raw", "vmProperties", "()[Ljava/lang/String;", sysprops_vm);

    // ---- java.lang.Shutdown（System.exit 的落地：刷输出 + 立刻退出）----
    r.add("java/lang/Shutdown", "beforeHalt", "()V", no_op);
    r.add("java/lang/Shutdown", "halt0", "(I)V", shutdown_halt0);

    // ---- jdk.internal.reflect.Reflection ----
    r.add("jdk/internal/reflect/Reflection", "getCallerClass", "()Ljava/lang/Class;", reflection_caller);
    // `Class.getModifiers()` / 反射层要知道类的 access_flags（`initPhase1` 里就会问）
    r.add(
        "jdk/internal/reflect/Reflection",
        "getClassAccessFlags",
        "(Ljava/lang/Class;)I",
        |vm, f| {
            let h = f.arg(0).as_i32();
            let flags = vm
                .class_obj_name(h)
                .and_then(|n| vm.store.class_file(&n))
                .map(|cf| cf.access_flags as i32)
                .unwrap_or(0);
            Ok(Slot::I(flags))
        },
    );

    // ---- java.lang.Runtime ----
    // `ArrayList.sort` → `Arrays.sort` 一族的路径上会问到 CPU 数（并行排序的门槛判断）。
    // 这条不补，任何 `list.sort(...)` 都会在这里断。
    r.add("java/lang/Runtime", "availableProcessors", "()I", runtime_available_processors);
    // 内存数字：只是给 `System.initPhase1` / `Runtime` 一个能用的值（VM 没有堆上限的概念）
    r.add("java/lang/Runtime", "maxMemory", "()J", |_, _| Ok(Slot::L(4 * 1024 * 1024 * 1024)));
    r.add("java/lang/Runtime", "totalMemory", "()J", |_, _| Ok(Slot::L(2 * 1024 * 1024 * 1024)));
    r.add("java/lang/Runtime", "freeMemory", "()J", |_, _| Ok(Slot::L(2 * 1024 * 1024 * 1024)));
    r.add("java/lang/Runtime", "gc", "()V", no_op);

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
    r.add("jdk/internal/misc/CDS", "getRandomSeedForDumping", "()J", |_, _| Ok(Slot::L(0)));
    r.add("jdk/internal/misc/CDS", "initializeFromArchive", "(Ljava/lang/Class;)V", no_op);

    // ---- jdk.internal.misc.Unsafe ----
    register_unsafe(r);

    // ---- java.lang.Thread ----
    register_thread(r);

    // ---- 分组注册（各文件独立，避免并行改同一处）----
    nat_intrinsics::register(r);
    nat_os::register(r);
    nat_util::register(r);
    nat_nio::register(r);
    nat_misc::register(r);
    nat_desktop::register(r);

    // ---- 原子读-改-写：**必须最后登记**，覆盖上面各文件里的非原子实现 ----
    //
    // 顺序是这里的全部要点：`register_unsafe(r)` 在前、`nat_intrinsics::register(r)` 在后，
    // 而登记表是 insert（后写赢）—— 所以 CAS / getAndAdd / getAndSet 实际跑的是
    // `nat_intrinsics` 那几条「读一次、算、写一次」的实现，中间没有原子性。
    // 后果不是「偶尔慢」而是**丢更新**：`AtomicInteger.incrementAndGet` 的 CAS 循环
    // 4 线程 × 5000 次实测丢 ~10%（18001/20000），`ConcurrentHashMap.casTabAt`、
    // `LongAdder`、`StampedLock` 全部跟着坏。
    register_unsafe_atomic(r);
    register_classloaders(r);
}

// ================================================================ 原子读-改-写

/// 分桶锁的数量。为什么要锁：槽位存储只提供 get/set（没有 per-cell CAS），
/// 而 `compareAndSetInt` 的语义是「读-比较-写」**整体原子**。
const RMW_STRIPES: usize = 64;

static RMW_LOCKS: [std::sync::Mutex<()>; RMW_STRIPES] =
    [const { std::sync::Mutex::new(()) }; RMW_STRIPES];

/// 取一把分桶锁：键是 (对象句柄, 槽下标) —— 同一格永远同一把锁（正确性），
/// 不同对象/不同槽基本不冲突（性能）。
///
/// 锁只覆盖「读 + 算 + 写」这几条指令，里面不调 Java、不分配、不 park，
/// 所以不会和监视器形成嵌套死锁；64 桶下 4 线程的冲突率可以忽略。
#[inline]
pub(super) fn rmw_guard(obj: i32, off: i64) -> std::sync::MutexGuard<'static, ()> {
    let k = (obj as u32 as usize)
        .wrapping_mul(0x9E37_79B1)
        ^ (off as usize).wrapping_mul(0x85EB_CA6B);
    RMW_LOCKS[(k >> 4) & (RMW_STRIPES - 1)]
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// 参数按宽度归一：宽度 4 取低 32 位（int / 引用句柄），宽度 8 取 64 位
#[inline]
fn rmw_arg(f: &Frame, i: usize, width: usize) -> u64 {
    if width >= 8 {
        f.arg_l(i) as u64
    } else {
        f.arg_i(i) as u32 as u64
    }
}

/// 读到的旧值 → 返回值（int 零扩展到 32 位，long 原样）
#[inline]
fn rmw_ret(cur: u64, width: usize) -> Slot {
    if width >= 8 {
        Slot::L(cur as i64)
    } else {
        Slot::I(cur as u32 as i32)
    }
}

/// `compareAndSet{Int,Long,Reference}`：整段「读-比较-写」在锁内完成
fn rmw_cas(vm: &mut Vm, f: &mut Frame, width: usize) -> Result<Slot, VmError> {
    let (obj, off) = (f.arg_i(0), f.arg_l(1));
    let i = off.max(0) as usize;
    let _g = rmw_guard(obj, off);
    let cur = read_mem(vm, obj, i, width);
    if cur == rmw_arg(f, 2, width) {
        write_mem(vm, obj, i, width, rmw_arg(f, 3, width));
        // Release：写出去的必须对下一个拿到锁的线程可见（acquire 在 lock() 里）
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        return Ok(Slot::I(1));
    }
    Ok(Slot::I(0))
}

/// `compareAndExchange{Int,Long,Reference}` → 返回**旧值**
fn rmw_cax(vm: &mut Vm, f: &mut Frame, width: usize) -> Result<Slot, VmError> {
    let (obj, off) = (f.arg_i(0), f.arg_l(1));
    let i = off.max(0) as usize;
    let _g = rmw_guard(obj, off);
    let cur = read_mem(vm, obj, i, width);
    if cur == rmw_arg(f, 2, width) {
        write_mem(vm, obj, i, width, rmw_arg(f, 3, width));
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }
    Ok(rmw_ret(cur, width))
}

/// `getAndAdd{Int,Long}` → 返回**旧值**
fn rmw_get_and_add(vm: &mut Vm, f: &mut Frame, width: usize) -> Result<Slot, VmError> {
    let (obj, off) = (f.arg_i(0), f.arg_l(1));
    let i = off.max(0) as usize;
    let _g = rmw_guard(obj, off);
    let cur = read_mem(vm, obj, i, width);
    let next = cur.wrapping_add(rmw_arg(f, 2, width));
    write_mem(vm, obj, i, width, next);
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    Ok(rmw_ret(cur, width))
}

/// `getAndSet{Int,Long,Reference}` → 返回**旧值**
fn rmw_get_and_set(vm: &mut Vm, f: &mut Frame, width: usize) -> Result<Slot, VmError> {
    let (obj, off) = (f.arg_i(0), f.arg_l(1));
    let i = off.max(0) as usize;
    let _g = rmw_guard(obj, off);
    let cur = read_mem(vm, obj, i, width);
    write_mem(vm, obj, i, width, rmw_arg(f, 2, width));
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    Ok(rmw_ret(cur, width))
}

fn cas_i(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> { rmw_cas(vm, f, 4) }
fn cas_l(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> { rmw_cas(vm, f, 8) }
fn cax_i(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> { rmw_cax(vm, f, 4) }
fn cax_l(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> { rmw_cax(vm, f, 8) }
fn add_i(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> { rmw_get_and_add(vm, f, 4) }
fn add_l(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> { rmw_get_and_add(vm, f, 8) }
fn set_i(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> { rmw_get_and_set(vm, f, 4) }
fn set_l(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> { rmw_get_and_set(vm, f, 8) }
/// 引用：句柄是 32 位，但槽宽是 8 —— 与 `nat_intrinsics` 的 `u_cas_ref` 同一个口径
fn cas_r(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> { rmw_cas(vm, f, 8) }
fn cax_r(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> { rmw_cax(vm, f, 8) }
fn set_r(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> { rmw_get_and_set(vm, f, 8) }

/// 把**原子**版本登记上去（覆盖 `nat_intrinsics` 里的非原子实现）
fn register_unsafe_atomic(r: &mut NativeRegistry) {
    let u = "jdk/internal/misc/Unsafe";
    r.add(u, "compareAndSetInt", "(Ljava/lang/Object;JII)Z", cas_i);
    r.add(u, "compareAndSetLong", "(Ljava/lang/Object;JJJ)Z", cas_l);
    r.add(
        u,
        "compareAndSetReference",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
        cas_r,
    );
    r.add(u, "compareAndExchangeInt", "(Ljava/lang/Object;JII)I", cax_i);
    r.add(u, "compareAndExchangeLong", "(Ljava/lang/Object;JJJ)J", cax_l);
    r.add(
        u,
        "compareAndExchangeReference",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        cax_r,
    );
    r.add(u, "getAndAddInt", "(Ljava/lang/Object;JI)I", add_i);
    r.add(u, "getAndAddLong", "(Ljava/lang/Object;JJ)J", add_l);
    r.add(u, "getAndSetInt", "(Ljava/lang/Object;JI)I", set_i);
    r.add(u, "getAndSetLong", "(Ljava/lang/Object;JJ)J", set_l);
    r.add(
        u,
        "getAndSetReference",
        "(Ljava/lang/Object;JLjava/lang/Object;)Ljava/lang/Object;",
        set_r,
    );
}

/// 合成一个「系统类加载器」对象。
///
/// 我们的类加载在 Rust 侧完成，没有真 `BuiltinClassLoader` 实例；但 JDK 里
/// `ClassLoader.getSystemClassLoader()` / `Thread.getContextClassLoader()` 只要返回 null，
/// 就会有一大片代码走 `Class.forName(name, false, null)` 或直接 NPE。
/// 这里给一个**不跑构造器**的 `ClassLoader` 壳（只填 `name`），够所有「拿它当令牌」的路径用。
fn synth_loader(vm: &mut Vm) -> i32 {
    let key = ("java/lang/ClassLoader".to_string(), "$vmless_system".to_string());
    if let Some(Slot::I(h)) = lk(&vm.sh.statics).get(&key).copied() {
        if h > 0 {
            return h;
        }
    }
    if !vm.store.has_class("java/lang/ClassLoader") {
        return 0;
    }
    let h = vm.alloc_vmobj("java/lang/ClassLoader");
    if h <= 0 {
        return 0;
    }
    let nm = vm.str_new("app".to_string());
    let _ = vm.put_named(h, "java/lang/ClassLoader", "name", Slot::I(nm));
    // `parent` 为 null（我们的加载器没有父链）
    //
    // **`unnamedModule` 必须非 null**：`ClassLoader.getUnnamedModule()` 直接返回这个字段，
    // 而 `ResourceBundle.getBundleImpl` 把它当 `CacheKey` 的实参做 `requireNonNull`
    // （`getBundleImpl(baseName, locale, caller, loader, control)` 里 `loader == null`
    // 时走 `BootLoader.getUnnamedModule()`，非 null 时走 `loader.getUnnamedModule()`）。
    // 少了它：任何走 `ResourceBundle` 的路径（Swing 的 `UIDefaults.getUI` 是第一个）
    // 都在 `CacheKey.<init>` 抛 NPE。这里造一个「未命名模块」替身：`name` 为 null
    // ⇒ `isNamed()` 为 false，与真实语义一致。
    if vm.store.has_class("java/lang/Module") {
        let m = vm.alloc_vmobj("java/lang/Module");
        if m > 0 {
            let _ = vm.put_named(m, "java/lang/Module", "loader", Slot::I(h));
            let _ = vm.put_named(h, "java/lang/ClassLoader", "unnamedModule", Slot::I(m));
            // 引导加载器的未命名模块（`BootLoader.getUnnamedModule()`；`loader == null`
            // 那条路径用它）。HotSpot 由 `BootLoader.<clinit>` 经 `defineUnnamedModule` 挂上，
            // 我们没跑那段引导，只能在这里补。
            lk(&vm.sh.statics)
                .insert(("jdk/internal/loader/BootLoader".to_string(), "UNNAMED_MODULE".to_string()), Slot::I(m));
        }
    }
    lk(&vm.sh.statics).insert(key, Slot::I(h));
    h
}

fn register_classloaders(r: &mut NativeRegistry) {
    // `SharedSecrets.getJavaLangReflectAccess()` —— **绝不能返回 null**。
    //
    // 真 JDK 的安装点是 `AccessibleObject.<clinit>`：
    //   `static { SharedSecrets.setJavaLangReflectAccess(new ReflectAccess()); }`
    // 但 `System.initPhase1` 在极早期就会通过 `Class` 走到反射访问，
    // 那时 `AccessibleObject` 还没初始化 → 拿到 null → `getRoot`/`getExecutableSharedParameterTypes`
    // 以空接收者被调用 → `initPhase1` 中断 → **`setOut0` 根本没跑到 → `System.out/err/in` 全 null**。
    // 而 `println` 被 exec 按方法名拦截，所以从 println 完全看不出来；
    // 只有 `System.out.print/printf`、或把 `System.out` 传出去才 NPE。
    //
    // 这里做懒安装：字段为空就先把 `AccessibleObject` 初始化（它会装 secret），
    // 还不行为我们直接造一个 `ReflectAccess` 实例塞进静态字段。
    r.add_override(
        "jdk/internal/access/SharedSecrets",
        "getJavaLangReflectAccess",
        "()Ljdk/internal/access/JavaLangReflectAccess;",
        |vm, _| {
            let key = ("jdk/internal/access/SharedSecrets".to_string(),
                       "javaLangReflectAccess".to_string());
            let cur = lk(&vm.sh.statics).get(&key).copied().unwrap_or(Slot::I(0));
            if cur.as_i32() > 0 {
                return Ok(cur);
            }
            let _ = vm.ensure_init("java/lang/reflect/AccessibleObject");
            let cur = lk(&vm.sh.statics).get(&key).copied().unwrap_or(Slot::I(0));
            if cur.as_i32() > 0 {
                return Ok(cur);
            }
            // 兜底：自己造一个（不跑构造器 —— `ReflectAccess` 没有实例状态）
            if vm.store.has_class("java/lang/reflect/ReflectAccess") {
                let h = vm.alloc_vmobj("java/lang/reflect/ReflectAccess");
                if h > 0 {
                    lk(&vm.sh.statics).insert(key, Slot::I(h));
                }
                return Ok(Slot::I(h));
            }
            Ok(Slot::I(0))
        },
    );
    // `JavaLangReflectAccess` 的成员都声明在接口上，而实现在 `ReflectAccess`。
    // 反射层还有两条 `Executable` 的访问器（`initPhase1` 会先碰到它们）。
    r.add_override(
        "java/lang/reflect/ReflectAccess",
        "getExecutableSharedParameterTypes",
        "(Ljava/lang/reflect/Executable;)[Ljava/lang/Class;",
        |vm, f| {
            let ex = f.arg(0);
            Ok(vm.get_named(ex.as_i32(), "java/lang/reflect/Executable", "parameterTypes"))
        },
    );
    r.add_override(
        "java/lang/reflect/ReflectAccess",
        "getExecutableSharedExceptionTypes",
        "(Ljava/lang/reflect/Executable;)[Ljava/lang/Class;",
        |vm, f| {
            let ex = f.arg(0);
            Ok(vm.get_named(ex.as_i32(), "java/lang/reflect/Executable", "exceptionTypes"))
        },
    );
    // `JavaLangReflectAccess.getRoot(AccessibleObject)`：返回「可访问性根」对象。
    // 我们没有任何 root 机制（每个 `Field`/`Method` 只有它自己），所以**原样返回实参**。
    // 少了它，`System.initPhase1` 在装 `SharedSecrets` 时就断，`System.out/err/in` 全 null。
    r.add_override(
        "java/lang/reflect/ReflectAccess",
        "getRoot",
        "(Ljava/lang/reflect/AccessibleObject;)Ljava/lang/reflect/AccessibleObject;",
        |_, f| Ok(f.arg(0)),
    );
    // ---- java.lang.ref.Reference：`Cleaner` 整套建立在这上面 ----
    // `refersTo0` 是比较「引用对象内部的 referent」与实参；我们的对象槽直接可读。
    r.add_override(
        "java/lang/ref/Reference",
        "refersTo0",
        "(Ljava/lang/Object;)Z",
        |vm, f| {
            let this = f.this();
            let other = f.arg(0);
            let r = vm.get_named(this, "java/lang/ref/Reference", "referent");
            Ok(Slot::I(i32::from(r.as_i32() == other.as_i32())))
        },
    );
    r.add_override(
        "java/lang/ref/Reference",
        "clear0",
        "()V",
        |vm, f| {
            let this = f.this();
            let _ = vm.put_named(this, "java/lang/ref/Reference", "referent", Slot::I(0));
            Ok(Slot::I(0))
        },
    );
    r.add_override(
        "java/lang/ref/PhantomReference",
        "clear0",
        "()V",
        |vm, f| {
            let this = f.this();
            let _ = vm.put_named(this, "java/lang/ref/Reference", "referent", Slot::I(0));
            Ok(Slot::I(0))
        },
    );
    // `Reference.get()` 只在 referent 还没被清时返回它
    r.add_override(
        "java/lang/ref/Reference",
        "get0",
        "()Ljava/lang/Object;",
        |vm, f| {
            let this = f.this();
            Ok(vm.get_named(this, "java/lang/ref/Reference", "referent"))
        },
    );
    // `ClassLoader.registerAsParallelCapable()` —— 直接返回 true。
    //
    // 真 JDK 用它把「这个加载器支持并行注册类」记进一张 `Set<Class<?>>`，
    // 判据是 `Reflection.getCallerClass()`（要跳过反射帧才算得出真正的子类）。
    // 我们的类加载在 Rust 侧、且全程持有全局锁，**本来就不存在并行注册问题**，
    // 所以两个重载都直接答 true。少了这条，`ClassLoader.<clinit>` 会抛
    // `InternalError: Unable to register as parallel capable`，整条类加载链断掉。
    for d in ["()Z", "(Ljava/lang/Class;)Z"] {
        r.add_override(
            "java/lang/ClassLoader",
            "registerAsParallelCapable",
            d,
            |_, _| Ok(Slot::I(1)),
        );
    }
    // `Reflection.getCallerClass()` —— `@CallerSensitive` 系列的基石。
    // **`ClassLoader.registerAsParallelCapable()` 就是靠它拿调用者类**：
    // 每个真类加载器的 `<clinit>` 都会调它，拿不到类（或拿到 Object）就抛
    // `IllegalCallerException: class java.lang.Object not a subclass of ClassLoader`，
    // 于是 `ClassLoader.getSystemClassLoader()` 整条链断掉 —— `Cleaner.create()` 也跟着断。
    r.add_override(
        "jdk/internal/reflect/Reflection",
        "getCallerClass",
        "()Ljava/lang/Class;",
        |vm, _| {
            let n = if vm.last_caller == usize::MAX {
                "java/lang/Object".to_string()
            } else {
                vm.store
                    .name_of_index(vm.last_caller)
                    .unwrap_or_else(|| "java/lang/Object".to_string())
            };
            Ok(Slot::I(vm.class_obj(&n)))
        },
    );
    r.add_override(
        "java/lang/ClassLoader",
        "getSystemClassLoader",
        "()Ljava/lang/ClassLoader;",
        |vm, _| Ok(Slot::I(synth_loader(vm))),
    );
    r.add_override(
        "java/lang/ClassLoader",
        "getPlatformClassLoader",
        "()Ljava/lang/ClassLoader;",
        |vm, _| Ok(Slot::I(synth_loader(vm))),
    );
    r.add_override(
        "java/lang/Thread",
        "getContextClassLoader",
        "()Ljava/lang/ClassLoader;",
        |vm, f| {
            let this = f.this();
            let v = vm.get_named(this, "java/lang/Thread", "contextClassLoader");
            if v.as_i32() > 0 {
                Ok(v)
            } else {
                Ok(Slot::I(synth_loader(vm)))
            }
        },
    );
    r.add_override(
        "java/lang/Thread",
        "setContextClassLoader",
        "(Ljava/lang/ClassLoader;)V",
        |vm, f| {
            let this = f.this();
            let v = f.arg(0);
            let _ = vm.put_named(this, "java/lang/Thread", "contextClassLoader", v);
            Ok(Slot::I(0))
        },
    );
}

/// `java.lang.Thread`：native 部分 + **VM 接管**的构造器 / `start()`。
///
/// 接管判据：真字节码里这几步要实现的是 VM 内部语义（建 OS 线程、维护
/// `eetop`/`threadStatus`），而 HotSpot 自己也是 native 实现；照字节码跑则会被拖进
/// `ThreadGroup` → `ClassLoader.getSystemClassLoader()` → `ServiceLoader` 一整条链，
/// 而那条链对本 VM 没有任何意义。**接管的只有「建/起线程」，其余全走真字节码**：
/// `run` / `join` / `getState` / `isAlive` / `sleep` / `wait` 的语义都在 Java 里。
fn register_thread(r: &mut NativeRegistry) {
    let t = "java/lang/Thread";
    // `<clinit>` 第一句
    r.add(t, "registerNatives", "()V", no_op);
    // 当前线程的 Thread 对象（懒建）
    r.add(t, "currentThread", "()Ljava/lang/Thread;", thread_current);
    r.add(t, "currentCarrierThread", "()Ljava/lang/Thread;", thread_current);
    r.add(t, "setCurrentThread", "(Ljava/lang/Thread;)V", no_op);
    // `sleep` 全族最终落到它（`sleep(long)` / `sleep(long,int)` / `sleep(Duration)`）
    r.add(t, "sleepNanos0", "(J)V", thread_sleep_nanos0);
    r.add(t, "interrupt0", "()V", thread_interrupt0);
    r.add(t, "clearInterruptEvent", "()V", no_op);
    r.add(t, "holdsLock", "(Ljava/lang/Object;)Z", thread_holds_lock);
    r.add(t, "yield0", "()V", thread_yield0);
    // 作用域值（JDK 21+ 的 ScopedValue）：本 VM 不支持，返回 null 即可 ——
    // `Thread.runWith` 只是把它原样传给 `ensureMaterializedForStackWalk`
    r.add(t, "findScopedValueBindings", "()Ljava/lang/Object;", |_, _| Ok(Slot::I(0)));
    r.add(t, "scopedValueCache", "()[Ljava/lang/Object;", |_, _| Ok(Slot::I(0)));
    r.add(t, "setScopedValueCache", "([Ljava/lang/Object;)V", no_op);
    r.add(t, "ensureMaterializedForStackWalk", "(Ljava/lang/Object;)V", no_op);
    // 栈轨迹：本 VM 没有调用栈模型（与 `Throwable.getStackTraceDepth` 一致）
    r.add(t, "getStackTrace0", "()Ljava/lang/Object;", |_, _| Ok(Slot::I(0)));
    r.add(t, "dumpThreads", "([Ljava/lang/Thread;)[[Ljava/lang/StackTraceElement;", |_, _| {
        Ok(Slot::I(0))
    });
    r.add(t, "getThreads", "()[Ljava/lang/Thread;", |_, _| Ok(Slot::I(0)));
    r.add(t, "setPriority0", "(I)V", no_op);
    r.add(t, "setNativeName", "(Ljava/lang/String;)V", no_op);
    r.add(t, "getNextThreadIdOffset", "()J", |_, _| Ok(Slot::L(0)));

    // 构造器：**全部**由 VM 接管（任何一个漏了都会把 ThreadGroup 那条链拖进来）
    r.add_override(t, "<init>", "()V", thread_ctor_0);
    r.add_override(t, "<init>", "(Ljava/lang/Runnable;)V", thread_ctor_r);
    r.add_override(t, "<init>", "(Ljava/lang/String;)V", thread_ctor_s);
    r.add_override(t, "<init>", "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;)V", thread_ctor_gr);
    r.add_override(t, "<init>", "(Ljava/lang/ThreadGroup;Ljava/lang/String;)V", thread_ctor_gs);
    r.add_override(t, "<init>", "(Ljava/lang/Runnable;Ljava/lang/String;)V", thread_ctor_rs);
    r.add_override(t, "<init>", "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;)V", thread_ctor_grs);
    r.add_override(t, "<init>", "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;J)V", thread_ctor_grsj);
    r.add_override(t, "<init>", "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;JZ)V", thread_ctor_grsjz);
    // `start()`：`ThreadContainers.root()` 那一层对我们是空的，直接建线程
    r.add_override(t, "start", "()V", thread_start_override);
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
    // `Unsafe.objectFieldOffset(Field)` 在 JDK 21+ 走的是「(Class, String)」那条重载
    // （`Field.getFieldAccessor` / `MethodHandles.Lookup` 都会问它），同一个语义按名字查
    r.add(
        u,
        "objectFieldOffset1",
        "(Ljava/lang/Class;Ljava/lang/String;)J",
        unsafe_field_offset_by_name,
    );
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
    // volatile 变体：读/写前后加 SeqCst 栅栏（D 项：内存可见性）
    r.add(u, "getBooleanVolatile", "(Ljava/lang/Object;J)Z", unsafe_get8);
    r.add(u, "getByteVolatile", "(Ljava/lang/Object;J)B", unsafe_get8);
    r.add(u, "getCharVolatile", "(Ljava/lang/Object;J)C", unsafe_get16);
    r.add(u, "getShortVolatile", "(Ljava/lang/Object;J)S", unsafe_get16);
    r.add(u, "getIntVolatile", "(Ljava/lang/Object;J)I", unsafe_get32_v);
    r.add(u, "getLongVolatile", "(Ljava/lang/Object;J)J", unsafe_get64_v);
    r.add(u, "getFloatVolatile", "(Ljava/lang/Object;J)F", unsafe_get_float);
    r.add(u, "getDoubleVolatile", "(Ljava/lang/Object;J)D", unsafe_get_double);
    r.add(u, "getReferenceVolatile", "(Ljava/lang/Object;J)Ljava/lang/Object;", unsafe_get_v);
    r.add(u, "putBooleanVolatile", "(Ljava/lang/Object;JZ)V", unsafe_put8);
    r.add(u, "putByteVolatile", "(Ljava/lang/Object;JB)V", unsafe_put8);
    r.add(u, "putCharVolatile", "(Ljava/lang/Object;JC)V", unsafe_put16);
    r.add(u, "putShortVolatile", "(Ljava/lang/Object;JS)V", unsafe_put16);
    r.add(u, "putIntVolatile", "(Ljava/lang/Object;JI)V", unsafe_put32_v);
    r.add(u, "putLongVolatile", "(Ljava/lang/Object;JJ)V", unsafe_put64_v);
    r.add(u, "putFloatVolatile", "(Ljava/lang/Object;JF)V", unsafe_put_float);
    r.add(u, "putDoubleVolatile", "(Ljava/lang/Object;JD)V", unsafe_put_double);
    r.add(u, "putReferenceVolatile", "(Ljava/lang/Object;JLjava/lang/Object;)V", unsafe_put_v);
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
    // 线程停靠：`LockSupport` → `Unsafe.park/unpark`（AQS 与并发容器的底座，必须真阻塞）
    r.add(u, "park", "(ZJ)V", unsafe_park);
    r.add(u, "unpark", "(Ljava/lang/Object;)V", unsafe_unpark);
    r.add(u, "fullFence", "()V", fence_seqcst);
    r.add(u, "acquireFence", "()V", fence_seqcst);
    r.add(u, "releaseFence", "()V", fence_seqcst);
    r.add(u, "loadFence", "()V", fence_seqcst);
    r.add(u, "storeFence", "()V", fence_seqcst);
}
// ---------------------------------------------------------------- 实现

/// `Shutdown.halt0(int)`：把已经缓冲的输出刷出去再立刻退出进程。
///
/// 为什么在这里退：`System.exit` 的语义是「**不**等非 daemon 线程」（与 `main` 返回不同）。
/// 输出是本 VM 的缓冲（`Vm::output`），不刷就丢 —— HotSpot 的 exit 也会 flush。
fn shutdown_halt0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let code = f.arg_i(0);
    vm.flush_output();
    std::process::exit(code);
}

/// `Reflection.getCallerClass()`：本 VM 没有调用栈模型，返回 `Object` 的类对象。
/// 它只被「有 SecurityManager 时」的分支用到（我们没有 SecurityManager，值用不上）。
fn reflection_caller(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::I(vm.class_obj("java/lang/Object")))
}

fn no_op(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    void()
}

// ---- 对象监视器：wait / notify / notifyAll（语义在 `vm/thread.rs`，这里只做参数搬运）----

fn obj_notify(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    vm.monitor_notify(this, false)?;
    void()
}

fn obj_notify_all(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    vm.monitor_notify(this, true)?;
    void()
}

/// `wait0(long millis)` / `wait(long)`：0 = 无限期
fn obj_wait0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let millis = f.arg_l(0);
    let timeout = if millis <= 0 {
        None
    } else {
        Some(std::time::Duration::from_millis(millis as u64))
    };
    vm.monitor_wait(this, timeout)?;
    void()
}

/// `wait(long millis, int nanos)`
fn obj_wait_millis_nanos(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let total = f
        .arg_l(0)
        .saturating_mul(1_000_000)
        .saturating_add(f.arg_i(1).max(0) as i64);
    let timeout = if total <= 0 {
        None
    } else {
        Some(std::time::Duration::from_nanos(total as u64))
    };
    vm.monitor_wait(this, timeout)?;
    void()
}

// ---- java.lang.Thread ----

fn thread_current(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::I(vm.current_thread_obj()))
}

fn thread_sleep_nanos0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let nanos = f.arg_l(0).max(0) as u64;
    if nanos == 0 {
        return void();
    }
    vm.thread_sleep(std::time::Duration::from_nanos(nanos))?;
    void()
}

fn thread_interrupt0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    vm.thread_interrupt(this);
    void()
}

fn thread_holds_lock(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::I(i32::from(vm.monitor_holds(f.arg_i(0)))))
}

fn thread_yield0(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    vm.safepoint();
    void()
}

/// `Thread.<init>` 的接管体：`name` = String 句柄（0 → 自动起名 "Thread-N"），`task` = Runnable
fn thread_init(vm: &mut Vm, f: &mut Frame, name: i32, task: i32) -> Result<Slot, VmError> {
    let this = f.this();
    let nm = if name != 0 { vm.obj_to_string(name) } else { vm.next_thread_name() };
    vm.thread_fill(this, &nm, task)?;
    void()
}

fn thread_ctor_0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    thread_init(vm, f, 0, 0)
}
fn thread_ctor_r(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let task = f.arg_i(0);
    thread_init(vm, f, 0, task)
}
fn thread_ctor_s(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let name = f.arg_i(0);
    thread_init(vm, f, name, 0)
}
fn thread_ctor_gr(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let task = f.arg_i(1);
    thread_init(vm, f, 0, task)
}
fn thread_ctor_gs(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let name = f.arg_i(1);
    thread_init(vm, f, name, 0)
}
fn thread_ctor_rs(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let task = f.arg_i(0);
    let name = f.arg_i(1);
    thread_init(vm, f, name, task)
}
fn thread_ctor_grs(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let task = f.arg_i(1);
    let name = f.arg_i(2);
    thread_init(vm, f, name, task)
}
fn thread_ctor_grsj(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let task = f.arg_i(1);
    let name = f.arg_i(2);
    thread_init(vm, f, name, task)
}
fn thread_ctor_grsjz(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let task = f.arg_i(1);
    let name = f.arg_i(2);
    thread_init(vm, f, name, task)
}

fn thread_start_override(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    vm.thread_start(this)?;
    void()
}

/// `jdk.internal.misc.VM.initialize()`：建好 `savedProps`（空 Map）。
/// 不建的话 `VM.getSavedProperty` 抛 IllegalStateException("Not yet initialized")，
/// `Integer.IntegerCache.<clinit>` 就挂在里面。
fn vm_initialize(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    let map = vm.new_instance("java/util/HashMap", "()V")?;
    lk(&vm.sh.statics).insert(("jdk/internal/misc/VM".to_string(), "savedProps".to_string()), Slot::I(map));
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
            let slots = vm.sh.heap.snapshot(this).unwrap_or_default();
            let cls = vm.sh.heap.class_of(this).unwrap_or_else(|| "java/lang/Object".into());
            Ok(Slot::I(vm.sh.heap.new_object(slots, &cls).unwrap_or(0)))
        }
        RefKind::Arr(_) => {
            let slots = vm.sh.heap.snapshot(this).unwrap_or_default();
            let cls = vm.sh.heap.class_of(this).unwrap_or_else(|| "[Ljava/lang/Object;".into());
            Ok(Slot::I(vm.sh.heap.new_array(slots, &cls).unwrap_or(0)))
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
    Ok(Slot::I(if vm.type_assignable(&other, &me) { 1 } else { 0 }))
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

/// `Runtime.availableProcessors()`：并行排序的门槛判断会问它。
/// 报一个真实值（`std::thread::available_parallelism`），不编造。
fn runtime_available_processors(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    Ok(Slot::I(n as i32))
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

/// 数组元素宽度缓存：`class_id → 宽度 | 0x10（是 `[B`）`。
///
/// `class_id` 与描述符的对应关系**永久不变**（`Shared::intern_class_id` 只增、
/// 不复用 id），所以这张直连表可以无锁读写：读到的条目 id 不匹配就重算再写回。
/// **不要**在这里缓存布局（槽数 / 引用位图）—— 那是并行加载下会变的东西。
static ARR_W: [std::sync::atomic::AtomicU32; 8192] =
    [const { std::sync::atomic::AtomicU32::new(0) }; 8192];

/// 句柄 → `Some((元素字节宽度, 是不是 `[B`))`；不是数组返回 `None`。
///
/// 数组句柄是**负数**、对象/字符串是正数（见 `shared.rs` 的
/// `new_object`/`new_array`/`new_string`），所以一次符号判断就分开了 ——
/// 不用 `Vm::ref_kind`：它会为**每个**句柄分配一个 `String` 类名，
/// 而 `Unsafe` 的每次读写都要问一次（实测这是 putByte 体里最大的一格）。
#[inline]
fn arr_kind(vm: &Vm, h: i32) -> Option<(usize, bool)> {
    if h >= 0 {
        return None;
    }
    let id = vm.sh.heap.hdr(h)?.class_id;
    let slot = (id as usize) & (ARR_W.len() - 1);
    let e = ARR_W[slot].load(std::sync::atomic::Ordering::Relaxed);
    let code = if (e >> 8) == id.wrapping_add(1) {
        e as u8
    } else {
        let d = vm.sh.heap.class_of(h)?;
        let c = (elem_width(&d) as u8) | if &*d == "[B" { 0x10 } else { 0 };
        ARR_W[slot].store((id.wrapping_add(1) << 8) | c as u32, std::sync::atomic::Ordering::Relaxed);
        c
    };
    Some(((code & 0x0F) as usize, code & 0x10 != 0))
}

/// `Unsafe` 的 `offset`（字节偏移）→ 我们数组堆里的**元素下标**
fn slot_index(vm: &Vm, obj: i32, off: i64) -> Option<usize> {
    let off = usize::try_from(off).ok()?;
    match arr_kind(vm, obj) {
        Some((w, _)) => Some(off / w),
        _ => Some(off),
    }
}

/// `[B` 数组在内存里是**连续字节**，而 `Unsafe.getLong/getInt` 按 HotSpot 的语义要
/// 一次读 8 / 4 个字节并打包（小端）。`ArraysSupport.vectorizedMismatch` 正是靠它
/// 按机器字比较。
fn read_mem(vm: &Vm, obj: i32, i: usize, width: usize) -> u64 {
    if let Some((w, is_bytes)) = arr_kind(vm, obj) {
        if is_bytes && width > 1 {
            let mut out = 0u64;
            for k in 0..width {
                let b = vm.sh.heap.get(obj, i + k).map(|s| s.as_i32() as u8).unwrap_or(0) as u64;
                out |= (b & 0xFF) << (8 * k);
            }
            return out;
        }
        let s = vm.sh.heap.get(obj, i / w).unwrap_or(Slot::I(0));
        let raw = match width {
            8 => s.as_i64() as u64,
            _ => s.as_i32() as u32 as u64,
        };
        let mask = if width >= 8 { u64::MAX } else { (1u64 << (8 * width)) - 1 };
        return raw & mask;
    }
    // 不是数组：类对象静态字段 / 字符串字段视图 / 对象实例槽
    let s = unsafe_slot(vm, obj, i as i64).unwrap_or(Slot::I(0));
    let raw = match width {
        8 => s.as_i64() as u64,
        _ => s.as_i32() as u32 as u64,
    };
    let mask = if width >= 8 { u64::MAX } else { (1u64 << (8 * width)) - 1 };
    raw & mask
}

fn write_mem(vm: &mut Vm, obj: i32, i: usize, width: usize, v: u64) {
    if let Some((w, is_bytes)) = arr_kind(vm, obj) {
        if is_bytes && width > 1 {
            for k in 0..width {
                vm.sh.heap.set(obj, i + k, Slot::I(((v >> (8 * k)) & 0xFF) as i32));
            }
            return;
        }
        let slot = match width {
            8 => Slot::L(v as i64),
            _ => Slot::I(v as u32 as i32),
        };
        let idx = i / w;
        // 引用槽必须存成 `Slot::I` + 走 `set_ref`（见 `unsafe_put_impl` 的长注释）
        let wrong_repr = !matches!(slot, Slot::I(_));
        let is_ref = wrong_repr && vm.sh.heap.hdr(obj).map(|o| o.is_ref(idx)).unwrap_or(false);
        if is_ref {
            vm.sh.heap.set_ref(obj, idx, Slot::I(slot.as_i32()));
        } else {
            vm.sh.heap.set(obj, idx, slot);
        }
        return;
    }
    let slot = match width {
        8 => Slot::L(v as i64),
        _ => Slot::I(v as u32 as i32),
    };
    unsafe_put_impl(vm, obj, i as i64, slot);
}
fn unsafe_slot(vm: &Vm, obj: i32, off: i64) -> Option<Slot> {
    // `staticFieldBase` 给的是**类对象**：这类访问要落到静态字段表，不是某个实例的槽。
    if let Some(cn) = vm.class_obj_name(obj) {
        let fname = vm.store.field_name_at(&cn, off.max(0) as usize)?;
        return Some(
            lk(&vm.sh.statics)
                .get(&(cn, fname))
                .copied()
                .unwrap_or(Slot::I(0)),
        );
    }
    let i = slot_index(vm, obj, off)?;
    // 字符串句柄（`ldc "abc"` 的字段视图）走字符串堆；其余（数组/对象）都是堆槽。
    // 这里不再问 `ref_kind`：它每次都要分配一个 String 类名，而只有「是不是字符串」
    // 这一位有用 —— 句柄编码里已经带着（`STR_BASE` 以上是字符串）。
    if obj >= STR_BASE {
        // 材料化过才有；没材料化就当成 0，调用方自己会重新取
        vm.sh.heap.str_field_get(obj, i)
    } else {
        vm.sh.heap.get(obj, i)
    }
}

fn unsafe_put_impl(vm: &mut Vm, obj: i32, off: i64, v: Slot) {
    // **数组快路**：数组句柄是负数，所以「是不是类对象」这一问可以整段跳过 ——
    // `class_obj_name` 一次 54 ns（拿 `class_objs` 的互斥量 + clone 一个 String），
    // 而 `Unsafe.put*` 的热路径（`StringLatin1/StringUTF16` 写 `byte[]`/`char[]`）
    // 全是数组。它在对象路径上仍然必须留着（静态字段访问要靠它认类对象）。
    if let Some((w, _)) = arr_kind(vm, obj) {
        let Some(i) = usize::try_from(off).ok().map(|o| o / w) else { return };
        let wrong_repr = !matches!(v, Slot::I(_));
        let is_ref = wrong_repr && vm.sh.heap.hdr(obj).map(|o| o.is_ref(i)).unwrap_or(false);
        if is_ref {
            vm.sh.heap.set_ref(obj, i, Slot::I(v.as_i32()));
        } else {
            vm.sh.heap.set(obj, i, v);
        }
        return;
    }
    // 同上：`staticFieldBase` 的类对象 → 写静态字段表
    if let Some(cn) = vm.class_obj_name(obj) {
        if let Some(fname) = vm.store.field_name_at(&cn, off.max(0) as usize) {
            lk(&vm.sh.statics).insert((cn, fname), v);
        }
        return;
    }
    let Some(i) = slot_index(vm, obj, off) else { return };
    match vm.ref_kind(obj) {
        RefKind::Arr(_) | RefKind::Obj(_) => {
            let wrong_repr = !matches!(v, Slot::I(_));
            let is_ref = wrong_repr && vm.sh.heap.hdr(obj).map(|o| o.is_ref(i)).unwrap_or(false);
            if is_ref {
                vm.sh.heap.set_ref(obj, i, Slot::I(v.as_i32()));
            } else {
                vm.sh.heap.set(obj, i, v);
            }
        }
        RefKind::Str => vm.str_field_write(obj, i, v),
        _ => {}
    }
}

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

// ---- volatile 变体：读后面 / 写前面各一道 SeqCst 栅栏 ----
//
// 本 VM 的堆只在持有 GIL 时访问，而 GIL 的每一次交接都是互斥锁的 acquire/release
// （本身就是全屏障），所以这里再加栅栏是**双保险 + 明确的编译器屏障**：
// `volatile` 的语义要求「不重排、立即可见」，不能靠「反正单线程」的隐式结论。
fn fence() {
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
}

fn fence_seqcst(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    fence();
    void()
}

fn unsafe_get32_v(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let r = unsafe_get32(vm, f)?;
    fence();
    Ok(r)
}

fn unsafe_get64_v(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let r = unsafe_get64(vm, f)?;
    fence();
    Ok(r)
}

fn unsafe_get_v(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let r = unsafe_get(vm, f)?;
    fence();
    Ok(r)
}

fn unsafe_put32_v(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    fence();
    unsafe_put32(vm, f)
}

fn unsafe_put64_v(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    fence();
    unsafe_put64(vm, f)
}

fn unsafe_put_v(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    fence();
    unsafe_put(vm, f)
}

/// `Unsafe.park(isAbsolute, time)`：真阻塞（见 `Vm::thread_park`）
fn unsafe_park(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let nanos = f.arg_l(1);
    let t = if nanos <= 0 {
        None
    } else {
        Some(std::time::Duration::from_nanos(nanos as u64))
    };
    vm.thread_park(t)?;
    void()
}

/// `Unsafe.unpark(thread)`（null = 当前线程）
fn unsafe_unpark(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let t = f.arg_i(0);
    vm.thread_unpark(t);
    void()
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

/// `objectFieldOffset1(Class, String)`：与 `objectFieldOffset0` 同一个语义，参数形式不同
fn unsafe_field_offset_by_name(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let cls = vm.class_obj_name(f.arg_i(0)).unwrap_or_default();
    let name = vm.text_of(f.arg_i(1));
    Ok(Slot::L(vm.store.field_index(&cls, &name).map(|i| i as i64).unwrap_or(-1)))
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
    Ok(Slot::I(vm.sh.heap.new_object(vec![Slot::I(0); n.max(1)], &cls).unwrap_or(0)))
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}


/// `System.nanoTime()`：**一次 `QueryPerformanceCounter`**。
///
/// 契约只要求「单调、单位纳秒、起点任意」，不要求对齐 epoch —— 原来每次调
/// `SystemTime::now()`（`GetSystemTimePreciseAsFileTime` 一路）实测 48 ns，
/// 而且系统时间被回拨时不单调。`Instant::now()` 在 Windows 上就是 QPC，
/// 起点在首次调用时取一次 epoch 纳秒，量级保持不变（有代码会拿它跟
/// `currentTimeMillis()*1e6` 比大小，别改原点）。
#[cfg(windows)]
extern "system" {
    fn QueryPerformanceCounter(out: *mut i64) -> i32;
    fn QueryPerformanceFrequency(out: *mut i64) -> i32;
}

#[cfg(windows)]
#[inline]
fn qpc() -> i64 {
    let mut v = 0i64;
    unsafe { QueryPerformanceCounter(&mut v) };
    v
}

/// `System.nanoTime()`：一次 `QueryPerformanceCounter` + 一次乘法。
///
/// 契约只要求「单调、纳秒、起点任意」—— 原来每次调 `SystemTime::now()`
/// （`GetSystemTimePreciseAsFileTime` 一路）实测 **47.6 ns**，而且系统时间被回拨时
/// **不单调**。这里直接读 QPC：Windows 的 QPC 频率一般是 10 MHz（整除 1e9），
/// 于是纳秒 = 刻度 × 100，一次乘法搞定；频率不能整除时才走 u128 除法（每次进程只
/// 判一次）。起点仍是首次调用时的 epoch 纳秒：量级不变，别处拿它跟
/// `currentTimeMillis()*1e6` 比大小也不会炸。
#[cfg(windows)]
fn now_nanos() -> i64 {
    static BASE: std::sync::OnceLock<(i64, i64, i64)> = std::sync::OnceLock::new();
    let (q0, v0, per) = *BASE.get_or_init(|| {
        let mut f = 0i64;
        unsafe { QueryPerformanceFrequency(&mut f) };
        let e = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        // 频率整除 1e9 时 per 是每刻度纳秒数，否则是 0（走慢路）
        let per = if f > 0 && 1_000_000_000 % f == 0 { 1_000_000_000 / f } else { 0 };
        (qpc(), e, per)
    });
    let d = qpc() - q0;
    if per > 0 {
        return v0 + d * per;
    }
    let mut f = 0i64;
    unsafe { QueryPerformanceFrequency(&mut f) };
    v0 + ((d as i128 * 1_000_000_000i128) / (f.max(1) as i128)) as i64
}

#[cfg(not(windows))]
fn now_nanos() -> i64 {
    static BASE: std::sync::OnceLock<(std::time::Instant, i64)> = std::sync::OnceLock::new();
    let (t0, v0) = BASE.get_or_init(|| {
        let e = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        (std::time::Instant::now(), e)
    });
    v0 + t0.elapsed().as_nanos() as i64
}

// ---- 系统属性：JNI（以及整个真 java.base 路径）的前置条件 ----
//
// 真 java.base 模式下 `System.getProperties()` 是 null —— 因为 `System.initPhase1()`
// 本该由 **VM 启动时**调用，而我们之前没调。它的第一步是问 native 要属性：
//
//   * `jdk.internal.util.SystemProps$Raw.platformProperties()` → **按位置**的 String[40]
//     （下标常量 `_os_name_NDX = 21` 之类是编译进 class 文件的，所以位置不能错）
//   * `jdk.internal.util.SystemProps$Raw.vmProperties()` → 键/值**成对**交替的 String[]
//
// 缺了它们：`System.loadLibrary` 在 `InternalError: null property: java.home` 上死，
// LWJGL 的 `Library` / `Configuration` 也拿不到 `os.name`。

/// `java.home` 的值。
///
/// **`VMLESS_JDK` 只表示「`java.base` 的类从哪来」，不等于 JDK 安装目录。**
/// 它常常指向 `lib/modules` 或解包出来的 `img/` —— 那里面**没有 `conf/security/java.security`**，
/// 于是 `java.security` 初始化抛 `InternalError: Error loading java.security file`，
/// 连带 `SecureRandom` / `Selector` / TLS 全挂（实测：`Selector.open()` 的第一道坎）。
///
/// 顺序：`VMLESS_JAVA_HOME` → `VMLESS_JDK` 里真带 `bin`+`lib` 的那层 → 自动探测的常见安装路径。
fn jdk_home() -> String {
    if let Ok(p) = std::env::var("VMLESS_JAVA_HOME") {
        let p = p.trim().to_string();
        if !p.is_empty() {
            return p;
        }
    }
    if let Ok(p) = std::env::var("VMLESS_JDK") {
        let p = p.trim().to_string();
        if !p.is_empty() {
            let path = std::path::Path::new(&p);
            if path.join("lib").is_dir() && path.join("bin").is_dir() {
                return p;
            }
            // `VMLESS_JDK` 也可能指向 `lib/modules` 或解包目录：往上找带 bin+lib 的那层
            let mut cur = Some(path);
            for _ in 0..3 {
                cur = cur.and_then(|c| c.parent());
                if let Some(c) = cur {
                    if c.join("lib").is_dir() && c.join("bin").is_dir() {
                        return c.to_string_lossy().into_owned();
                    }
                }
            }
            // 到这一步说明 `VMLESS_JDK` 不是 JDK 安装目录（是 `lib/modules` / `img/`）：
            // **不要把它当 java.home 用**，继续往下探测真正的安装目录。
        }
    }
    for cand in [
        r"C:\Program Files\Eclipse Adoptium\jdk-25.0.4.101-hotspot",
        r"C:\Program Files\Java\jdk-25",
        r"C:\Program Files\Java\jdk-21",
    ] {
        if std::path::Path::new(cand).is_dir() {
            return cand.to_string();
        }
    }
    // 最后兜底：环境里的 JAVA_HOME
    if let Ok(h) = std::env::var("JAVA_HOME") {
        if !h.trim().is_empty() && std::path::Path::new(h.trim()).is_dir() {
            return h.trim().to_string();
        }
    }
    ".".to_string()
}

fn env_or(name: &str, alt: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| alt.to_string())
}

fn cwd() -> String {
    std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| ".".into())
}

/// `platformProperties()`：**位置**固定，见 `SystemProps$Raw` 的 `_xxx_NDX` 常量。
fn platform_props() -> Vec<Option<String>> {
    let win = cfg!(windows);
    let mut v: Vec<Option<String>> = vec![None; 40];
    let mut set = |i: usize, s: String| v[i] = Some(s);
    // 0..3 display.*，5..8 format.*（本地化信息没有就用空串）
    for i in [0usize, 1, 2, 3, 5, 6, 7, 8] {
        set(i, String::new());
    }
    set(4, if win { "\\".into() } else { "/".into() }); // file.separator
    // 9..16 代理相关：不设（null = 属性不存在），和没配代理的 JVM 一致
    set(17, std::env::temp_dir().to_string_lossy().into_owned()); // java.io.tmpdir
    set(18, if win { "\r\n".into() } else { "\n".into() }); // line.separator
    set(19, if win { "GBK".into() } else { "UTF-8".into() }); // native.encoding
    set(20, if cfg!(target_arch = "x86_64") { "amd64".into() } else { "x86".into() }); // os.arch
    set(
        21,
        if win {
            "Windows 11".into()
        } else if cfg!(target_os = "macos") {
            "Mac OS X".into()
        } else {
            "Linux".into()
        },
    ); // os.name
    set(22, "10.0".into()); // os.version
    set(23, if win { ";".into() } else { ":".into() }); // path.separator
    set(27, "utf-8".into()); // stderr.encoding
    set(28, "utf-8".into()); // stdin.encoding
    set(29, "utf-8".into()); // stdout.encoding
    set(30, String::new()); // sun.arch.abi
    set(31, "64".into()); // sun.arch.data.model
    set(32, "little".into()); // sun.cpu.endian
    set(33, if cfg!(target_arch = "x86_64") { "amd64".into() } else { "x86".into() }); // sun.cpu.isalist
    set(34, "UnicodeLittle".into()); // sun.io.unicode.encoding
    set(35, if win { "GBK".into() } else { "UTF-8".into() }); // sun.jnu.encoding
    set(36, String::new()); // sun.os.patch.level
    set(37, cwd()); // user.dir
    set(38, env_or("USERPROFILE", &env_or("HOME", "."))); // user.home
    set(39, env_or("USERNAME", &env_or("USER", "user"))); // user.name
    v
}

/// `vmProperties()`：键/值成对的列表（`java.home` 等 VM 侧属性 + 命令行 `-D`）
fn vm_props() -> Vec<(String, String)> {
    let mut p: Vec<(String, String)> = Vec::new();
    let mut push = |k: &str, v: String| p.push((k.to_string(), v));
    push("java.home", jdk_home());
    push("java.vendor", "vmless".into());
    push("java.vendor.url", "https://example.invalid/vmless".into());
    push("java.version", "25.0.4".into());
    push("java.version.date", "2026-01-20".into());
    push("java.runtime.name", "vmless Java Runtime".into());
    push("java.runtime.version", "25.0.4".into());
    push("java.vm.name", "vmless".into());
    push("java.vm.version", env!("CARGO_PKG_VERSION").into());
    push("java.vm.vendor", "vmless".into());
    push("java.vm.info", "interpreter".into());
    push("java.specification.name", "Java Platform API Specification".into());
    push("java.specification.vendor", "Oracle Corporation".into());
    push("java.specification.version", "25".into());
    push("java.class.version", "69.0".into());
    push("java.class.path", ".".into());
    push("java.library.path", cwd());
    push("jdk.debug", "release".into());
    push("file.encoding", "UTF-8".into());
    push("stdout.encoding", "utf-8".into());
    push("stderr.encoding", "utf-8".into());
    // 命令行 `-Dk=v` 最后并进来（后写的赢）
    if let Ok(g) = crate::vm::PROPS.lock() {
        for (k, v) in g.iter() {
            p.push((k.clone(), v.clone()));
        }
    }
    p
}

/// 把 `Vec<Option<String>>` 变成 `String[]`（`None` → null）
fn positional_array(vm: &mut Vm, vals: Vec<Option<String>>) -> Slot {
    let mut slots = Vec::with_capacity(vals.len());
    for v in vals {
        slots.push(Slot::I(match v {
            Some(s) => vm.str_new(s),
            None => 0,
        }));
    }
    let h = vm.sh.heap.new_array(slots, "[Ljava/lang/String;").unwrap_or(0);
    Slot::I(h)
}

fn sysprops_platform(vm: &mut Vm, _f: &mut Frame) -> Result<Slot, VmError> {
    Ok(positional_array(vm, platform_props()))
}

fn sysprops_vm(vm: &mut Vm, _f: &mut Frame) -> Result<Slot, VmError> {
    let pairs = vm_props();
    let mut slots = Vec::with_capacity(pairs.len() * 2);
    for (k, v) in pairs {
        let kh = vm.str_new(k);
        let vh = vm.str_new(v);
        slots.push(Slot::I(kh));
        slots.push(Slot::I(vh));
    }
    let h = vm.sh.heap.new_array(slots, "[Ljava/lang/String;").unwrap_or(0);
    Ok(Slot::I(h))
}
