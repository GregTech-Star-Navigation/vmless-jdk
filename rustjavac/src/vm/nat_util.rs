//! native 实现分组：nat_util —— java.base 里「杂项但挡路」的那批 native。
//!
//! 由 `native_reg.rs` 的 `register_all` 末尾调用 `register(r)`。这样几条并行工作线
//! 各改各的文件，不用碰 `native_reg.rs`。
//!
//! 分组：
//!
//! | 组 | 类 | 为什么挡路 |
//! | --- | --- | --- |
//! | A | `jdk.internal.misc.VM` / `java.lang.System` / `Console` / `Signal` | `System.initPhase1`、`setOut`、库名映射 |
//! | B | `java.lang.Thread` 剩余 | `start0` / `getThreads` / `dumpThreads` / 栈轨迹 |
//! | C | `java.util.zip.{Inflater,Deflater,CRC32,Adler32}` | jar / gzip 读取，**自己写的 DEFLATE**（无外部依赖） |
//! | D | `jdk.internal.foreign.abi.fallback.LibFallback` | FFM 兜底链接器（本机没有 libffi） |
//! | E | `sun.util.locale.provider.HostLocaleProviderAdapterImpl` | 本地化数据（日期/数字格式） |
//! | F | `ScopedMemoryAccess` | `ByteBuffer.allocateDirect` 链上的 `<clinit>`（`Unsafe` 内存族归 `nat_intrinsics.rs`） |
//!
//! ## 几条不要踩的
//!
//! 1. **描述符必须与 OpenJDK 一字不差**。错一个字符 = 静默不命中 = 运行时
//!    `UnsatisfiedLinkError`。
//! 2. `Inflater.inflateBytesBytes` 返回的 `long` 是**打包值**，不是长度：
//!    bit0..30 = 消耗输入、bit31..61 = 产出输出、bit62 = finished、bit63 = needDict。
//!    依据是 `Inflater.inflate(byte[],int,int)` 的字节码（`ret & 0x7fffffff` /
//!    `ret >>> 31` / `ret >>> 62` / `ret >>> 63`）；`Deflater` 同形，只是 bit63 表示
//!    「参数设置失败」。
//! 3. 压缩用 **stored 块**（不压缩）—— 合法 DEFLATE，压缩率 ≈ 1.0004（见 `emit_stored`）。
//!    解压是**完整实现**：stored / 固定哈夫曼 / 动态哈夫曼 + 32K 回溯窗口 + zlib 包装。
//! 4. 本 VM 的 `println` 是 `exec.rs:1044` 按**方法名**拦截的（不经 `PrintStream`），
//!    所以 `setOut0` 只能把静态字段换掉，重定向后的流不会被用到 —— 见 [`system_set_out`]。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

// 共享的「假」native 内存 arena（全 VM 一份，nat_intrinsics / nat_os 共用）。
// 在这里声明而不是 vm/mod.rs：那两处是几条并行工作线的公用文件，少改一行就少一次冲突。
#[path = "nat_mem.rs"]
pub(crate) mod nat_mem;

use super::NativeRegistry;
use super::super::frame::Frame;
use super::super::shared::lk;
use super::super::slot::Slot;
use super::super::{Vm, VmError};

pub(super) fn register(r: &mut NativeRegistry) {
    group_a_vm_system(r);
    group_b_thread(r);
    group_c_zip(r);
    group_d_libfallback(r);
    group_e_host_locale(r);
    group_f_raw_memory(r);
}

// ---------------------------------------------------------------- 公共小工具

/// 调试用：`VMLESS_NAT_TRACE=1` 时把 nat_util 的关键节点打到 stderr
fn trace() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VMLESS_NAT_TRACE").is_some())
}

fn void() -> Result<Slot, VmError> {
    Ok(Slot::I(0))
}

fn no_op(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    void()
}

fn zi(v: i32) -> Result<Slot, VmError> {
    Ok(Slot::I(v))
}

/// 读一个 `String` 参数（`str_new` 造的句柄与真 String 对象都认）
fn arg_text(vm: &mut Vm, f: &Frame, i: usize) -> String {
    let h = f.arg_i(i);
    vm.value_text(h).unwrap_or_default()
}

fn new_text(vm: &mut Vm, s: impl Into<String>) -> i32 {
    vm.str_new(s.into())
}

/// 造一个 `String[]`
fn text_array(vm: &mut Vm, items: &[String]) -> i32 {
    let mut slots = Vec::with_capacity(items.len());
    for it in items {
        slots.push(Slot::I(new_text(vm, it.clone())));
    }
    vm.sh.heap.new_array(slots, "[Ljava/lang/String;").unwrap_or(0)
}

/// 读 `byte[]` 的一段。元素按 0..255 归一（`write_mem` 也是这个约定）。
fn read_bytes(vm: &Vm, h: i32, off: usize, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(vm.sh.heap.get(h, off + i).map(|s| s.as_i32() as u8).unwrap_or(0));
    }
    out
}

/// 往 `byte[]` 的一段写
fn write_bytes(vm: &mut Vm, h: i32, off: usize, data: &[u8]) {
    for (k, b) in data.iter().enumerate() {
        vm.sh.heap.set(h, off + k, Slot::I(*b as i32));
    }
}

// ================================================================ A 组：VM / System

fn group_a_vm_system(r: &mut NativeRegistry) {
    let sys = "java/lang/System";
    // 这三个是 `System.setOut/setErr/setIn` 的落地：HotSpot 里它们才是**真写静态字段**
    // 的那一步（`System.setOut` 自身只有一句 `setOut0(out)`）。注册成 no-op 会让
    // 重定向彻底失效。
    r.add(sys, "setOut0", "(Ljava/io/PrintStream;)V", system_set_out);
    r.add(sys, "setErr0", "(Ljava/io/PrintStream;)V", system_set_err);
    r.add(sys, "setIn0", "(Ljava/io/InputStream;)V", system_set_in);
    // `System.mapLibraryName("foo")` → `foo.dll`
    r.add(sys, "mapLibraryName", "(Ljava/lang/String;)Ljava/lang/String;", system_map_library_name);

    let vm = "jdk/internal/misc/VM";
    // 没有用户自定义类加载器的调用栈 → 与 HotSpot「只有引导帧」时一样返回 null
    r.add(vm, "latestUserDefinedLoader0", "()Ljava/lang/ClassLoader;", |_, _| Ok(Slot::I(0)));
    // uid 一族：Windows 上 HotSpot 返回 0（没有 getuid）
    r.add(vm, "getuid", "()J", |_, _| Ok(Slot::L(0)));
    r.add(vm, "geteuid", "()J", |_, _| Ok(Slot::L(0)));
    r.add(vm, "getgid", "()J", |_, _| Ok(Slot::L(0)));
    r.add(vm, "getegid", "()J", |_, _| Ok(Slot::L(0)));
    // 启动参数：我们没有 JVM 参数（`-D` 走属性表），给空数组而不是 null
    r.add(vm, "getRuntimeArguments", "()[Ljava/lang/String;", |vm, _| {
        Ok(Slot::I(vm.sh.heap.new_array(Vec::new(), "[Ljava/lang/String;").unwrap_or(0)))
    });

    // ---- jdk.internal.reflect.Reflection.getCallerClass ----
    //
    // 本 VM 没有调用栈，`native_reg.rs` 里给的是 `java/lang/Object`。这会在
    // `ClassLoader.registerAsParallelCapable()` 处炸：它要求「调用者是 ClassLoader 的子类」，
    // 拿 Object 就抛 IllegalCallerException —— `InnocuousThread` / `Cleaner` /
    // `ProcessHandleImpl` 的静态初始化全都要先过这一关。
    //
    // 改成返回 `java/lang/ClassLoader`：它是引导类（loader 为 null），
    // 因此 `Class.forName` 一类「按调用者取加载器」的语义与原来一致（都落到引导加载器），
    // 而 `isAssignableFrom` 检查能过。没有栈的 VM 只能这么近似，宁可比真实调用链更宽松。
    r.add("jdk/internal/reflect/Reflection", "getCallerClass", "()Ljava/lang/Class;", get_caller_class);
    // ---- java.lang.NullPointerException ----
    //
    // 「有帮助的 NPE 消息」（JDK 14+）：JVM 分析字节码给出「哪个引用是 null」。
    // 我们没有调试信息，返回 null = 「没有额外信息」，getMessage() 照旧返回原始消息。
    // 不接住它，**任何** NPE 都会变成 UnsatisfiedLinkError，把真问题盖掉
    //（实测：ProcessBuilder.start() 里的 NPE 就是这样被伪装成缺 native 的）。
    r.add(
        "java/lang/NullPointerException",
        "getExtendedNPEMessage",
        "()Ljava/lang/String;",
        |_, _| Ok(Slot::I(0)),
    );
    // ---- java.lang.ref.Reference ----
    //
    // 没有 GC 引用处理线程（ReferenceHandler）也没有 pending 列表：我们的引用是
    // 「永不入队」的语义 —— 对 Cleaner / WeakHashMap 都安全（只是拿不到「已被回收」
    // 的通知，不会出错）。
    let rf = "java/lang/ref/Reference";
    r.add(rf, "refersTo0", "(Ljava/lang/Object;)Z", |vm, f| {
        let this = f.this();
        let target = f.arg_i(0);
        let referent = vm.get_named(this, "java/lang/ref/Reference", "referent").as_i32();
        Ok(Slot::I(i32::from(referent == target)))
    });
    r.add(rf, "clear0", "()V", no_op);
    r.add(rf, "hasReferencePendingList", "()Z", |_, _| zi(0));
    r.add(rf, "getAndClearReferencePendingList", "()Ljava/lang/ref/Reference;", |_, _| Ok(Slot::I(0)));
    r.add(rf, "waitForReferencePendingList", "()V", |_, _| {
        // `Reference$ReferenceHandler.run()` 是 `while (true) { processPendingReferences(); }`，
        // 而这个方法是它的第一句 —— Java 侧期望它阻塞到有 pending Reference 为止。
        // 我们从不产生 pending Reference（`hasReferencePendingList` 恒 false），于是三种写法：
        //   · 立刻返回        → handler 线程 **100% CPU 空转** + 疯狂打 safepoint，别的线程被饿死
        //   · 无限期 park     → 它是在持有监视器时进来的，直接把主线程锁死
        //   · **睡一小片再返回** ← 取这个：让出 CPU，也不改变任何锁语义
        std::thread::sleep(std::time::Duration::from_millis(1));
        Ok(Slot::I(0))
    });
    // ---- java.lang.VirtualThread ----
    // 本 VM 没有虚拟线程调度器（也就没有 JVMTI 与 continuation），这些全是
    // 「告诉 JVMTI 线程挂载/卸载」的通知口 —— 没有 JVMTI 时注册成空实现是正确语义，
    // 不是偷懒：HotSpot 里没有 JVMTI 时这些 native 也只做一次判空就返回。
    let vt = "java/lang/VirtualThread";
    r.add(vt, "postPinnedEvent", "(Ljava/lang/String;)V", no_op);
    r.add(vt, "notifyJvmtiStart", "()V", no_op);
    r.add(vt, "notifyJvmtiEnd", "()V", no_op);
    r.add(vt, "notifyJvmtiMount", "(Z)V", no_op);
    r.add(vt, "notifyJvmtiUnmount", "(Z)V", no_op);
    r.add(vt, "notifyJvmtiDisableSuspend", "(Z)V", no_op);
    // 没有调度器 → 没有「待解除阻塞的虚拟线程」
    r.add(vt, "takeVirtualThreadListToUnblock", "()Ljava/lang/VirtualThread;", |_, _| Ok(Slot::I(0)));

    // ---- 内置库（libzip / libjava）----
    //
    // `ZipUtils.<clinit>` 第一件事是 `BootLoader.loadLibrary("zip")`，而 HotSpot 的
    // `findBuiltinLib` 会对 java/zip/nio 这类**静态链进 libjava 的库**返回原名
    // （= 「这是内置库，不要去文件系统找 .dll」）。我们所有 zip native 都是自己实现的，
    // 所以正解是：认下「内置」标记，不 dlopen。
    r.add(
        "jdk/internal/loader/NativeLibraries",
        "findBuiltinLib",
        "(Ljava/lang/String;)Ljava/lang/String;",
        find_builtin_lib,
    );
    // 内置库的「装载」= 记一个假句柄后返回成功；非内置库交回 jni_lib 的真实现
    r.add(
        "jdk/internal/loader/NativeLibraries",
        "load",
        "(Ljdk/internal/loader/NativeLibraries$NativeLibraryImpl;Ljava/lang/String;ZZ)Z",
        natives_load,
    );

    // `jdk.internal.loader.BootLoader`：`ZipUtils.loadLibrary()`（zip 各类的 <clinit>）会走到
    // `BootLoader.loadLibrary`，它要先把「引导加载器的未命名模块」挂上。我们没有模块系统 → no-op。
    // 真写静态字段 `UNNAMED_MODULE`（HotSpot 的 native 也是这么干的）：
    // 不写的话 `BootLoader.getUnnamedModule()` 返回 null，后面 `synchronized (module)` 直接 NPE。
    r.add("jdk/internal/loader/BootLoader", "setBootLoaderUnnamedModule0", "(Ljava/lang/Module;)V", |vm, f| {
        let m = f.arg_i(0);
        lk(&vm.sh.statics)
            .insert(("jdk/internal/loader/BootLoader".to_string(), "UNNAMED_MODULE".to_string()), Slot::I(m));
        void()
    });
    // 引导模块里的包名表：我们没有 JPMS 运行时镜像 → 空数组 / null（都表示「找不到」）
    r.add("jdk/internal/loader/BootLoader", "getSystemPackageNames", "()[Ljava/lang/String;", |vm, _| {
        Ok(Slot::I(vm.sh.heap.new_array(Vec::new(), "[Ljava/lang/String;").unwrap_or(0)))
    });
    r.add("jdk/internal/loader/BootLoader", "getSystemPackageLocation", "(Ljava/lang/String;)Ljava/lang/String;", |_, _| {
        Ok(Slot::I(0))
    });

    // **启动早期把 `java.nio.Buffer` 初始化掉**。
    //
    // 为什么：`java.util.zip.ZipUtils.<clinit>` 第一句就是
    // `SharedSecrets.getJavaNioAccess()`，而 SharedSecrets 在该接口对象为 null 时会走
    // `ensureClassInitialized(Buffer.class)` → `MethodHandles.lookup().ensureInitialized(...)`，
    // 这条 **indy 接管里没有实现**（`src/vm/indy.rs:1643`，不在本文件范围内）。
    // 只要 Buffer 先初始化过，`setJavaNioAccess` 就把对象填上了，那条路不会再走。
    // 触发点选在 `VM.initialize()`（`VM.<clinit>` 里调用，早于任何 zip 类）。
    r.add("jdk/internal/misc/VM", "initialize", "()V", vm_initialize_with_nio);

    // `Console.ttyStatus()`：不是 tty → 0（与「输出被重定向」的 JVM 一致）
    r.add("java/io/Console", "ttyStatus", "()I", |_, _| zi(0));

    // `jdk.internal.misc.Signal`
    r.add("jdk/internal/misc/Signal", "findSignal0", "(Ljava/lang/String;)I", signal_find);
    // 「旧处理器」：0 = 之前没装过
    r.add("jdk/internal/misc/Signal", "handle0", "(IJ)J", |_, _| Ok(Slot::L(0)));
    // 不真发信号：真 raise 会把宿主进程带走
    r.add("jdk/internal/misc/Signal", "raise0", "(I)V", no_op);
}

/// `System.setOut0(PrintStream)`：**真写静态字段** `System.out`。
///
/// 局限（不在本文件能修的范围内）：`exec.rs:1044` 把 `println` 按**方法名**拦截成
/// `Vm::println`，不看接收者，所以 `System.setOut(x); System.out.println(..)` 仍然
/// 打到宿主 stdout。字段本身是真换掉了 —— `System.out == x` 成立。
/// `VM.initialize()`：与 `native_reg.rs` 的同名实现同样的两件事，
/// 外加「提前把 `java.nio.Buffer` 初始化掉」（理由见 `group_a_vm_system` 里那段注释）。
/// 本文件在 `register_all` 末尾注册，所以这条会**覆盖**前面的同名项。
fn vm_initialize_with_nio(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    // `VM.getSavedProperty` 在 `savedProps == null` 时直接抛 IllegalStateException，
    // 而 `Integer.IntegerCache.<clinit>` 就读它 → 必须真建一个空 Map
    let map = vm.new_instance("java/util/HashMap", "()V")?;
    lk(&vm.sh.statics).insert(("jdk/internal/misc/VM".to_string(), "savedProps".to_string()), Slot::I(map));
    // `java.lang.System.lineSeparator` 是 `initPhase1` 赋值的静态字段；我们没有那段启动流程，
    // 字段永远是 null，而 `System.lineSeparator()` 读的就是它 → 这里按平台属性补上。
    // 此刻 System 还没初始化（属性表是它 <clinit> 里建的），所以不能读属性，直接按平台给
    let sep: &str = if cfg!(windows) { "\r\n" } else { "\n" };
    let sep_h = vm.str_new(sep.to_string());
    lk(&vm.sh.statics)
        .insert(("java/lang/System".to_string(), "lineSeparator".to_string()), Slot::I(sep_h));
    // Buffer.<clinit> 会 setJavaNioAccess；失败也不该拖死 VM.initialize（zip 用不了而其它照跑）
    let r = vm.ensure_init("java/nio/Buffer");
    if trace() {
        eprintln!("[nat_util] VM.initialize: Buffer 预初始化 = {:?}", r.is_ok());
    }
    void()
}

/// 内置库名单：这些库的符号由 VM 自己提供（HotSpot 里它们静态链进 libjava）
fn find_builtin_lib(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let name = arg_text(vm, f, 0);
    if trace() {
        eprintln!("[nat_util] findBuiltinLib({name})");
    }
    let base = std::path::Path::new(&name)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.clone());
    let stem = base.trim_end_matches(".dll").trim_end_matches(".so").trim_end_matches(".dylib");
    let builtin = matches!(stem, "zip" | "java" | "nio" | "jimage" | "net" | "extnet" | "management");
    if builtin {
        let h = new_text(vm, name);
        Ok(Slot::I(h))
    } else {
        Ok(Slot::I(0))
    }
}

/// `NativeLibraries.load(impl, name, isBuiltin, throwIfFail)`：
/// 内置库给假句柄就返回 true（符号由本注册表提供）；其它库转给 `jni_lib` 的真 dlopen。
fn natives_load(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let impl_obj = f.arg_i(0);
    if trace() {
        eprintln!("[nat_util] NativeLibraries.load builtin={}", f.arg_i(2) != 0);
    }
    let is_builtin = f.arg_i(2) != 0;
    if !is_builtin {
        return super::jni_lib::native_libraries_load(vm, f);
    }
    vm.put_named(
        impl_obj,
        "jdk/internal/loader/NativeLibraries$NativeLibraryImpl",
        "handle",
        Slot::L(0x1000_0000),
    );
    Ok(Slot::I(1))
}

/// 没有调用栈时 `Reflection.getCallerClass()` 的替身。
///
/// 为什么需要：`ClassLoader.registerAsParallelCapable()` 会检查「调用者是不是
/// ClassLoader 的子类」，而且每个类**只能注册一次**（第二次返回 false →
/// InternalError("Unable to register as parallel capable")）。
/// 原来这里一律返回 `java/lang/Object` → IllegalCallerException；
/// 一律返回 `java/lang/ClassLoader` → 第一次能过、第二次就 InternalError。
///
/// 所以按**轮转**返回 java.base 里真实存在的 ClassLoader 子类：每个类各注册一次，
/// 都不会撞车。这是近似 —— 真正的调用者类只有解释器知道；
/// 但 @CallerSensitive 的检查只需要「一个合法的 ClassLoader 子类」。
fn get_caller_class(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    // 本 VM 没有调用栈，但有一个**等价信息**：`sh.initializing`（哪个线程正在初始化哪个类）。
    // 真实调用点绝大多数都在 `@CallerSensitive` 方法的调用者 `<clinit>` 里：
    //
    // * `ClassLoader.registerAsParallelCapable()`：要求「调用者是 ClassLoader 的子类」，
    //   拿错就是 IllegalCallerException / InternalError —— Cleaner、InnocuousThread 都过不去；
    // * `MethodHandles.lookup()`：会把 caller 记进 `indy().lookups`，而
    //   `MhUtil.findVarHandle(lookup, name, type)` 就是 `lookup.findVarHandle(lookup.lookupClass(), ...)`。
    //   调用者记错 → lookupClass 错的 → findVarHandle 报「第一个参数不是 Class」，
    //   实测卡死 ThreadBuilders$BaseThreadFactory / ProcessHandleImpl / ProcessBuilder。
    //
    // 所以：**当前线程正在初始化的类**就是最可能的调用者。恰好一个时直接用它；
    // 多个或没有时退回轮转（保证 registerAsParallelCapable 至少拿到一个合法的 ClassLoader 子类）。
    let me = super::super::thread::cur_tid();
    let in_init: Vec<String> = lk(&vm.sh.initializing)
        .iter()
        .filter(|(_, tid)| **tid == me)
        .map(|(name, _)| name.clone())
        .collect();
    if in_init.len() == 1 {
        return Ok(Slot::I(vm.class_obj(&in_init[0])));
    }
    // 兜底：按继承链从浅到深挑一个「父类已登记」的 ClassLoader 子类
    const CANDIDATES: [&str; 5] = [
        "java/security/SecureClassLoader",
        "jdk/internal/loader/BuiltinClassLoader",
        "jdk/internal/loader/ClassLoaders$BootClassLoader",
        "jdk/internal/loader/ClassLoaders$AppClassLoader",
        "jdk/internal/loader/ClassLoaders$PlatformClassLoader",
    ];
    static DONE: [AtomicBool; 5] = [
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
    ];
    let mut pick = CANDIDATES.len() - 1;
    for i in 0..CANDIDATES.len() {
        let parent_ok = i == 0 || DONE[i - 1].load(Ordering::Relaxed);
        if parent_ok && !DONE[i].load(Ordering::Relaxed) {
            DONE[i].store(true, Ordering::Relaxed);
            pick = i;
            break;
        }
    }
    Ok(Slot::I(vm.class_obj(CANDIDATES[pick])))
}

fn system_set_out(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.arg_i(0);
    lk(&vm.sh.statics).insert(("java/lang/System".to_string(), "out".to_string()), Slot::I(h));
    void()
}

fn system_set_err(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.arg_i(0);
    lk(&vm.sh.statics).insert(("java/lang/System".to_string(), "err".to_string()), Slot::I(h));
    void()
}

fn system_set_in(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.arg_i(0);
    lk(&vm.sh.statics).insert(("java/lang/System".to_string(), "in".to_string()), Slot::I(h));
    void()
}

/// `System.mapLibraryName(String)`：Windows 加 `.dll`，Linux 加 `lib*.so`，macOS 加 `lib*.dylib`。
fn system_map_library_name(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let name = arg_text(vm, f, 0);
    let lower = name.to_ascii_lowercase();
    let mapped = if lower.ends_with(".dll") || lower.ends_with(".so") || lower.ends_with(".dylib") {
        name
    } else if cfg!(windows) {
        format!("{name}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{name}.dylib")
    } else {
        format!("lib{name}.so")
    };
    let h = new_text(vm, mapped);
    Ok(Slot::I(h))
}

/// `Signal.findSignal0(String)`：HotSpot（Windows）认 INT/TERM/BREAK/HUP，其余 -1，
/// `Signal.handle` 拿到 -1 会抛 IllegalArgumentException。
fn signal_find(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let name = arg_text(vm, f, 0);
    zi(signal_number(&name))
}

fn signal_number(s: &str) -> i32 {
    match s {
        "INT" => 2,
        "TERM" => 15,
        "BREAK" => 21,
        "HUP" => 1,
        "ABRT" => 22,
        "SEGV" => 11,
        _ => -1,
    }
}

// ================================================================ B 组：Thread 剩余

fn group_b_thread(r: &mut NativeRegistry) {
    let t = "java/lang/Thread";
    // `Thread.start()` 被 VM 接管（`native_reg.rs` 的 `thread_start_override`），
    // 正常路径到不了这里；但真 java.base 里 `start0` 是 native，注册上才不会在
    // 「有人绕过 start() 直接调」时 UnsatisfiedLinkError。
    // currentThread / start 都要「补齐字段」后再交给 VM（见 ensure_thread_fields）
    r.add(t, "currentThread", "()Ljava/lang/Thread;", thread_current_filled);
    r.add(t, "currentCarrierThread", "()Ljava/lang/Thread;", thread_current_filled);
    r.add_override(t, "start", "()V", thread_start_filled);

    // start0 只有「绕过 start() 直接调」才会走到    r.add(t, "start0", "()V", thread_start0);
    // 活线程表：我们只有「当前线程」这一条确定信息 → 至少让 `enumerate` 看得到它
    r.add(t, "getThreads", "()[Ljava/lang/Thread;", thread_get_threads);
    // 每个线程的栈轨迹：本 VM 没有调用栈模型 → 给**等长的空数组**
    // （真返回 null 会让 `Thread.getAllStackTraces` 里的 `traces[i]` 直接 NPE）
    r.add(t, "dumpThreads", "([Ljava/lang/Thread;)[[Ljava/lang/StackTraceElement;", thread_dump_threads);
    // 其它线程的轨迹：一条合成帧
    r.add(t, "getStackTrace0", "()Ljava/lang/Object;", thread_stack_trace0);
    // 优先级：Java 侧 `Thread.priority(int)` 在 `holder.group == null` 时**根本不会**调到这里
    // （我们的 Thread 构造器不建 ThreadGroup）。注册上是为了「有实现」，不是让它生效。
    r.add(t, "setPriority0", "(I)V", no_op);
    // 栈轨迹的正门。`Thread.getStackTrace()` 在 JDK 25 里是普通方法（不是 native）：
    // 当前线程走 `new Exception().getStackTrace()` → 我们的 `Throwable` 没有栈 → 空数组。
    // 用 VM 接管把它换成「一条合成帧」，于是 `getStackTrace()` 非空。
    r.add_override(t, "getStackTrace", "()[Ljava/lang/StackTraceElement;", thread_get_stack_trace);
    // 线程 id：JDK 的 tid 由 ThreadIdentifiers.next()（Unsafe.getAndAddLong(null, 偏移, 1)）发，
    // 而我们的 Thread 构造器是 VM 接管的 —— 那段 Java 赋值不跑 → 所有线程的 tid 都是 0。
    // 这里按对象句柄首次查询时发一个单调 id（够 threadId() 的唯一性契约）。
    r.add_override(t, "threadId", "()J", thread_id_override);
}

/// Thread.currentThread()：拿到的对象要**补齐所有该非 null 的引用字段**
/// （group / contextClassLoader / interruptLock）。
///
/// 为什么在 native 里补而不是构造器里：本 VM 的 Thread 构造器是 VM 接管的
/// （native_reg.rs 的 thread_ctor_*），主线程对象更是 Vm::current_thread_obj()
/// 直接造的 —— 两条路都不跑 JDK 的字段初始化。缺 contextClassLoader 会让
/// ServiceLoader / InnocuousThread 一类代码直接 NPE；缺 group 会让
/// synchronized (threadGroup) 炸（Cleaner / ProcessHandle 都踩过）。
fn thread_current_filled(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    let h = vm.current_thread_obj();
    ensure_thread_fields(vm, h);
    if trace() {
        eprintln!("[nat_util] currentThread -> {h}");
    }
    Ok(Slot::I(h))
}

/// Thread.start()：接管后先补齐字段再起线程（参数语义与 native_reg 的同名接管一致）
fn thread_start_filled(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.this();
    ensure_thread_fields(vm, h);
    if trace() {
        eprintln!("[nat_util] Thread.start {h}");
    }
    vm.thread_start(h)?;
    void()
}

/// 把 Thread（及其 FieldHolder）上**必须非 null** 的引用字段补成合法对象。
///
/// 只补「JDK 自己一定会初始化、而我们跳过了那一步」的字段；
/// **threadLocals 故意留 null** —— ThreadLocal.get() 对 null map 会走
/// createMap 懒建（这也是 HotSpot 的正常路径），塞一个空壳 map 反而会让
/// getEntry 读到 null 的 table 而 NPE。
/// 补字段期间的重入闸（见 ensure_thread_fields 的注释）
static FILLING_FIELDS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 退出时清闸
struct FillGuard;

impl Drop for FillGuard {
    fn drop(&mut self) {
        FILLING_FIELDS.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

fn ensure_thread_fields(vm: &mut Vm, h: i32) {
    if h <= 0 {
        return;
    }
    // 重入闸：补 contextClassLoader 要跑 Java 构造器，而构造器里又会调
    // Thread.currentThread() → 回到本函数。没有这道闸就是无限递归
    //（实测表现：currentThread 被疯狂调用，看着像死循环）。重入时直接返回，
    // 外层那一趟会把字段补完。
    if FILLING_FIELDS.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let _guard = FillGuard;
    // 1) holder.group：getThreadGroup() 直接返回它，Cleaner / InnocuousThread 会拿它去 synchronized
    // 根 ThreadGroup 可能是 thread.rs 先建的（那边没设 maxPriority）→ 这里兜底补一次
    let g0 = root_group(vm);
    if g0 > 0 && vm.get_named(g0, "java/lang/ThreadGroup", "maxPriority").as_i32() <= 0 {
        vm.put_named(g0, "java/lang/ThreadGroup", "maxPriority", Slot::I(10));
    }
    let holder = vm.get_named(h, "java/lang/Thread", "holder").as_i32();
    if holder > 0 {
        let grp = vm.get_named(holder, "java/lang/Thread$FieldHolder", "group").as_i32();
        if grp <= 0 {
            let g = root_group(vm);
            if g > 0 {
                vm.put_named(holder, "java/lang/Thread$FieldHolder", "group", Slot::I(g));
            }
        }
    }
    // 2) contextClassLoader：ServiceLoader / InnocuousThread 会直接用它
    let ccl = vm.get_named(h, "java/lang/Thread", "contextClassLoader").as_i32();
    if ccl <= 0 {
        if let Some(l) = some_class_loader(vm) {
            vm.put_named(h, "java/lang/Thread", "contextClassLoader", Slot::I(l));
        }
    }
    // 3) interruptLock：interrupt() 里 synchronized (interruptLock)
    let il = vm.get_named(h, "java/lang/Thread", "interruptLock").as_i32();
    if il <= 0 {
        let o = vm.new_instance("java/lang/Object", "()V").unwrap_or(0);
        vm.put_named(h, "java/lang/Thread", "interruptLock", Slot::I(o));
    }
}

/// 全局共享的根 ThreadGroup（缓存进静态字段表，与 thread.rs 的 root_thread_group 同值域）
fn root_group(vm: &mut Vm) -> i32 {
    let key = ("java/lang/ThreadGroup".to_string(), "$vmless_root".to_string());
    if let Some(Slot::I(h)) = lk(&vm.sh.statics).get(&key).copied() {
        if h > 0 {
            return h;
        }
    }
    if !vm.store.has_class("java/lang/ThreadGroup") {
        return 0;
    }
    let h = vm.alloc_vmobj("java/lang/ThreadGroup");
    if h <= 0 {
        return 0;
    }
    let nm = vm.str_new("main".to_string());
    let _ = vm.put_named(h, "java/lang/ThreadGroup", "name", Slot::I(nm));
    let _ = vm.put_named(h, "java/lang/ThreadGroup", "parent", Slot::I(h));
    // maxPriority 必须是 10：Thread.priority(int) 会拿它做上限裁剪，
    // 默认 0 会把 setPriority 的结果裁成 0（表现为 getPriority() 永远是 0）
    let _ = vm.put_named(h, "java/lang/ThreadGroup", "maxPriority", Slot::I(10));
    let _ = vm.put_named(h, "java/lang/ThreadGroup", "nthreads", Slot::I(1));
    lk(&vm.sh.statics).insert(key, Slot::I(h));
    h
}

/// 造一个可用的 ClassLoader 实例给 contextClassLoader 用。
///
/// 优先真 ClassLoaders$BootClassLoader（它的构造器是空的）；造不出来就退化成
/// 一个 java/lang/ClassLoader 的裸对象 —— 后者只保证「非 null 且能当锁 / 能比身份」，
/// 不能真去 getResource。
fn some_class_loader(vm: &mut Vm) -> Option<i32> {
    fn cache() -> &'static Mutex<HashMap<u8, i32>> {
        static C: OnceLock<Mutex<HashMap<u8, i32>>> = OnceLock::new();
        C.get_or_init(|| Mutex::new(HashMap::new()))
    }
    if let Some(v) = cache().lock().ok().and_then(|g| g.get(&0u8).copied()) {
        if v > 0 {
            return Some(v);
        }
    }
    let made = vm
        .new_instance("jdk/internal/loader/ClassLoaders$BootClassLoader", "()V")
        .ok()
        .filter(|h| *h > 0)
        .or_else(|| {
            if vm.store.has_class("java/lang/ClassLoader") {
                Some(vm.alloc_vmobj("java/lang/ClassLoader"))
            } else {
                None
            }
        });
    match made {
        Some(h) if h > 0 => {
            let m = cache();
            let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
            g.insert(0u8, h);
            Some(h)
        }
        _ => None,
    }
}

fn thread_start0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    vm.thread_start(this)?;
    void()
}

fn thread_get_threads(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    let cur = vm.current_thread_obj();
    let h = vm.sh.heap.new_array(vec![Slot::I(cur)], "[Ljava/lang/Thread;").unwrap_or(0);
    Ok(Slot::I(h))
}

fn thread_dump_threads(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let arr = f.arg_i(0);
    let n = vm.sh.heap.array_len(arr).unwrap_or(0);
    let mut slots = Vec::with_capacity(n);
    for _ in 0..n {
        let inner = vm.sh.heap.new_array(Vec::new(), "[Ljava/lang/StackTraceElement;").unwrap_or(0);
        slots.push(Slot::I(inner));
    }
    let h = vm.sh.heap.new_array(slots, "[[Ljava/lang/StackTraceElement;").unwrap_or(0);
    Ok(Slot::I(h))
}

fn thread_stack_trace0(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::I(fake_stack_trace(vm)))
}

fn thread_id_override(_vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let this = f.this();
    let mut g = thread_ids().lock().unwrap_or_else(|e| e.into_inner());
    let id = *g.entry(this).or_insert_with(|| NEXT.fetch_add(1, Ordering::Relaxed));
    Ok(Slot::L(id as i64))
}

fn thread_ids() -> &'static Mutex<HashMap<i32, u64>> {
    static M: OnceLock<Mutex<HashMap<i32, u64>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

fn thread_get_stack_trace(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::I(fake_stack_trace(vm)))
}

/// 造一个「一条帧」的 `StackTraceElement[]`。
///
/// 本 VM 没有调用栈可枚举，所以帧内容固定成 `java.lang.Object.<clinit>`（Object.java, -1）：
/// 只保证**非空、可读**，不假装有真实行号。
fn fake_stack_trace(vm: &mut Vm) -> i32 {
    let mut one = Vec::with_capacity(1);
    if let Some(h) = make_stack_trace_element(vm) {
        one.push(Slot::I(h));
    }
    vm.sh.heap.new_array(one, "[Ljava/lang/StackTraceElement;").unwrap_or(0)
}

fn make_stack_trace_element(vm: &mut Vm) -> Option<i32> {
    let cls = new_text(vm, "java.lang.Object");
    let method = new_text(vm, "<clinit>");
    let file = new_text(vm, "Object.java");
    // (String classLoaderName, String moduleName, String moduleVersion,
    //  String declaringClass, String methodName, String fileName, int lineNumber)
    let desc = "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;I)V";
    let args = [Slot::I(0), Slot::I(0), Slot::I(0), Slot::I(cls), Slot::I(method), Slot::I(file), Slot::I(-1)];
    vm.new_instance_args("java/lang/StackTraceElement", desc, &args).ok()
}

// ================================================================ C 组：java.util.zip
//
// 没有外部依赖（不许引 flate2），所以 DEFLATE 自己写：
//
// * **解压**：完整实现 —— stored / 固定哈夫曼 / 动态哈夫曼，32K 回溯窗口，
//   zlib 包装（CMF/FLG + adler32 尾）与 raw（nowrap）两种。
// * **压缩**：只用 **stored 块**（BTYPE=00）。合法 DEFLATE，任何 inflater 都认，
//   代价是压缩率 ≈ 1.0004（2033 字节的文本压完 2049 字节），而且
//   `Deflater(默认)` 与 `Deflater(9)` 产出的字节**完全一样** —— 级别只影响压缩率，
//   不影响流的合法性。

fn group_c_zip(r: &mut NativeRegistry) {
    let inf = "java/util/zip/Inflater";
    r.add(inf, "initIDs", "()V", no_op);
    r.add(inf, "init", "(Z)J", z_init_inflater);
    r.add(inf, "setDictionary", "(J[BII)V", z_inflater_set_dict);
    r.add(inf, "setDictionaryBuffer", "(JJI)V", z_unsupported_buffer);
    r.add(inf, "inflateBytesBytes", "(J[BII[BII)J", z_inflate_bytes_bytes);
    r.add(inf, "inflateBytesBuffer", "(J[BIIJI)J", z_unsupported_buffer);
    r.add(inf, "inflateBufferBytes", "(JJI[BII)J", z_unsupported_buffer);
    r.add(inf, "inflateBufferBuffer", "(JJIJI)J", z_unsupported_buffer);
    r.add(inf, "getAdler", "(J)I", z_inflater_adler);
    r.add(inf, "reset", "(J)V", z_inflater_reset);
    r.add(inf, "end", "(J)V", z_free);

    let def = "java/util/zip/Deflater";
    r.add(def, "init", "(IIZ)J", z_init_deflater);
    r.add(def, "setDictionary", "(J[BII)V", z_deflater_set_dict);
    r.add(def, "setDictionaryBuffer", "(JJI)V", z_unsupported_buffer);
    r.add(def, "deflateBytesBytes", "(J[BII[BIIII)J", z_deflate_bytes_bytes);
    r.add(def, "deflateBytesBuffer", "(J[BIIJIII)J", z_unsupported_buffer);
    r.add(def, "deflateBufferBytes", "(JJI[BIIII)J", z_unsupported_buffer);
    r.add(def, "deflateBufferBuffer", "(JJIJIII)J", z_unsupported_buffer);
    r.add(def, "getAdler", "(J)I", z_deflater_adler);
    r.add(def, "reset", "(J)V", z_deflater_reset);
    r.add(def, "end", "(J)V", z_free);

    // 校验和：GZIP 的头/尾（CRC32）与 zlib 的尾（Adler32）都靠它们
    let c = "java/util/zip/CRC32";
    r.add(c, "update", "(II)I", crc32_update_byte);
    r.add(c, "updateBytes0", "(I[BII)I", crc32_update_bytes);
    r.add(c, "updateByteBuffer0", "(IJII)I", z_unsupported_buffer);
    let a = "java/util/zip/Adler32";
    r.add(a, "update", "(II)I", adler32_update_byte);
    r.add(a, "updateBytes", "(I[BII)I", adler32_update_bytes);
    r.add(a, "updateBuffer", "(IJI)I", z_unsupported_buffer);
}

/// `Inflater` / `Deflater` 的 native 状态。Java 侧的 `long` 地址 → 这里的一条。
///
/// 为什么不用真指针：Java 只把它当不透明句柄（`address() != 0` 判「还开着」），
/// 用 `HashMap` 存就不会有 use-after-free。
struct ZStream {
    inf: Option<Inflate>,
    def: Option<Deflate>,
    adler: u32,
}

struct Inflate {
    nowrap: bool,
    /// zlib 头还没读
    need_head: bool,
    /// 已经读到 BFINAL 块
    last_block: bool,
    /// 整条流解完（含 zlib 尾）
    finished: bool,
    phase: Phase,
    /// 位读取器残留（跨调用）：buf 用 u64 存到 56 位
    bitbuf: u64,
    bitcnt: u32,
    /// stored 块剩余字节（`NONE` = 还没读 LEN/NLEN）
    stored_left: u32,
    lit: Huff,
    dist: Huff,
    /// 回溯窗口（最多 32K）；`setDictionary` 时当成前缀
    win: Vec<u8>,
    /// 跨调用残留：已经解出「长度」但还没读到「距离」的那一次匹配
    /// （位读不够时必须把长度存下来，否则下次会重读长度符号 → 全乱）
    pending_len: Option<usize>,
    /// 跨调用残留：回溯拷贝拷到一半，输出缓冲满了（(距离, 还剩几个字节)）
    pending_copy: Option<(usize, usize)>,
}

const STORED_NONE: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    /// 块头（或 zlib 头）
    Head,
    Stored,
    Codes,
    /// zlib 尾 4 字节 adler
    Tail,
}

#[derive(Clone)]
struct Huff {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huff {
    fn new() -> Huff {
        Huff { counts: [0; 16], symbols: Vec::new() }
    }
    fn empty(&self) -> bool {
        self.symbols.is_empty()
    }
}

struct Deflate {
    nowrap: bool,
    wrote_head: bool,
    /// 攒着还没成块的输入（stored 块最大 65535）
    in_buf: Vec<u8>,
    /// 已生成、还没被调用方取走的压缩字节
    out_pending: Vec<u8>,
    /// 收尾块与 zlib 尾已写
    trailer_done: bool,
    /// 见过 FINISH
    finish: bool,
}

fn zstreams() -> &'static Mutex<HashMap<u64, ZStream>> {
    static Z: OnceLock<Mutex<HashMap<u64, ZStream>>> = OnceLock::new();
    Z.get_or_init(|| Mutex::new(HashMap::new()))
}

fn zlock() -> MutexGuard<'static, HashMap<u64, ZStream>> {
    zstreams().lock().unwrap_or_else(|e| e.into_inner())
}

fn zalloc(s: ZStream) -> i64 {
    static NEXT: AtomicU64 = AtomicU64::new(0x7f00_0000_0001);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    zlock().insert(id, s);
    id as i64
}

fn z_free(_vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    if addr != 0 {
        zlock().remove(&addr);
    }
    void()
}

fn fresh_inflate(nowrap: bool) -> Inflate {
    Inflate {
        nowrap,
        need_head: !nowrap,
        last_block: false,
        finished: false,
        phase: Phase::Head,
        bitbuf: 0,
        bitcnt: 0,
        stored_left: STORED_NONE,
        lit: Huff::new(),
        dist: Huff::new(),
        win: Vec::new(),
        pending_len: None,
        pending_copy: None,
    }
}

fn z_init_inflater(_vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let nowrap = f.arg_i(0) != 0;
    Ok(Slot::L(zalloc(ZStream { inf: Some(fresh_inflate(nowrap)), def: None, adler: 1 })))
}

fn z_init_deflater(_vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let nowrap = f.arg_i(2) != 0;
    let d = Deflate {
        nowrap,
        wrote_head: false,
        in_buf: Vec::new(),
        out_pending: Vec::new(),
        trailer_done: false,
        finish: false,
    };
    Ok(Slot::L(zalloc(ZStream { inf: None, def: Some(d), adler: 1 })))
}

/// `ByteBuffer` / 直接内存变体：本 VM 没有裸内存语义。
/// 明确报错，而不是返回 0 让它当「没解出东西」继续跑 —— 那会安静地产出错误数据。
fn z_unsupported_buffer(_vm: &mut Vm, _f: &mut Frame) -> Result<Slot, VmError> {
    Err(VmError {
        msg: "java.util.zip：本 VM 不支持 ByteBuffer(直接内存) 形态的 inflate/deflate".into(),
        exc: None,
    })
}

fn z_inflater_set_dict(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    let arr = f.arg_i(1);
    let off = f.arg_i(2).max(0) as usize;
    let len = f.arg_i(3).max(0) as usize;
    let bytes = read_bytes(vm, arr, off, len);
    let a = adler32(1, &bytes);
    let mut g = zlock();
    if let Some(s) = g.get_mut(&addr) {
        if let Some(inf) = s.inf.as_mut() {
            // 字典 = 回溯窗口的前缀（raw 模式才有意义）
            let keep = bytes.len().min(32768);
            inf.win = bytes[bytes.len() - keep..].to_vec();
            s.adler = a;
        }
    }
    void()
}

fn z_deflater_set_dict(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    let arr = f.arg_i(1);
    let off = f.arg_i(2).max(0) as usize;
    let len = f.arg_i(3).max(0) as usize;
    let bytes = read_bytes(vm, arr, off, len);
    // stored 块不引用字典，所以「设了但不用」是合法的；为了不静默丢信息，仍然校验流存在
    let _ = bytes;
    let mut g = zlock();
    if let Some(s) = g.get_mut(&addr) {
        if s.def.is_none() {
            drop(g);
            return Err(VmError { msg: "Deflater: address 不是 deflate 流".into(), exc: None });
        }
    }
    void()
}

fn z_inflater_adler(_vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    let g = zlock();
    let v = g.get(&addr).map(|s| s.adler).unwrap_or(1);
    Ok(Slot::I(v as i32))
}

fn z_deflater_adler(_vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    let g = zlock();
    let v = g.get(&addr).map(|s| s.adler).unwrap_or(1);
    Ok(Slot::I(v as i32))
}

fn z_inflater_reset(_vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    let mut g = zlock();
    if let Some(s) = g.get_mut(&addr) {
        let nowrap = s.inf.as_ref().map(|i| i.nowrap).unwrap_or(false);
        s.inf = Some(fresh_inflate(nowrap));
        s.adler = 1;
    }
    void()
}

fn z_deflater_reset(_vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    let mut g = zlock();
    if let Some(s) = g.get_mut(&addr) {
        let nowrap = s.def.as_ref().map(|d| d.nowrap).unwrap_or(false);
        s.def = Some(Deflate {
            nowrap,
            wrote_head: false,
            in_buf: Vec::new(),
            out_pending: Vec::new(),
            trailer_done: false,
            finish: false,
        });
        s.adler = 1;
    }
    void()
}

// ---------------------------------------------------------------- 打包返回

/// `(消耗输入 & 0x7fffffff) | (产出 << 31) | (finished << 62) | (extra << 63)`
fn pack(consumed: usize, produced: usize, finished: bool, extra: bool) -> i64 {
    let c = (consumed as i64) & 0x7fff_ffff;
    let p = ((produced as i64) & 0x7fff_ffff) << 31;
    let f = (finished as i64) << 62;
    let e = (extra as i64) << 63;
    c | p | f | e
}

// ---------------------------------------------------------------- 解压

fn z_inflate_bytes_bytes(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    let in_arr = f.arg_i(1);
    let in_off = f.arg_i(2).max(0) as usize;
    let in_len = f.arg_i(3).max(0) as usize;
    let out_arr = f.arg_i(4);
    let out_off = f.arg_i(5).max(0) as usize;
    let out_len = f.arg_i(6).max(0) as usize;

    let input = read_bytes(vm, in_arr, in_off, in_len);
    let mut out = vec![0u8; out_len];

    let mut g = zlock();
    let s = g.get_mut(&addr).ok_or_else(|| VmError {
        msg: "Inflater: address 无效（未 init 或已 end）".into(),
        exc: None,
    })?;
    let adler0 = s.adler;
    let inf = s.inf.as_mut().ok_or_else(|| VmError {
        msg: "Inflater: address 不是 inflate 流".into(),
        exc: None,
    })?;
    match inflate_run(inf, &input, &mut out) {
        Ok((consumed, produced)) => {
            let a = adler32(adler0, &out[..produced]);
            let finished = inf.finished;
            s.adler = a;
            drop(g);
            if produced > 0 {
                write_bytes(vm, out_arr, out_off, &out[..produced]);
            }
            Ok(Slot::L(pack(consumed, produced, finished, false)))
        }
        Err(ZErr::Bad) => {
            drop(g);
            Err(vm.throw("java/util/zip/DataFormatException"))
        }
        Err(ZErr::NeedDict) => {
            // zlib 头里 FDICT=1：Java 侧靠 bit63 置 needDict 字段
            drop(g);
            Ok(Slot::L(pack(0, 0, false, true)))
        }
    }
}

#[derive(Debug)]
enum ZErr {
    Bad,
    NeedDict,
}

/// 位读取器。**不借用 `Inflate`**（否则解压循环里无法同时改窗口和表）。
/// 残留（`buf`/`cnt`）在循环结束时写回 `Inflate`，下次调用接着用。
///
/// 用 `u64` 而不是 `u32` 缓冲：一个「长度 + 额外位 + 距离 + 额外位」的单位最长
/// 15+5+15+13 = 48 位，必须能一次看全 `u32` 存不下。
struct Bits<'a> {
    input: &'a [u8],
    pos: usize,
    buf: u64,
    cnt: u32,
}

impl<'a> Bits<'a> {
    fn refill(&mut self) {
        while self.cnt <= 56 && self.pos < self.input.len() {
            self.buf |= (self.input[self.pos] as u64) << self.cnt;
            self.pos += 1;
            self.cnt += 8;
        }
    }

    /// 取 `n`（≤32）位。`None` = 输入不够（**不消耗**任何位）
    fn take(&mut self, n: u32) -> Option<u32> {
        if n == 0 {
            return Some(0);
        }
        self.refill();
        if self.cnt < n {
            return None;
        }
        let v = (self.buf & ((1u64 << n) - 1)) as u32;
        self.buf >>= n;
        self.cnt -= n;
        Some(v)
    }

    /// 保证至少 `n` 位可用（不够就返回 false，且不消耗）
    fn has(&mut self, n: u64) -> bool {
        self.refill();
        self.cnt as u64 >= n
    }

    fn consume(&mut self, n: u32) {
        if n == 0 {
            return;
        }
        self.buf >>= n;
        self.cnt -= n;
    }

    /// 丢掉到字节边界的残余位，并把「读进来但没用的整字节」还给 `pos`
    fn byte_align(&mut self) {
        let r = self.cnt % 8;
        if r > 0 {
            self.buf >>= r;
            self.cnt -= r;
        }
        self.rewind_whole_bytes();
    }

    fn rewind_whole_bytes(&mut self) {
        let k = (self.cnt / 8) as usize;
        if k > 0 {
            self.cnt -= 8 * k as u32;
            self.pos = self.pos.saturating_sub(k);
            self.buf = if self.cnt == 0 { 0 } else { self.buf & ((1u64 << self.cnt) - 1) };
        }
    }

    /// 按字节读（调用点必须先 `byte_align`）
    fn raw(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.pos + n > self.input.len() {
            return None;
        }
        let s = &self.input[self.pos..self.pos + n];
        self.pos += n;
        Some(s)
    }

    /// 收工：把没用完的整字节还回输入，返回「本次消耗了多少输入」
    fn finish(&mut self) -> usize {
        self.rewind_whole_bytes();
        self.pos
    }
}

enum Dec {
    Sym(u16),
    NeedInput,
    Bad,
}

/// 解码结果（**不消耗位**）：符号 + 码长
enum Peek {
    Sym(u16, u32),
    NeedInput,
    Bad,
}

/// 规范哈夫曼解码（puff 的 counts/symbols 算法）：**只看不取**。
///
/// 必须不取：跨调用时位可能不够解完一个符号，而「解了一半」的位状态是没法存的
/// （这正是本实现第一版踩的坑 —— 分块喂输入时第 12 个字节就解歪了）。
fn decode_peek(b: &mut Bits, h: &Huff) -> Peek {
    if h.empty() {
        return Peek::Bad;
    }
    let mut code: i32 = 0;
    let mut first: i32 = 0;
    let mut index: i32 = 0;
    for len in 1..=15u32 {
        b.refill();
        if b.cnt < len {
            return Peek::NeedInput;
        }
        let bit = ((b.buf >> (len - 1)) & 1) as i32;
        code |= bit;
        let count = h.counts[len as usize] as i32;
        if code - first < count {
            let i = (index + (code - first)) as usize;
            return match h.symbols.get(i) {
                Some(sym) => Peek::Sym(*sym, len),
                None => Peek::Bad,
            };
        }
        index += count;
        first = (first + count) << 1;
        code <<= 1;
    }
    Peek::Bad
}

/// 解一个符号并**取走**它的位
fn decode(b: &mut Bits, h: &Huff) -> Dec {
    match decode_peek(b, h) {
        Peek::Sym(sym, len) => {
            b.consume(len);
            Dec::Sym(sym)
        }
        Peek::NeedInput => Dec::NeedInput,
        Peek::Bad => Dec::Bad,
    }
}

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] =
    [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] =
    [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
const CL_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

fn huff_from_lengths(lens: &[u8]) -> Huff {
    let mut h = Huff::new();
    for &l in lens {
        if (l as usize) < 16 {
            h.counts[l as usize] += 1;
        }
    }
    if h.counts[0] as usize == lens.len() {
        // 一个符号都没有：只有距离树会这样（块里没有回溯引用）
        return h;
    }
    let mut offs = [0u16; 16];
    h.counts[0] = 0;
    for len in 1..16 {
        offs[len] = offs[len - 1] + h.counts[len - 1];
    }
    h.symbols = vec![0; lens.len()];
    for (sym, &l) in lens.iter().enumerate() {
        if l != 0 {
            h.symbols[offs[l as usize] as usize] = sym as u16;
            offs[l as usize] += 1;
        }
    }
    h
}

fn fixed_lit() -> Huff {
    let mut lens = Vec::with_capacity(288);
    for _ in 0..144 {
        lens.push(8u8);
    }
    for _ in 144..256 {
        lens.push(9);
    }
    for _ in 256..280 {
        lens.push(7);
    }
    for _ in 280..288 {
        lens.push(8);
    }
    huff_from_lengths(&lens)
}

fn fixed_dist() -> Huff {
    huff_from_lengths(&[5u8; 30])
}

fn push_out(win: &mut Vec<u8>, out: &mut [u8], produced: &mut usize, byte: u8) {
    out[*produced] = byte;
    *produced += 1;
    win.push(byte);
    if win.len() > 32768 {
        let cut = win.len() - 32768;
        win.drain(0..cut);
    }
}

/// 解压主循环，返回 `(消耗输入, 产出输出)`。
///
/// 可随时因为「输入用完」或「输出满」返回：位读取器把没用完的整字节还给 `pos`，
/// 半个符号的状态不保留（`take` 不足就不取），所以下次从同一个字节继续。
fn inflate_run(inf: &mut Inflate, input: &[u8], out: &mut [u8]) -> Result<(usize, usize), ZErr> {
    let mut b = Bits { input, pos: 0, buf: inf.bitbuf, cnt: inf.bitcnt };
    let mut produced = 0usize;
    let mut err: Option<ZErr> = None;

    loop {
        if produced >= out.len() {
            break;
        }
        if inf.finished {
            break;
        }
        match inf.phase {
            Phase::Head => {
                if inf.need_head {
                    b.byte_align();
                    let hdr = match b.raw(2) {
                        Some(s) => ((s[0] as u32) << 8) | s[1] as u32,
                        None => break,
                    };
                    let cmf = hdr >> 8;
                    if cmf & 0x0f != 8 || hdr % 31 != 0 {
                        err = Some(ZErr::Bad);
                        break;
                    }
                    if hdr & 0x20 != 0 {
                        // FDICT：后面 4 字节字典 adler，我们只能报「需要字典」
                        let _ = b.raw(4);
                        err = Some(ZErr::NeedDict);
                        break;
                    }
                    b.buf = 0;
                    b.cnt = 0;
                    inf.need_head = false;
                }
                // 块头：3 位。可能需要输入 → 记下检查点（表还没动，回退只需位状态）
                let cp = (b.buf, b.cnt, b.pos);
                let hdr = match b.take(3) {
                    Some(v) => v,
                    None => break,
                };
                let last = hdr & 1 == 1;
                let btype = (hdr >> 1) & 3;
                match btype {
                    0 => {
                        b.byte_align();
                        inf.stored_left = STORED_NONE;
                        inf.phase = Phase::Stored;
                        inf.last_block |= last;
                    }
                    1 => {
                        inf.lit = fixed_lit();
                        inf.dist = fixed_dist();
                        inf.phase = Phase::Codes;
                        inf.last_block |= last;
                    }
                    2 => match read_dynamic(&mut b) {
                        Ok((lit, dist)) => {
                            inf.lit = lit;
                            inf.dist = dist;
                            inf.phase = Phase::Codes;
                            inf.last_block |= last;
                        }
                        Err(Dec::NeedInput) => {
                            b.buf = cp.0;
                            b.cnt = cp.1;
                            b.pos = cp.2;
                            break;
                        }
                        Err(Dec::Bad) | Err(Dec::Sym(_)) => {
                            err = Some(ZErr::Bad);
                            break;
                        }
                    },
                    _ => {
                        err = Some(ZErr::Bad);
                        break;
                    }
                }
            }
            Phase::Stored => {
                if inf.stored_left == STORED_NONE {
                    b.byte_align();
                    let cp = (b.buf, b.cnt, b.pos);
                    let len = match b.raw(4) {
                        Some(s) => {
                            let l = u16::from_le_bytes([s[0], s[1]]);
                            let n = u16::from_le_bytes([s[2], s[3]]);
                            if l ^ 0xffff != n {
                                err = Some(ZErr::Bad);
                                break;
                            }
                            l as u32
                        }
                        None => {
                            b.buf = cp.0;
                            b.cnt = cp.1;
                            b.pos = cp.2;
                            break;
                        }
                    };
                    inf.stored_left = len;
                }
                let avail_in = b.input.len() - b.pos;
                let n = (inf.stored_left as usize).min(avail_in).min(out.len() - produced);
                if n > 0 {
                    let chunk_start = b.pos;
                    for k in 0..n {
                        let byte = b.input[chunk_start + k];
                        push_out(&mut inf.win, out, &mut produced, byte);
                    }
                    b.pos += n;
                    inf.stored_left -= n as u32;
                }
                if inf.stored_left == 0 {
                    inf.phase = if inf.last_block { Phase::Tail } else { Phase::Head };
                }
                if n == 0 {
                    break; // 输入或输出用完了
                }
            }
            Phase::Codes => {
                let mut done_block = false;
                // 0) 上一次拷到一半的匹配：先把剩下的字节补完
                if let Some((dist, mut rem)) = inf.pending_copy.take() {
                    while rem > 0 && produced < out.len() {
                        if dist == 0 || dist > inf.win.len() {
                            err = Some(ZErr::Bad);
                            break;
                        }
                        let byte = inf.win[inf.win.len() - dist];
                        push_out(&mut inf.win, out, &mut produced, byte);
                        rem -= 1;
                    }
                    if err.is_some() {
                        break;
                    }
                    if rem > 0 {
                        inf.pending_copy = Some((dist, rem));
                        break; // 输出还是满的
                    }
                }
                while produced < out.len() {
                    // 1) 长度（257..285）。跨调用残留时直接复用，不重读符号
                    let len = match inf.pending_len {
                        Some(l) => l,
                        None => {
                            let (sym, slen) = match decode_peek(&mut b, &inf.lit) {
                                Peek::Sym(sym, slen) => (sym, slen),
                                Peek::NeedInput => break,
                                Peek::Bad => {
                                    err = Some(ZErr::Bad);
                                    break;
                                }
                            };
                            if sym < 256 {
                                b.consume(slen);
                                push_out(&mut inf.win, out, &mut produced, sym as u8);
                                continue;
                            }
                            if sym == 256 {
                                b.consume(slen);
                                done_block = true;
                                break;
                            }
                            let li = (sym - 257) as usize;
                            if li >= 29 {
                                err = Some(ZErr::Bad);
                                break;
                            }
                            // 长度符号 + 它的额外位必须一次看全，否则一位都不取
                            let need = slen as u64 + LEN_EXTRA[li] as u64;
                            if !b.has(need) {
                                break;
                            }
                            b.consume(slen);
                            let extra = b.take(LEN_EXTRA[li] as u32).unwrap_or(0);
                            let l = LEN_BASE[li] as usize + extra as usize;
                            inf.pending_len = Some(l);
                            l
                        }
                    };
                    // 2) 距离（0..29）
                    let dist = {
                        let (dsym, dlen) = match decode_peek(&mut b, &inf.dist) {
                            Peek::Sym(sym, dlen) => (sym as usize, dlen),
                            Peek::NeedInput => break, // 长度已存进 pending_len
                            Peek::Bad => {
                                err = Some(ZErr::Bad);
                                break;
                            }
                        };
                        if dsym >= 30 {
                            err = Some(ZErr::Bad);
                            break;
                        }
                        let need = dlen as u64 + DIST_EXTRA[dsym] as u64;
                        if !b.has(need) {
                            break;
                        }
                        b.consume(dlen);
                        let extra = b.take(DIST_EXTRA[dsym] as u32).unwrap_or(0);
                        DIST_BASE[dsym] as usize + extra as usize
                    };
                    inf.pending_len = None;
                    // 3) 从窗口回溯拷贝（可能拷一半就被输出缓冲截断）
                    if dist == 0 || dist > inf.win.len() {
                        err = Some(ZErr::Bad);
                        break;
                    }
                    let mut rem = len;
                    while rem > 0 && produced < out.len() {
                        let byte = inf.win[inf.win.len() - dist];
                        push_out(&mut inf.win, out, &mut produced, byte);
                        rem -= 1;
                    }
                    if rem > 0 {
                        inf.pending_copy = Some((dist, rem));
                        break; // 输出满
                    }
                }
                if err.is_some() {
                    break;
                }
                if done_block {
                    inf.phase = if inf.last_block { Phase::Tail } else { Phase::Head };
                } else {
                    break; // 输入/输出不足
                }
            }
            Phase::Tail => {
                if inf.nowrap {
                    inf.finished = true;
                    break;
                }
                b.byte_align();
                let cp = (b.buf, b.cnt, b.pos);
                match b.raw(4) {
                    Some(_) => {
                        inf.finished = true;
                        break;
                    }
                    None => {
                        b.buf = cp.0;
                        b.cnt = cp.1;
                        b.pos = cp.2;
                        break;
                    }
                }
            }
        }
    }

    let consumed = b.finish();
    inf.bitbuf = b.buf;
    inf.bitcnt = b.cnt;
    match err {
        Some(e) => Err(e),
        None => Ok((consumed, produced)),
    }
}

/// 动态哈夫曼表：HLIT/HDIST/HCLEN + 码长表 + RLE。
/// 中途缺输入时**不写回**任何表（返回 `NeedInput`，调用方回退位状态重来）。
fn read_dynamic(b: &mut Bits) -> Result<(Huff, Huff), Dec> {
    let hlit = match b.take(5) {
        Some(v) => v as usize + 257,
        None => return Err(Dec::NeedInput),
    };
    let hdist = match b.take(5) {
        Some(v) => v as usize + 1,
        None => return Err(Dec::NeedInput),
    };
    let hclen = match b.take(4) {
        Some(v) => v as usize + 4,
        None => return Err(Dec::NeedInput),
    };
    if hlit > 286 || hdist > 30 {
        return Err(Dec::Bad);
    }
    let mut cl = [0u8; 19];
    for i in 0..hclen {
        cl[CL_ORDER[i]] = match b.take(3) {
            Some(v) => v as u8,
            None => return Err(Dec::NeedInput),
        };
    }
    let clh = huff_from_lengths(&cl);
    let mut lens = vec![0u8; hlit + hdist];
    let mut i = 0;
    while i < lens.len() {
        let sym = match decode(b, &clh) {
            Dec::Sym(s) => s,
            Dec::NeedInput => return Err(Dec::NeedInput),
            Dec::Bad => return Err(Dec::Bad),
        };
        match sym {
            0..=15 => {
                lens[i] = sym as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err(Dec::Bad);
                }
                let prev = lens[i - 1];
                let n = match b.take(2) {
                    Some(v) => v as usize + 3,
                    None => return Err(Dec::NeedInput),
                };
                for _ in 0..n {
                    if i >= lens.len() {
                        return Err(Dec::Bad);
                    }
                    lens[i] = prev;
                    i += 1;
                }
            }
            17 => {
                let n = match b.take(3) {
                    Some(v) => v as usize + 3,
                    None => return Err(Dec::NeedInput),
                };
                for _ in 0..n {
                    if i >= lens.len() {
                        return Err(Dec::Bad);
                    }
                    lens[i] = 0;
                    i += 1;
                }
            }
            18 => {
                let n = match b.take(7) {
                    Some(v) => v as usize + 11,
                    None => return Err(Dec::NeedInput),
                };
                for _ in 0..n {
                    if i >= lens.len() {
                        return Err(Dec::Bad);
                    }
                    lens[i] = 0;
                    i += 1;
                }
            }
            _ => return Err(Dec::Bad),
        }
    }
    Ok((huff_from_lengths(&lens[..hlit]), huff_from_lengths(&lens[hlit..])))
}

// ---------------------------------------------------------------- 压缩（stored 块）

fn z_deflate_bytes_bytes(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let addr = f.arg_l(0) as u64;
    let in_arr = f.arg_i(1);
    let in_off = f.arg_i(2).max(0) as usize;
    let in_len = f.arg_i(3).max(0) as usize;
    let out_arr = f.arg_i(4);
    let out_off = f.arg_i(5).max(0) as usize;
    let out_len = f.arg_i(6).max(0) as usize;
    // 0 = NO_FLUSH / 1 = PARTIAL / 2 = SYNC / 3 = FULL / 4 = FINISH
    let flush = f.arg_i(7);

    let input = read_bytes(vm, in_arr, in_off, in_len);
    let mut out = vec![0u8; out_len];

    let mut g = zlock();
    let s = g.get_mut(&addr).ok_or_else(|| VmError {
        msg: "Deflater: address 无效（未 init 或已 end）".into(),
        exc: None,
    })?;
    let adler_new = adler32(s.adler, &input);
    s.adler = adler_new;
    let def = s.def.as_mut().ok_or_else(|| VmError {
        msg: "Deflater: address 不是 deflate 流".into(),
        exc: None,
    })?;

    // 输入全收下（stored 实现不需要边读边算），把收到的字节数原样报回去
    def.in_buf.extend_from_slice(&input);
    if !def.wrote_head {
        if !def.nowrap {
            // 0x78 0x01：(CMF<<8|FLG) % 31 == 0，CM=8，FDICT=0，最低压缩级
            def.out_pending.push(0x78);
            def.out_pending.push(0x01);
        }
        def.wrote_head = true;
    }
    if flush == 4 {
        def.finish = true;
    }
    if flush >= 2 {
        emit_stored(def);
    }
    if def.finish && !def.trailer_done {
        // 所有输入都已成块 → 写 final 空块 + zlib 尾
        emit_stored(def);
        emit_final(def, adler_new);
        def.trailer_done = true;
    }
    let n = def.out_pending.len().min(out.len());
    out[..n].copy_from_slice(&def.out_pending[..n]);
    def.out_pending.drain(0..n);
    // 还有待写出的字节时不能报 finished：否则 Java 侧会停在大半截的流上
    let finished = def.trailer_done && def.out_pending.is_empty();
    drop(g);

    if n > 0 {
        write_bytes(vm, out_arr, out_off, &out[..n]);
    }
    Ok(Slot::L(pack(input.len(), n, finished, false)))
}

/// 把 `in_buf` 全部变成 stored 块（每块 ≤ 65535），块头字节 `0x00` = 对齐 + BFINAL=0 + BTYPE=00
fn emit_stored(d: &mut Deflate) {
    while !d.in_buf.is_empty() {
        let n = d.in_buf.len().min(65535);
        let chunk: Vec<u8> = d.in_buf.drain(0..n).collect();
        d.out_pending.push(0x00);
        let len = chunk.len() as u16;
        d.out_pending.extend_from_slice(&len.to_le_bytes());
        d.out_pending.extend_from_slice(&(!len).to_le_bytes());
        d.out_pending.extend_from_slice(&chunk);
    }
}

/// final 空块（BFINAL=1, BTYPE=00）+ zlib 尾（原始数据 adler32，大端）
fn emit_final(d: &mut Deflate, adler: u32) {
    d.out_pending.push(0x01);
    d.out_pending.extend_from_slice(&0u16.to_le_bytes());
    d.out_pending.extend_from_slice(&0xffffu16.to_le_bytes());
    if !d.nowrap {
        d.out_pending.extend_from_slice(&adler.to_be_bytes());
    }
}

// ---------------------------------------------------------------- CRC32 / Adler32

const CRC_TABLE: [u32; 256] = build_crc_table();

const fn build_crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

fn crc32_update(crc: u32, data: &[u8]) -> u32 {
    let mut c = !crc;
    for &b in data {
        c = CRC_TABLE[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

fn adler32(adler: u32, data: &[u8]) -> u32 {
    let mut a = adler & 0xffff;
    let mut b = (adler >> 16) & 0xffff;
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn crc32_update_byte(_vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let crc = f.arg_i(0) as u32;
    let b = f.arg_i(1) as u8;
    Ok(Slot::I(crc32_update(crc, &[b]) as i32))
}

fn crc32_update_bytes(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let crc = f.arg_i(0) as u32;
    let arr = f.arg_i(1);
    let off = f.arg_i(2).max(0) as usize;
    let len = f.arg_i(3).max(0) as usize;
    let data = read_bytes(vm, arr, off, len);
    Ok(Slot::I(crc32_update(crc, &data) as i32))
}

fn adler32_update_byte(_vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let a = f.arg_i(0) as u32;
    let b = f.arg_i(1) as u8;
    Ok(Slot::I(adler32(a, &[b]) as i32))
}

fn adler32_update_bytes(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let a = f.arg_i(0) as u32;
    let arr = f.arg_i(1);
    let off = f.arg_i(2).max(0) as usize;
    let len = f.arg_i(3).max(0) as usize;
    let data = read_bytes(vm, arr, off, len);
    Ok(Slot::I(adler32(a, &data) as i32))
}

// ================================================================ D 组：LibFallback（FFM 兜底）

fn group_d_libfallback(r: &mut NativeRegistry) {
    let fb = "jdk/internal/foreign/abi/fallback/LibFallback";
    // **本机没有 libffi** → `init()` 返回 false：`LibFallback.SUPPORTED` 为 false，
    // `Linker.nativeLinker()` 走「本平台不支持」的干净失败路径，而不是在半初始化
    // 状态下炸。这是诚实的做法：编不过去的能力不要假装有。
    r.add(fb, "init", "()Z", |_, _| zi(0));
    // 尺寸/对齐是 ABI 常量（Windows x64：short 2 / int 4 / long 4 / wchar 2 / long long 8 / double 8）
    r.add(fb, "sizeofCif", "()J", |_, _| Ok(Slot::L(128)));
    r.add(fb, "ffi_sizeof_short", "()I", |_, _| zi(2));
    r.add(fb, "ffi_sizeof_int", "()I", |_, _| zi(4));
    r.add(fb, "ffi_sizeof_long", "()I", |_, _| zi(4));
    r.add(fb, "ffi_sizeof_wchar", "()I", |_, _| zi(2));
    r.add(fb, "alignof_long_long", "()I", |_, _| zi(8));
    r.add(fb, "alignof_double", "()I", |_, _| zi(8));
    r.add(fb, "ffi_default_abi", "()I", |_, _| zi(0));
    r.add(fb, "ffi_type_struct", "()S", |_, _| zi(13));
    // `ffi_type*`：没有 libffi → 给固定假地址（只要非 0，`Linker` 侧的尺寸推算就能继续；
    // 这些值没人解引用）
    // ffi_type_* 的假地址（每个类型一个固定值，只要非 0）
    r.add(fb, "ffi_type_void", "()J", const_type_addr(0x1000_0000));
    r.add(fb, "ffi_type_uint8", "()J", const_type_addr(0x1000_0001));
    r.add(fb, "ffi_type_sint8", "()J", const_type_addr(0x1000_0002));
    r.add(fb, "ffi_type_uint16", "()J", const_type_addr(0x1000_0003));
    r.add(fb, "ffi_type_sint16", "()J", const_type_addr(0x1000_0004));
    r.add(fb, "ffi_type_uint32", "()J", const_type_addr(0x1000_0005));
    r.add(fb, "ffi_type_sint32", "()J", const_type_addr(0x1000_0006));
    r.add(fb, "ffi_type_uint64", "()J", const_type_addr(0x1000_0007));
    r.add(fb, "ffi_type_sint64", "()J", const_type_addr(0x1000_0008));
    r.add(fb, "ffi_type_float", "()J", const_type_addr(0x1000_0009));
    r.add(fb, "ffi_type_double", "()J", const_type_addr(0x1000_000a));
    r.add(fb, "ffi_type_pointer", "()J", const_type_addr(0x1000_000b));
    // cif 准备：给 FFI_OK(0)
    r.add(fb, "ffi_prep_cif", "(JIIJJ)I", |_, _| zi(0));
    r.add(fb, "ffi_prep_cif_var", "(JIIIJJ)I", |_, _| zi(0));
    r.add(fb, "ffi_get_struct_offsets", "(IJJ)I", |_, _| zi(0));
    // 真正的调用与闭包：没有 libffi 做不到，明确报错（不能静默返回 0 ——
    // 那会让 FFM 把垃圾当返回值）
    r.add(fb, "createClosure", "(JLjava/lang/Object;[J)I", libfallback_unsupported);
    r.add(fb, "freeClosure", "(JJ)V", no_op);
    r.add(fb, "doDowncall", "(JJJJLjava/lang/Object;JI[Ljava/lang/Object;I)V", libfallback_unsupported);
}

/// 给 `ffi_type_*` 用的常量返回器：`NativeFn` 是 fn 指针，捕获闭包转不过去
fn const_type_addr(id: i64) -> fn(&mut Vm, &mut Frame) -> Result<Slot, VmError> {
    match id {
        0x1000_0000 => |_, _| Ok(Slot::L(0x1000_0000)),
        0x1000_0001 => |_, _| Ok(Slot::L(0x1000_0001)),
        0x1000_0002 => |_, _| Ok(Slot::L(0x1000_0002)),
        0x1000_0003 => |_, _| Ok(Slot::L(0x1000_0003)),
        0x1000_0004 => |_, _| Ok(Slot::L(0x1000_0004)),
        0x1000_0005 => |_, _| Ok(Slot::L(0x1000_0005)),
        0x1000_0006 => |_, _| Ok(Slot::L(0x1000_0006)),
        0x1000_0007 => |_, _| Ok(Slot::L(0x1000_0007)),
        0x1000_0008 => |_, _| Ok(Slot::L(0x1000_0008)),
        0x1000_0009 => |_, _| Ok(Slot::L(0x1000_0009)),
        0x1000_000a => |_, _| Ok(Slot::L(0x1000_000a)),
        _ => |_, _| Ok(Slot::L(0x1000_000b)),
    }
}

fn libfallback_unsupported(_vm: &mut Vm, _f: &mut Frame) -> Result<Slot, VmError> {
    Err(VmError {
        msg: "LibFallback: 本机没有 libffi，FFM 的 doDowncall/createClosure 无法实现\
              （init() 已返回 false，Linker 应走「不支持」路径）"
            .into(),
        exc: None,
    })
}

// ================================================================ E 组：HostLocale（简化版）

fn group_e_host_locale(r: &mut NativeRegistry) {
    let h = "sun/util/locale/provider/HostLocaleProviderAdapterImpl";
    // **简化版**：只覆盖 zh / en 两档（数据取自本机真 java 的 HostLocale 输出，
    // 也就是 Windows 宿主数据），其余 locale 回落到调用方传进来的 default 值 ——
    // 与「host 没有该 locale」时的 JDK 行为一致。
    r.add(h, "initialize", "()Z", |_, _| zi(1));
    r.add(h, "getDefaultLocale", "(I)Ljava/lang/String;", |vm, _f| {
        // 本机（Windows 中文）默认：display / format 都是 zh-CN
        let s = new_text(vm, "zh-CN");
        Ok(Slot::I(s))
    });
    r.add(h, "getDateTimePattern", "(IILjava/lang/String;)Ljava/lang/String;", hl_datetime_pattern);
    r.add(h, "getCalendarID", "(Ljava/lang/String;)I", |_, _| zi(-1));
    r.add(h, "getAmPmStrings", "(Ljava/lang/String;[Ljava/lang/String;)[Ljava/lang/String;", hl_ampm);
    r.add(h, "getEras", "(Ljava/lang/String;[Ljava/lang/String;)[Ljava/lang/String;", hl_eras);
    r.add(h, "getMonths", "(Ljava/lang/String;[Ljava/lang/String;)[Ljava/lang/String;", hl_months);
    r.add(h, "getShortMonths", "(Ljava/lang/String;[Ljava/lang/String;)[Ljava/lang/String;", hl_short_months);
    r.add(h, "getWeekdays", "(Ljava/lang/String;[Ljava/lang/String;)[Ljava/lang/String;", hl_weekdays);
    r.add(
        h,
        "getShortWeekdays",
        "(Ljava/lang/String;[Ljava/lang/String;)[Ljava/lang/String;",
        hl_short_weekdays,
    );
    r.add(h, "getNumberPattern", "(ILjava/lang/String;)Ljava/lang/String;", hl_number_pattern);
    r.add(h, "isNativeDigit", "(Ljava/lang/String;)Z", |_, _| zi(0));

    // `DecimalFormatSymbols` / `DateFormatSymbols` 的取值点：
    // 有数据就给数据，没数据就返回调用方给的 default
    r.add(h, "getCurrencySymbol", "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;", |vm, f| {
        hl_str_or_default(vm, f, |d| d.currency_symbol)
    });
    r.add(
        h,
        "getInternationalCurrencySymbol",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        |vm, f| hl_str_or_default(vm, f, |d| d.currency_code),
    );
    r.add(h, "getInfinity", "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;", |vm, f| {
        hl_str_or_default(vm, f, |d| d.infinity)
    });
    r.add(h, "getNaN", "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;", |vm, f| {
        hl_str_or_default(vm, f, |d| d.nan)
    });
    r.add(h, "getDecimalSeparator", "(Ljava/lang/String;C)C", |vm, f| hl_char(vm, f, |d| d.decimal_sep));
    r.add(h, "getGroupingSeparator", "(Ljava/lang/String;C)C", |vm, f| hl_char(vm, f, |d| d.grouping_sep));
    r.add(h, "getMinusSign", "(Ljava/lang/String;C)C", |vm, f| hl_char(vm, f, |d| d.minus_sign));
    r.add(h, "getMonetaryDecimalSeparator", "(Ljava/lang/String;C)C", |vm, f| {
        hl_char(vm, f, |d| d.monetary_sep)
    });
    r.add(h, "getPercent", "(Ljava/lang/String;C)C", |vm, f| hl_char(vm, f, |d| d.percent));
    r.add(h, "getPerMill", "(Ljava/lang/String;C)C", |vm, f| hl_char(vm, f, |d| d.permill));
    r.add(h, "getZeroDigit", "(Ljava/lang/String;C)C", |vm, f| hl_char(vm, f, |d| d.zero_digit));
    r.add(h, "getCalendarDataValue", "(Ljava/lang/String;I)I", |vm, f| {
        let lang = arg_text(vm, f, 0);
        let kind = f.arg_i(1);
        let v = match (hl_data(&lang), kind) {
            (Some(d), 0) => d.first_day_of_week,
            (Some(d), 1) => d.min_days_in_first_week,
            _ => 0,
        };
        zi(v)
    });
    r.add(
        h,
        "getCalendarDisplayStrings",
        "(Ljava/lang/String;III)[Ljava/lang/String;",
        hl_calendar_display,
    );
    r.add(
        h,
        "getDisplayString",
        "(Ljava/lang/String;ILjava/lang/String;)Ljava/lang/String;",
        hl_display_string,
    );
}

struct HlData {
    date_long: &'static str,
    date_short: &'static str,
    time_long: &'static str,
    time_short: &'static str,
    num: &'static str,
    num_cur: &'static str,
    num_pct: &'static str,
    num_int: &'static str,
    ampm: [&'static str; 2],
    eras: [&'static str; 2],
    months: [&'static str; 12],
    short_months: [&'static str; 12],
    weekdays: [&'static str; 8],
    short_weekdays: [&'static str; 8],
    currency_symbol: &'static str,
    currency_code: &'static str,
    decimal_sep: char,
    grouping_sep: char,
    minus_sign: char,
    monetary_sep: char,
    percent: char,
    permill: char,
    zero_digit: char,
    infinity: &'static str,
    nan: &'static str,
    first_day_of_week: i32,
    min_days_in_first_week: i32,
    display_language: &'static str,
}

const HL_ZH: HlData = HlData {
    date_long: "yyyy年M月d日",
    date_short: "yyyy/M/d",
    time_long: "H:mm:ss",
    time_short: "H:mm",
    num: "#,##0.##;-#,##0.##",
    num_cur: "¤#,##0.##;¤-#,##0.##",
    num_pct: "#,##0.##%;-#,##0.##%",
    num_int: "#,##0;-#,##0",
    ampm: ["上午", "下午"],
    eras: ["上午", "公元"],
    months: [
        "一月", "二月", "三月", "四月", "五月", "六月", "七月", "八月", "九月", "十月", "十一月", "十二月",
    ],
    short_months: ["1月", "2月", "3月", "4月", "5月", "6月", "7月", "8月", "9月", "10月", "11月", "12月"],
    weekdays: ["", "星期日", "星期一", "星期二", "星期三", "星期四", "星期五", "星期六"],
    short_weekdays: ["", "周日", "周一", "周二", "周三", "周四", "周五", "周六"],
    currency_symbol: "¥",
    currency_code: "CNY",
    decimal_sep: '.',
    grouping_sep: ',',
    minus_sign: '-',
    monetary_sep: '.',
    percent: '%',
    permill: '‰',
    zero_digit: '0',
    infinity: "∞",
    nan: "NaN",
    first_day_of_week: 0,
    min_days_in_first_week: 0,
    display_language: "中文",
};

const HL_EN: HlData = HlData {
    date_long: "dddd, MMMM d, yyyy",
    date_short: "M/d/yyyy",
    time_long: "h:mm:ss tt",
    time_short: "h:mm tt",
    num: "#,##0.##;-#,##0.##",
    num_cur: "¤#,##0.##;(¤#,##0.##)",
    num_pct: "#,##0.##%;-#,##0.##%",
    num_int: "#,##0;-#,##0",
    ampm: ["AM", "PM"],
    eras: ["AM", "A.D."],
    months: [
        "January", "February", "March", "April", "May", "June", "July", "August", "September",
        "October", "November", "December",
    ],
    short_months: ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"],
    weekdays: ["", "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"],
    short_weekdays: ["", "Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"],
    currency_symbol: "$",
    currency_code: "USD",
    decimal_sep: '.',
    grouping_sep: ',',
    minus_sign: '-',
    monetary_sep: '.',
    percent: '%',
    permill: '‰',
    zero_digit: '0',
    infinity: "∞",
    nan: "NaN",
    first_day_of_week: 0,
    min_days_in_first_week: 0,
    display_language: "English",
};

/// 语言标签（`toLanguageTag()` 形态，例：`zh-CN`）→ 数据行。
/// 只按**语言**匹配：`zh-CN`/`zh-TW`/`zh` 都落到 `HL_ZH`（简化版的取舍）。
fn hl_data(lang_tag: &str) -> Option<&'static HlData> {
    let lang = lang_tag
        .trim()
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match lang.as_str() {
        "zh" => Some(&HL_ZH),
        "en" => Some(&HL_EN),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum HlField {
    AmPm,
    Eras,
    Months,
    ShortMonths,
    Weekdays,
    ShortWeekdays,
}

fn hl_strings(vm: &mut Vm, f: &mut Frame, which: HlField) -> Result<Slot, VmError> {
    let lang = arg_text(vm, f, 0);
    let def = f.arg_i(1);
    let d = match hl_data(&lang) {
        Some(d) => d,
        None => return Ok(Slot::I(def)),
    };
    let items: Vec<String> = match which {
        HlField::AmPm => d.ampm.iter().map(|s| s.to_string()).collect(),
        HlField::Eras => d.eras.iter().map(|s| s.to_string()).collect(),
        // 月份/星期数组的下标从 0 起有占位（JDK 的 host 实现给空串）
        // 月份数组 13 项（Calendar 的月份从 0 起，第 13 项是「闰月」占位）
        HlField::Months | HlField::ShortMonths => {
            let src = if matches!(which, HlField::Months) { &d.months } else { &d.short_months };
            let mut v: Vec<String> = src.iter().map(|s| s.to_string()).collect();
            v.push(String::new());
            v
        }
        HlField::Weekdays => d.weekdays.iter().map(|s| s.to_string()).collect(),
        HlField::ShortWeekdays => d.short_weekdays.iter().map(|s| s.to_string()).collect(),
    };
    Ok(Slot::I(text_array(vm, &items)))
}

fn hl_ampm(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    hl_strings(vm, f, HlField::AmPm)
}
fn hl_eras(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    hl_strings(vm, f, HlField::Eras)
}
fn hl_months(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    hl_strings(vm, f, HlField::Months)
}
fn hl_short_months(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    hl_strings(vm, f, HlField::ShortMonths)
}
fn hl_weekdays(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    hl_strings(vm, f, HlField::Weekdays)
}
fn hl_short_weekdays(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    hl_strings(vm, f, HlField::ShortWeekdays)
}

fn hl_char(vm: &mut Vm, f: &mut Frame, get: fn(&HlData) -> char) -> Result<Slot, VmError> {
    let lang = arg_text(vm, f, 0);
    let def = f.arg_i(1);
    match hl_data(&lang) {
        Some(d) => Ok(Slot::I(get(d) as i32)),
        None => Ok(Slot::I(def)),
    }
}

fn hl_str_or_default(vm: &mut Vm, f: &mut Frame, get: fn(&HlData) -> &'static str) -> Result<Slot, VmError> {
    let lang = arg_text(vm, f, 0);
    let def = f.arg_i(1);
    match hl_data(&lang) {
        Some(d) => {
            let s = new_text(vm, get(d));
            Ok(Slot::I(s))
        }
        None => Ok(Slot::I(def)),
    }
}

fn hl_number_pattern(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let kind = f.arg_i(0);
    let lang = arg_text(vm, f, 1);
    let d = match hl_data(&lang) {
        Some(d) => d,
        None => return Ok(Slot::I(0)),
    };
    let p = match kind {
        0 => d.num,
        1 => d.num_cur,
        2 => d.num_pct,
        _ => d.num_int,
    };
    let s = new_text(vm, p);
    Ok(Slot::I(s))
}

/// `getDateTimePattern(int dateStyle, int timeStyle, String langTag)`：
/// `-1` = 这次不需要；`DateFormat.FULL=0 / LONG=1 / MEDIUM=2 / SHORT=3`。
/// host 侧只有「短」「长」两档，MEDIUM 归到短。
fn hl_datetime_pattern(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let ds = f.arg_i(0);
    let ts = f.arg_i(1);
    let lang = arg_text(vm, f, 2);
    let d = match hl_data(&lang) {
        Some(d) => d,
        None => return Ok(Slot::I(0)),
    };
    let date = if ds == 3 || ds == 2 { d.date_short } else { d.date_long };
    let time = if ts == 3 || ts == 2 { d.time_short } else { d.time_long };
    let s = match (ds >= 0, ts >= 0) {
        (true, false) => date.to_string(),
        (false, true) => time.to_string(),
        (true, true) => format!("{date} {time}"),
        _ => return Ok(Slot::I(0)),
    };
    let h = new_text(vm, s);
    Ok(Slot::I(h))
}

/// `getCalendarDisplayStrings(langTag, field, style, calendarType)`：
/// `field`：`MONTH=2`；`style`：`ALL_STYLES=0 / SHORT=1 / LONG=2`。
/// 只给 MONTH，其余返回 null（调用方回落到 CLDR）。
fn hl_calendar_display(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let lang = arg_text(vm, f, 0);
    let field = f.arg_i(1);
    let style = f.arg_i(2);
    let d = match hl_data(&lang) {
        Some(d) => d,
        None => return Ok(Slot::I(0)),
    };
    if field != 2 {
        return Ok(Slot::I(0));
    }
    let items: Vec<String> = match style {
        1 => d.short_months.iter().map(|s| s.to_string()).collect(),
        2 => d.months.iter().map(|s| s.to_string()).collect(),
        _ => {
            let mut v: Vec<String> = d.months.iter().map(|s| s.to_string()).collect();
            v.extend(d.short_months.iter().map(|s| s.to_string()));
            v
        }
    };
    Ok(Slot::I(text_array(vm, &items)))
}

/// `getDisplayString(langTag, type, key)`：`DN_LOCALE_LANGUAGE=2`。
/// 其余（货币/地区/脚本/变体）我们只有语言级的名字 → null，让调用方回落。
fn hl_display_string(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let lang = arg_text(vm, f, 0);
    let kind = f.arg_i(1);
    let d = match hl_data(&lang) {
        Some(d) => d,
        None => return Ok(Slot::I(0)),
    };
    if kind != 2 {
        return Ok(Slot::I(0));
    }
    let s = new_text(vm, d.display_language);
    Ok(Slot::I(s))
}

// ================================================================ F 组：直接内存
//
// `Unsafe` 的内存族（`allocateMemory0` / `setMemory0` / `copyMemory0` / 裸地址读写）
// **归 `nat_intrinsics.rs`**（另一条工作线），本文件不重复注册 —— 注册顺序是
// `nat_intrinsics → nat_os → nat_util`，在这里再注册一次会把它们整组覆盖掉。
//
// 这里只补一个 `Unsafe` 之外、但 `ByteBuffer.allocateDirect` 链上会碰到的类。
// 附注：JDK 25 的 `ZipUtils.defaultBuf` 已经是 `ByteBuffer.allocate(0)`（堆上），
// 不再需要直接内存。

fn group_f_raw_memory(r: &mut NativeRegistry) {
    // `ByteBuffer.allocateDirect` 链上 `ScopedMemoryAccess.<clinit>` 会调它
    r.add("jdk/internal/misc/ScopedMemoryAccess", "registerNatives", "()V", no_op);
    r.add(
        "jdk/internal/misc/ScopedMemoryAccess",
        "closeScope0",
        "(Ljdk/internal/foreign/MemorySessionImpl;Ljdk/internal/misc/ScopedMemoryAccess$ScopedAccessError;)V",
        no_op,
    );
    // `java.util.zip.ZipUtils.defaultBuf = ByteBuffer.allocateDirect(0)` 是 zip 的静态
    // 初始化点。本 VM 没有「裸内存 + Cleaner 回收」这条链（`DirectByteBuffer` 构造器要
    // `Unsafe.allocateMemory` + `Cleaner.create`，后者在无栈 VM 上会挂住），
    // 而 zip 的 byte[] 路径（`setInput(byte[],int,int)`）**根本不用 defaultBuf** ——
    // 它只是被 getstatic 碰到一下。所以这里返回 null：`Inflater`/\`Deflater\` 里
    // `input == null` 正好是「走 byte[] 路径」的标志。
    r.add_override("java/nio/ByteBuffer", "allocateDirect", "(I)Ljava/nio/ByteBuffer;", |_, _| {
        Ok(Slot::I(0))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_known_value() {
        // "123456789" 的 CRC-32 是 0xCBF43926
        assert_eq!(crc32_update(0, b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn adler32_matches_known_value() {
        // "Wikipedia" 的 Adler-32 是 0x11E60398
        assert_eq!(adler32(1, b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn pack_layout_matches_jdk_bytecode() {
        assert_eq!(pack(1, 0, false, false), 1);
        assert_eq!(pack(0, 1, false, false), 1i64 << 31);
        assert_eq!(pack(0, 0, true, false), 1i64 << 62);
        assert_eq!(pack(0, 0, false, true), (1i64) << 63);
    }

    #[test]
    fn signal_numbers() {
        assert_eq!(signal_number("INT"), 2);
        assert_eq!(signal_number("TERM"), 15);
        assert_eq!(signal_number("nope"), -1);
    }

    #[test]
    fn huffman_tables_build() {
        let h = fixed_lit();
        assert!(h.counts[7] > 0 && h.counts[8] > 0 && h.counts[9] > 0);
        assert_eq!(h.symbols.len(), 288);
        assert_eq!(fixed_dist().symbols.len(), 30);
    }

    #[test]
    fn stored_roundtrip() {
        // stored 块自检：压出来的流用本文件的 inflate 解回原文
        let src: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let mut d = Deflate {
            nowrap: true,
            wrote_head: false,
            in_buf: src.clone(),
            out_pending: Vec::new(),
            trailer_done: false,
            finish: true,
        };
        emit_stored(&mut d);
        emit_final(&mut d, 1);
        let mut inf = fresh_inflate(true);
        let mut out = vec![0u8; src.len() + 8];
        let mut used = 0usize;
        let mut produced = 0usize;
        // 输出缓冲区大小有限，可能要调多次（与 Java 侧的循环一致）
        for _ in 0..8 {
            if inf.finished {
                break;
            }
            let (u, pr) = match inflate_run(&mut inf, &d.out_pending[used..], &mut out[produced..]) {
                Ok(v) => v,
                Err(_) => panic!("inflate 失败"),
            };
            used += u;
            produced += pr;
            if u == 0 && pr == 0 {
                break;
            }
        }
        assert_eq!(produced, src.len());
        assert_eq!(&out[..produced], &src[..]);
        assert_eq!(used, d.out_pending.len());
        assert!(inf.finished);
    }

    #[test]
    fn fixed_huffman_literals() {
        // 手搓一个只含「字面量 + EOB」的固定哈夫曼块，验证解码路径
        // 块头：BFINAL=1 BTYPE=01 → 3 位 0b011 → 字节 0x03（后面补 5 个 0 位）
        // 'A'(65) 的固定码：8 位，码字 = 0x30 + 65 = 0x71（从高位写，LSB 先出）
        // 这里只验证 EOB(256) = 7 位 0 这一个最小流
        let mut s = Bits { input: &[0b0000_0011u8, 0b0000_0000], pos: 0, buf: 0, cnt: 0 };
        let hdr = s.take(3).unwrap();
        assert_eq!(hdr, 0b011);
        let lit = fixed_lit();
        let sym = match decode(&mut s, &lit) {
            Dec::Sym(v) => v,
            _ => panic!("EOB 解不出来"),
        };
        assert_eq!(sym, 256);
    }

    /// 真 java 压出来的 zlib 流（`new Deflater()` + 2033 字节 UTF-8 文本，
    /// 见 `D:/vmless/nat/g/Gen.java`）：解出来必须是同一份原文。
    ///
    /// 这是**动态哈夫曼 + 回溯引用 + zlib 包装（含 adler32 尾）**的端到端证据 ——
    /// 只靠自压自解证明不了解码器对真 zlib 流有效。
    #[test]
    fn inflates_real_zlib_stream_from_jdk() {
        let hex = "789c0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29848c2a1e553caa7854f1a8e251c5c34b716252724a6a5a7a466656764e6e5e7e41615171496959794565d5d09279b263edb369ed0aa1216eba160acfb676bf583f150086f2e5ad";
        let mut src = Vec::new();
        let b = hex.as_bytes();
        let mut i = 0;
        while i + 1 < b.len() {
            let hi = (b[i] as char).to_digit(16).unwrap() as u8;
            let lo = (b[i + 1] as char).to_digit(16).unwrap() as u8;
            src.push((hi << 4) | lo);
            i += 2;
        }
        // 原文：与 Gen.java 的 payload() 一致
        let mut text = String::new();
        for _ in 0..40 {
            text.push_str("The quick brown fox jumps over the lazy dog. ");
        }
        for k in 0..200 {
            text.push((b'a' + (k % 26) as u8) as char);
        }
        text.push_str("中文 UTF-8 测试");
        let want = text.as_bytes();
        assert_eq!(want.len(), 2019);

        let mut inf = fresh_inflate(false);
        let mut out = vec![0u8; want.len() + 16];
        let (used, produced) = match inflate_run(&mut inf, &src, &mut out) {
            Ok(v) => v,
            Err(_) => panic!("解 zlib 流失败"),
        };
        assert_eq!(produced, want.len(), "解出来的长度不对");
        assert_eq!(&out[..produced], want, "解出来的内容不对");
        assert_eq!(used, src.len(), "应当把整条流（含 adler 尾）都吃掉");
        assert!(inf.finished);
    }

    /// 输出驱动的分块：每次只给 37 字节输出，输入一次给全
    /// （这正是 `Inflater.inflate(byte[],off,len)` 的典型用法）
    #[test]
    fn inflates_with_tiny_output_buffer() {
        let src = real_zlib_stream();
        let mut inf = fresh_inflate(false);
        let mut out_all = Vec::new();
        let mut buf = [0u8; 37];
        let mut fed = 0usize;
        for _ in 0..200 {
            if inf.finished {
                break;
            }
            let (used, produced) = match inflate_run(&mut inf, &src[fed..], &mut buf) {
                Ok(v) => v,
                Err(_) => panic!("输出分块解失败 fed={fed}"),
            };
            fed += used;
            out_all.extend_from_slice(&buf[..produced]);
            if used == 0 && produced == 0 {
                break;
            }
        }
        assert_eq!(out_all.len(), 2019, "输出分块解出来的长度不对");
    }

    /// 输入驱动的分块：每次只喂 3 字节（不够就再补 8 字节），
    /// 逼出「位不够 → 不消耗、等输入」这条路径
    #[test]
    fn inflates_in_small_input_chunks() {
        let src = real_zlib_stream();
        let mut inf = fresh_inflate(false);
        let mut out_all = Vec::new();
        let mut buf = [0u8; 4096];
        let mut fed = 0usize;
        let mut window = 3usize;
        for _ in 0..4000 {
            if inf.finished {
                break;
            }
            let end = src.len().min(fed + window);
            let (used, produced) = match inflate_run(&mut inf, &src[fed..end], &mut buf) {
                Ok(v) => v,
                Err(_) => panic!("输入分块解失败 fed={fed} window={window}"),
            };
            fed += used;
            out_all.extend_from_slice(&buf[..produced]);
            if used == 0 && produced == 0 {
                // 这次一位都没吃得下（需要的位比窗口多）：窗口翻倍再喂，最多到 64 字节
                if fed >= src.len() {
                    break;
                }
                window = (window * 2).min(64);
            } else {
                window = 3;
            }
        }
        assert_eq!(fed, src.len(), "应当把整条流都吃掉");
        assert_eq!(out_all.len(), 2019, "输入分块解出来的长度不对");
        assert!(inf.finished);
    }

    /// 真 java 压出来的 zlib 流（`new Deflater()` + 2033 字节 UTF-8 文本）
    fn real_zlib_stream() -> Vec<u8> {
        let hex = "789c0bc94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d5228014ae72456552aa4e4a7eb29848c2a1e553caa7854f1a8e251c5c34b716252724a6a5a7a466656764e6e5e7e41615171496959794565d5d09279b263edb369ed0aa1216eba160acfb676bf583f150086f2e5ad";
        let mut src = Vec::new();
        let b = hex.as_bytes();
        let mut i = 0;
        while i + 1 < b.len() {
            let hi = (b[i] as char).to_digit(16).unwrap() as u8;
            let lo = (b[i + 1] as char).to_digit(16).unwrap() as u8;
            src.push((hi << 4) | lo);
            i += 2;
        }
        src
    }

}
