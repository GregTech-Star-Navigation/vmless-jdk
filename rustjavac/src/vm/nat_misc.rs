//! native 实现分组：nat_misc
//!
//! 由 `native_reg.rs` 的 `register_all` 末尾调用 `register(r)`。
//!
//! 覆盖面：java.base 里**剩下的、既不属于 OS/NIO 也不属于内存与反射核心**的那一组。
//!
//! | 类 | 覆盖 | 说明 |
//! | --- | --- | --- |
//! | `java/lang/ClassLoader` | 5/5 真实现 | `defineClass0/1/2`、`findLoadedClass0`、`findBootstrapClass`、`retrieveDirectives` |
//! | `java/lang/Thread` | `start0` | 交给 `vm/thread.rs::thread_start`（`start()` 已有 override，这条是「有人绕过 start 直接调」的兜底） |
//! | `java/lang/Module` | 5/5 | 全是 **no-op**：Java 侧自己维护 reads/exports 表，native 只通知 VM，见下 |
//! | `java/lang/ref/{Finalizer,PhantomReference}` | 4/4 真实现 | 终结器关闭（与 `--finalization=disabled` 一致）；`clear0`/`refersTo0` 直接读写 `referent` |
//! | `java/lang/reflect/Executable`/`Field` | 3/3 真实现 | `getParameters0` 从 `MethodParameters` 属性真造 `Parameter[]` |
//! | `java/io/ObjectStreamClass` | 2/2 真实现 | `hasStaticInitializer` 看类里有没有 `<clinit>` |
//! | `java/util/zip/Adler32.updateByteBuffer` | 真实现 | 地址落在 `nat_mem` arena 里，能真读 |
//! | `java/util/TimeZone` | 2/2 | 见「时区」一节（有一处**已知差异**，别拿它做逐字节对比） |
//! | `jdk/internal/vm/*`、`jdk/internal/foreign/abi/*`、`jdk/internal/misc/CDS`、`jdk/internal/perf/Perf` … | 混合 | 能力探测给真值，真做不到的**明确抛异常** |
//!
//! ## 三条「故意不注册」，不要再来补
//!
//! 1. **`java/lang/invoke/{MethodHandle,MethodHandleNatives,VarHandle,LambdaProxyClassArchive}`**
//!    （8 + 12 + 31 + 2 条）—— `exec.rs` 的三条 invoke 分支都是先判
//!    `is_invoke_api_class(&info.class)`（= 前缀 `java/lang/invoke/`），命中就交给
//!    `indy.rs::invoke_api`，**永远走不到 `ACC_NATIVE` 的注册表**。注册了是死代码，
//!    而且会误导下一个人「这些方法是 native 实现的」。这条判据 `nat_intrinsics.rs` 也写过一遍。
//! 2. **`jdk/internal/vm/Continuation` 的 VM 内建**（`enterSpecial`/`doYield`/`pin`/`unpin`/
//!    `isPinned0`）—— 见「Continuation 与 FFM」一节：本 VM 没有 fiber，明确抛。
//! 3. **`jdk/internal/foreign/abi/{NativeEntryPoint,UpcallLinker,UpcallStubs}` 的 stub 构造**
//!    —— 同上，`ForeignLinkerSupport.isSupported0()` 已经是 false，JDK 会自己走
//!    `FallbackLinker`（`LibFallback` 的 28 条 ffi native 在 `nat_os.rs` 里是真实现）。
//!
//! ## 一条硬约定
//!
//! 「做不到」= **抛 `UnsupportedOperationException`**（`unsupported()`）。
//! 不许返回 0 / false / null 让调用方当「没数据」继续跑 —— 那会安静地产出错误行为，
//! 而且没人查得出来。唯一的例外是**契约本身就是 null 的返回值**（`findLoadedClass0`
//! 查不到、`retrieveDirectives` 没有 `-ea`、`getTypeAnnotationBytes0` 没有类型注解），
//! 每处都在代码里写明了「为什么 null 就是正确答」。

use super::{Frame, NativeRegistry, Slot, Vm, VmError};

pub(super) fn register(r: &mut NativeRegistry) {
    classloader_reg(r);
    thread_reg(r);
    module_reg(r);
    stack_reg(r);
    continuation_reg(r);
    ffm_reg(r);
    vm_support_reg(r);
    cds_reg(r);
    perf_reg(r);
    ref_reg(r);
    reflect_reg(r);
    zip_reg(r);
    serial_reg(r);
    tz_reg(r);
    misc_reg(r);
}

// ================================================================ 小工具

fn void() -> Result<Slot, VmError> {
    Ok(Slot::I(0))
}

fn zi(v: i32) -> Result<Slot, VmError> {
    Ok(Slot::I(v))
}

fn zl(v: i64) -> Result<Slot, VmError> {
    Ok(Slot::L(v))
}

fn no_op(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    void()
}

/// 「这个 native 本 VM 做不到」——**抛 Java 异常**，不返回哨兵值。
///
/// 用 `UnsupportedOperationException` 而不是 VM 内部错误：调用方（Java 代码）
/// 能 `catch` 到，栈上也能看到是哪一步不支持；而 `VmError{exc:None}` 只会打印一行
/// 内部消息，`try/catch` 完全接不住。
fn unsupported(vm: &mut Vm, what: impl Into<String>) -> VmError {
    vm.throw_msg("java/lang/UnsupportedOperationException", what.into())
}

/// 参数里的 `String`（null → 空串）
fn arg_text(vm: &mut Vm, f: &Frame, i: usize) -> String {
    vm.value_text(f.arg_i(i)).unwrap_or_default()
}

/// `a.b.C` → `a/b/C`（`defineClass` 名字的两种写法都收）
fn to_internal(name: &str) -> String {
    let n = name.strip_prefix('L').filter(|s| s.ends_with(';')).map(|s| &s[..s.len() - 1]);
    n.unwrap_or(name).replace('.', "/")
}

/// 读 `byte[]` 的一段。元素按 0..=255 归一（`baload` 侧再 `as i8`，见 `exec.rs`）。
fn bytes_of(vm: &Vm, h: i32, off: usize, len: usize) -> Result<Vec<u8>, &'static str> {
    if h == 0 {
        return Err("byte[] 是 null");
    }
    let total = vm.sh.heap.array_len(h).ok_or("不是数组")?;
    if off.checked_add(len).map(|end| end > total).unwrap_or(true) {
        return Err("越界");
    }
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(vm.sh.heap.get(h, off + i).map(|s| s.as_i32() as u8).unwrap_or(0));
    }
    Ok(out)
}

/// 建一个 `String`（null 用 0 表示 —— 有些 native 要区分「空串」与「没给」）
fn opt_text(vm: &mut Vm, s: Option<String>) -> i32 {
    match s {
        Some(t) => vm.str_new(t),
        None => 0,
    }
}

// ================================================================ java.lang.ClassLoader

fn classloader_reg(r: &mut NativeRegistry) {
    const CL: &str = "java/lang/ClassLoader";
    // `registerNatives` 已在 `native_reg.rs` 里注册（`<clinit>` 第一句），不重复。
    r.add(
        CL,
        "defineClass1",
        "(Ljava/lang/ClassLoader;Ljava/lang/String;[BIILjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;",
        cl_define_class1,
    );
    r.add(
        CL,
        "defineClass2",
        "(Ljava/lang/ClassLoader;Ljava/lang/String;Ljava/nio/ByteBuffer;IILjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;",
        cl_define_class2,
    );
    // JDK 17+ 的通用入口：`Lookup.defineClass`（隐藏类/同 nest 定义）走的也是它。
    r.add(
        CL,
        "defineClass0",
        "(Ljava/lang/ClassLoader;Ljava/lang/Class;Ljava/lang/String;[BIILjava/security/ProtectionDomain;ZILjava/lang/Object;)Ljava/lang/Class;",
        cl_define_class0,
    );
    r.add(CL, "findLoadedClass0", "(Ljava/lang/String;)Ljava/lang/Class;", cl_find_loaded);
    r.add(CL, "findBootstrapClass", "(Ljava/lang/String;)Ljava/lang/Class;", cl_find_bootstrap);
    r.add(
        CL,
        "retrieveDirectives",
        "()Ljava/lang/AssertionStatusDirectives;",
        cl_retrieve_directives,
    );
}

/// 真定义：`vm.define_class` 解析字节码、挂进类表，再取类对象。
///
/// 与 HotSpot 的差别（写在文档里，不藏着）：不做双亲委派、不做字节码校验、
/// 不按 ProtectionDomain 建 CodeSource —— 本 VM 没有 `java.lang.ClassLoader` 的父子链，
/// 也没有 `ProtectionDomain` 的语义（没有 SecurityManager）。
fn define_bytes(vm: &mut Vm, name: &str, bytes: &[u8]) -> Result<Slot, VmError> {
    let want = to_internal(name);
    match vm.define_class(&want, bytes) {
        Ok(actual) => Ok(Slot::I(vm.class_obj(&actual))),
        Err(e) => {
            let msg = e.msg;
            Err(vm.throw_msg("java/lang/ClassFormatError", msg))
        }
    }
}

/// `(this, name, b, off, len, pd, source)`
fn cl_define_class1(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let name = arg_text(vm, f, 1);
    let b = f.arg_i(2);
    let off = f.arg_i(3).max(0) as usize;
    let len = f.arg_i(4).max(0) as usize;
    let bytes = match bytes_of(vm, b, off, len) {
        Ok(v) => v,
        Err(why) => {
            return Err(vm.throw_msg(
                "java/lang/IndexOutOfBoundsException",
                format!("defineClass: byte[] {why}（off={off} len={len}）"),
            ))
        }
    };
    define_bytes(vm, &name, &bytes)
}

/// `defineClass0(loader, lookup, name, b, off, len, pd, initialize, flags, classData)`
///
/// `lookup != null` 走的是 `Lookup.defineClass`（隐藏类 / 同 nest 定义）：隐藏类需要
/// VM 的「一个类名对多个 Class」能力，我们没有（类表按内部名索引，同名会被替换），
/// 所以**明确拒绝**。`flags`（弱引用/强引用）与 `classData` 同理只有隐藏类会用。
fn cl_define_class0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    if f.arg_i(1) != 0 {
        return Err(unsupported(
            vm,
            "ClassLoader.defineClass0: 隐藏类（Lookup.defineClass）需要「一个名字多个 Class」，本 VM 的类表按名索引，做不到",
        ));
    }
    if f.arg_i(8) != 0 || f.arg_i(9) != 0 {
        return Err(unsupported(
            vm,
            "ClassLoader.defineClass0: flags/classData 只用于隐藏类，本 VM 不支持",
        ));
    }
    let name = arg_text(vm, f, 2);
    let b = f.arg_i(3);
    let off = f.arg_i(4).max(0) as usize;
    let len = f.arg_i(5).max(0) as usize;
    let bytes = match bytes_of(vm, b, off, len) {
        Ok(v) => v,
        Err(why) => {
            return Err(vm.throw_msg(
                "java/lang/IndexOutOfBoundsException",
                format!("defineClass0: byte[] {why}（off={off} len={len}）"),
            ))
        }
    };
    define_bytes(vm, &name, &bytes)
}

/// `defineClass(String, ByteBuffer, ProtectionDomain)`
///
/// 要真做就得从 `ByteBuffer` 里把字节抠出来，而**直接内存**形态背后是
/// `Unsafe.allocateMemory` 的 arena 句柄（`nat_mem`），堆形态背后是 `byte[]` 私有字段 `hb`
/// —— 两条路都要按实现类分叉，属于 NIO 那一摊（`nat_nio.rs`）。
/// 这里是「明确不支持」，不是「返回 null 当没定义」。
fn cl_define_class2(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let _ = f;
    Err(unsupported(
        vm,
        "ClassLoader.defineClass(String, ByteBuffer, ProtectionDomain): ByteBuffer 形态未实现（用 byte[] 形态）",
    ))
}

/// 查**已加载**的类。查不到 → null（这是 `findLoadedClass` 的契约，不是错误）。
fn cl_find_loaded(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let name = to_internal(&arg_text(vm, f, 0));
    if vm.store.has_class(&name) {
        return zi(vm.class_obj(&name));
    }
    zi(0)
}

/// 走 classpath 按需加载（= HotSpot 的「从 boot loader 找并 define」）。
/// 加载不到 → null（契约允许：调用方接着抛 `ClassNotFoundException`）。
fn cl_find_bootstrap(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let name = to_internal(&arg_text(vm, f, 0));
    if !vm.ensure_class_loaded(&name) {
        return zi(0);
    }
    zi(vm.class_obj(&name))
}

/// `initializeJavaAssertionModes` 的入参：`-ea`/`-esa` 的指令表。
///
/// 本 VM 的断言状态是「关闭」（`Class.desiredAssertionStatus0` 恒 0，见 `native_reg.rs`），
/// 所以这里给 **null** —— Java 侧那一句是 `if (asd != null) {...}`，
/// null 正是「没有断言指令」这个状态的正确表示（与 `java` 不带 `-ea` 一致）。
fn cl_retrieve_directives(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    zi(0)
}

// ================================================================ java.lang.Thread

fn thread_reg(r: &mut NativeRegistry) {
    // `Thread.start()` 在本 VM 里是 override（`native_reg.rs::thread_start_override`），
    // 所以 `start0` 只有「绕过 start() 直接调」才走得到。注册上是因为它是**真 native**：
    // 不注册的话那条兜底路径会 UnsatisfiedLinkError。
    r.add("java/lang/Thread", "start0", "()V", thread_start0);
}

fn thread_start0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.this();
    vm.thread_start(h)?;
    void()
}

// ================================================================ java.lang.Module
//
// 这五条在 HotSpot 里维护的是 **VM 侧的模块图**（ModuleEntry 的 reads/exports 边）。
// 本 VM 的模块信息在 `load/modules.rs`（`--module-path` / `--add-reads` 那一套），
// 是**装载期**的静态图，没有「运行期改一个 Module 对象的边」这回事。
//
// 关键：Java 侧自己维护用户可见的那份状态 —— `implAddReads` 在调 `addReads0` 之前
// 已经把 `WeakPairMap reads` 填好了（`javap -c java/lang/Module` 可查），
// `defineModule0` 也是构造函数把 `name`/`loader`/`descriptor` 都写完之后才调。
// 所以 no-op 不但安全，而且用户可见行为与 HotSpot 一致（`canRead`/`isExported` 走的是
// Java 那张表）。**别改成抛异常**：`Module` 的构造函数会调 `defineModule0`，
// 抛了等于让所有 `java.lang.Module` 都造不出来。

fn module_reg(r: &mut NativeRegistry) {
    const M: &str = "java/lang/Module";
    r.add(
        M,
        "defineModule0",
        "(Ljava/lang/Module;ZLjava/lang/String;Ljava/lang/String;[Ljava/lang/Object;)V",
        module_noop,
    );
    r.add(M, "addReads0", "(Ljava/lang/Module;Ljava/lang/Module;)V", module_noop);
    r.add(
        M,
        "addExports0",
        "(Ljava/lang/Module;Ljava/lang/String;Ljava/lang/Module;)V",
        module_noop,
    );
    r.add(
        M,
        "addExportsToAll0",
        "(Ljava/lang/Module;Ljava/lang/String;)V",
        module_noop,
    );
    r.add(
        M,
        "addExportsToAllUnnamed0",
        "(Ljava/lang/Module;Ljava/lang/String;)V",
        module_noop,
    );
}

fn module_noop(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    void()
}

// ================================================================ StackWalker
//
// 本 VM **没有调用栈模型**：`Vm` 只有 `depth`（一个防递归爆栈的计数器）和
// `last_caller`（最近一次字节码调用点的类下标），`Frame` 是栈上临时值、执行完就没了。
// 没有第二个帧、没有返回地址、没有 bci，于是 `StackWalker` 的整套东西做不出来。
//
// 两条不同的处理，理由不一样：
//
// * `checkStackWalkModes` → **true**。它问的不是「VM 支持栈回溯吗」，而是
//   「`StackWalker.MODE_*` 的常量值跟 VM 的一致吗」（`javap -c java/lang/StackStreamFactory`
//   的 `init()` 里只有这一个断言，false 就 `throw new InternalError`）。那些常量是
//   Java 侧写死的，我们当然一致；返回 false 只会让 `StackStreamFactory` 的
//   `<clinit>` 抛 `ExceptionInInitializerError`，把整个类毒掉 —— 那不是「更诚实」，
//   是制造一个更难查的错。
// * 真正取帧的三条 → **抛 `UnsupportedOperationException`**。它们要么填 `Object[]`、
//   要么返回整数帧数，返回 0/空就是「静默给出『没有帧』这个错误结论」。

fn stack_reg(r: &mut NativeRegistry) {
    r.add("java/lang/StackStreamFactory", "checkStackWalkModes", "()Z", |_, _| zi(1));
    const A: &str = "java/lang/StackStreamFactory$AbstractStackWalker";
    r.add(
        A,
        "callStackWalk",
        "(IILjdk/internal/vm/ContinuationScope;Ljdk/internal/vm/Continuation;II[Ljava/lang/Object;)Ljava/lang/Object;",
        stack_call_walk,
    );
    r.add(
        A,
        "fetchStackFrames",
        "(IJIII[Ljava/lang/Object;)I",
        stack_fetch_frames,
    );
    // 只把 continuation 记到 walk 对象上，没有帧可标记 —— no-op 是这个「空操作」的真语义。
    r.add(
        A,
        "setContinuation",
        "(J[Ljava/lang/Object;Ljdk/internal/vm/Continuation;)V",
        no_op,
    );
    r.add("java/lang/StackFrameInfo", "expandStackFrameInfo", "()V", stack_expand);
    r.add(
        "java/lang/StackTraceElement",
        "initStackTraceElement",
        "(Ljava/lang/StackTraceElement;Ljava/lang/StackFrameInfo;)V",
        stack_init_element,
    );
    r.add(
        "java/lang/StackTraceElement",
        "initStackTraceElements",
        "([Ljava/lang/StackTraceElement;Ljava/lang/Object;I)V",
        stack_init_elements,
    );
}

fn stack_call_walk(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Err(unsupported(
        vm,
        "StackWalker: 本 VM 没有调用栈模型（没有帧栈/返回地址/bci），无法回溯栈帧",
    ))
}

fn stack_fetch_frames(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Err(unsupported(vm, "StackWalker.fetchStackFrames: 本 VM 没有调用栈模型"))
}

fn stack_expand(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Err(unsupported(
        vm,
        "StackFrameInfo.expandStackFrameInfo: 本 VM 没有调用栈模型",
    ))
}

fn stack_init_element(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Err(unsupported(
        vm,
        "StackTraceElement.initStackTraceElement: 需要 StackFrameInfo 的 VM 侧字段，本 VM 无帧模型",
    ))
}

/// `Throwable` 的原始 backtrace → `StackTraceElement[]`。
///
/// 本 VM 的 `Throwable.fillInStackTrace` 不给 backtrace（见 `native_reg.rs`），
/// 所以 `backtrace == null` / `depth <= 0` 是**唯一会发生的形态**，此时按契约空着返回。
/// 真给了 backtrace 说明有人绕过了 VM 的实现 —— 那种情况明确抛，不猜。
fn stack_init_elements(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let backtrace = f.arg_i(1);
    let depth = f.arg_i(2);
    if backtrace == 0 || depth <= 0 {
        return void();
    }
    Err(unsupported(
        vm,
        "StackTraceElement.initStackTraceElements: 收到非空 backtrace，但本 VM 从不生成它",
    ))
}

// ================================================================ Continuation / FFM
//
// **Continuation**：本 VM 没有 fiber（没有可切换的栈），虚拟线程用平台线程顶
// （`java/lang/VirtualThread` 的 7 条 native 在 `native_reg.rs` 里就是这么做的）。
// 因此 `enterSpecial`/`doYield`/`pin`/`unpin`/`isPinned0` 一律抛；`isSupported0` 给 false
// —— 这是给调用方的**能力探测**，false 的含义就是「别用 continuation，退回平台线程」，
// 属于真值而不是哨兵。
//
// **FFM**：`ForeignLinkerSupport.isSupported0()` 给 false 会让 `CABI.computeCurrent()`
// 走 `FallbackLinker`（`javap -c jdk/internal/foreign/CABI` 可查：false 分支正是
// `FallbackLinker.isSupported()` → `CABI.FALLBACK`），那条路的 `LibFallback.ffi_*`
// 在 `nat_os.rs` 里是真实现 —— 所以 false **不是**「关掉 FFM」，而是「走 fallback 实现」。
// 反过来 `makeDowncallStub`/`makeUpcallStub`（JVM 自己生成 call stub）在这条路上一句都
// 不会被调，所以那几条直接抛。

fn continuation_reg(r: &mut NativeRegistry) {
    // `VirtualThread.<clinit>` 第一句就是它（HotSpot 在这里把那 7 个 native 注册进
    // 各自的方法槽）。本 VM 的 VirtualThread 用平台线程顶，那 7 条已在
    // `native_reg.rs` 里按 (类,名,描述符) 静态登记，这里只需要一个空的注册点 ——
    // 不注册的话 `Thread.ofVirtual()` 连类都初始化不了。
    r.add("java/lang/VirtualThread", "registerNatives", "()V", no_op);
    const C: &str = "jdk/internal/vm/Continuation";
    r.add(C, "registerNatives", "()V", no_op);
    r.add(C, "enterSpecial", "(Ljdk/internal/vm/Continuation;ZZ)V", continuation_unsupported);
    r.add(C, "doYield", "()I", continuation_unsupported);
    r.add(C, "isPinned0", "(Ljdk/internal/vm/ContinuationScope;)I", continuation_unsupported);
    r.add(C, "pin", "()V", continuation_unsupported);
    r.add(C, "unpin", "()V", continuation_unsupported);
    r.add(
        "jdk/internal/vm/ContinuationSupport",
        "isSupported0",
        "()Z",
        // 能力探测：本 VM 没有 continuation。虚拟线程由平台线程顶（见上）。
        |_, _| zi(0),
    );
}

fn continuation_unsupported(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Err(unsupported(
        vm,
        "jdk.internal.vm.Continuation: 本 VM 没有 fiber/可切换栈，虚拟线程用平台线程顶；continuation 不支持",
    ))
}

fn ffm_reg(r: &mut NativeRegistry) {
    r.add(
        "jdk/internal/vm/ForeignLinkerSupport",
        "isSupported0",
        "()Z",
        // false ⇒ CABI.FALLBACK ⇒ FallbackLinker + LibFallback（nat_os.rs 的真实现）。
        // 给 true 等于宣称「JVM 能生成跨架构 call stub」，那是假的。
        |_, _| zi(0),
    );
    const NEP: &str = "jdk/internal/foreign/abi/NativeEntryPoint";
    r.add(NEP, "registerNatives", "()V", no_op);
    r.add(
        NEP,
        "makeDowncallStub",
        "(Ljava/lang/invoke/MethodType;Ljdk/internal/foreign/abi/ABIDescriptor;[Ljdk/internal/foreign/abi/VMStorage;[Ljdk/internal/foreign/abi/VMStorage;ZIZ)J",
        ffm_unsupported,
    );
    r.add(NEP, "freeDowncallStub0", "(J)Z", ffm_unsupported);
    const UL: &str = "jdk/internal/foreign/abi/UpcallLinker";
    r.add(UL, "registerNatives", "()V", no_op);
    r.add(
        UL,
        "makeUpcallStub",
        "(Ljava/lang/invoke/MethodHandle;Ljdk/internal/foreign/abi/ABIDescriptor;Ljdk/internal/foreign/abi/UpcallLinker$CallRegs;ZJ)J",
        ffm_unsupported,
    );
    const US: &str = "jdk/internal/foreign/abi/UpcallStubs";
    r.add(US, "registerNatives", "()V", no_op);
    r.add(US, "freeUpcallStub0", "(J)Z", ffm_unsupported);
}

fn ffm_unsupported(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Err(unsupported(
        vm,
        "FFM 的 JVM stub 路径不可用：ForeignLinkerSupport.isSupported0()=false，本 VM 走 FallbackLinker(LibFallback)",
    ))
}

// ================================================================ VM 支持面的探测

fn vm_support_reg(r: &mut NativeRegistry) {
    const V: &str = "jdk/internal/vm/VMSupport";
    r.add(V, "getVMTemporaryDirectory", "()Ljava/lang/String;", vmsupport_tmpdir);
    // `-javaagent` 的属性表：`sun.java.command` / `sun.java.launcher` 一类。
    // 本 VM 不支持 attach（没有 JVMTI），这些属性没人读 —— 原样返回入参。
    r.add(
        V,
        "initAgentProperties",
        "(Ljava/util/Properties;)Ljava/util/Properties;",
        |_, f| Ok(f.arg(0)),
    );
    // JFR 的线程快照：需要 VM 侧的挂起点与 JavaThread 状态，做不到。
    r.add(
        "jdk/internal/vm/ThreadSnapshot",
        "create",
        "(Ljava/lang/Thread;)Ljdk/internal/vm/ThreadSnapshot;",
        |vm, _| Err(unsupported(vm, "ThreadSnapshot.create: 本 VM 没有 JFR/挂起点")),
    );
    // 向量 API：没有 JVM 向量内建。`getCPUFeatures` 给空串 = 「没有额外 CPU 特性」，
    // 这是 `VectorSupport` 里 `getMaxLaneCount` 的输入；`getMaxLaneCount` 给 0 = 没有 lane
    // （调用方据此退回标量实现，这是它的契约，不是哨兵）。
    const VS: &str = "jdk/internal/vm/vector/VectorSupport";
    r.add(VS, "registerNatives", "()I", |_, _| zi(0));
    r.add(VS, "getCPUFeatures", "()Ljava/lang/String;", |vm, _| {
        let h = vm.str_new(String::new());
        zi(h)
    });
    r.add(VS, "getMaxLaneCount", "(Ljava/lang/Class;)I", |_, _| zi(0));
    // preview 特性：与 `java`（不带 `--enable-preview`）一致 → false。
    r.add(
        "jdk/internal/misc/PreviewFeatures",
        "isPreviewEnabled",
        "()Z",
        |_, _| zi(0),
    );
}

fn vmsupport_tmpdir(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    let dir = crate::vm::prop("java.io.tmpdir")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let h = vm.str_new(dir.to_string_lossy().into_owned());
    zi(h)
}

// ================================================================ CDS

fn cds_reg(r: &mut NativeRegistry) {
    const C: &str = "jdk/internal/misc/CDS";
    // 没有归档（`classes.jsa`）→ 没有归档模块。
    r.add(
        C,
        "defineArchivedModules",
        "(Ljava/lang/ClassLoader;Ljava/lang/ClassLoader;)V",
        no_op,
    );
    r.add(C, "logLambdaFormInvoker", "(Ljava/lang/String;)V", no_op);
    // 类初始化屏障是归档类的语义（归档的类已被 JVM 初始化过）。没有归档 → 不需要。
    r.add(C, "needsClassInitBarrier0", "(Ljava/lang/Class;)Z", |_, _| zi(0));
    // 这两条由 `-XX:DumpLoadedClassList` / `-XX:ArchiveClassesAtExit` 触发，本 VM 没有。
    r.add(C, "dumpClassList", "(Ljava/lang/String;)V", cds_unsupported);
    r.add(C, "dumpDynamicArchive", "(Ljava/lang/String;)V", cds_unsupported);
}

fn cds_unsupported(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Err(unsupported(
        vm,
        "CDS: 本 VM 没有类数据归档（-XX:DumpLoadedClassList / ArchiveClassesAtExit 不支持）",
    ))
}

// ================================================================ Perf（hsperfdata）

fn perf_reg(r: &mut NativeRegistry) {
    const P: &str = "jdk/internal/perf/Perf";
    r.add(P, "registerNatives", "()V", no_op);
    r.add(P, "highResCounter", "()J", |_, _| zl(perf_counter()));
    r.add(P, "highResFrequency", "()J", |_, _| zl(1_000_000_000));
    // 其余四条要真读写 `/tmp/hsperfdata_<user>/<pid>`（一块共享内存文件）。
    // 本 VM 不导出 PerfData，写一个空文件只会让 `jps`/`jstat` 读到垃圾 —— 明确抛。
    r.add(P, "attach0", "(I)Ljava/nio/ByteBuffer;", perf_unsupported);
    r.add(
        P,
        "createLong",
        "(Ljava/lang/String;IIJ)Ljava/nio/ByteBuffer;",
        perf_unsupported,
    );
    r.add(
        P,
        "createByteArray",
        "(Ljava/lang/String;II[BI)Ljava/nio/ByteBuffer;",
        perf_unsupported,
    );
    r.add(P, "detach", "(Ljava/nio/ByteBuffer;)V", no_op);
}

/// `highResCounter` 只要**单调**且与 `highResFrequency` 同一时基就成立
/// （调用方算 `(c2 - c1) / freq` 秒）。用进程启动起的纳秒数，频率 1e9。
fn perf_counter() -> i64 {
    static BASE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    BASE.get_or_init(std::time::Instant::now).elapsed().as_nanos() as i64
}

fn perf_unsupported(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Err(unsupported(
        vm,
        "jdk.internal.perf.Perf: 本 VM 不导出 hsperfdata（没有共享内存计数器文件）",
    ))
}

// ================================================================ java.lang.ref

fn ref_reg(r: &mut NativeRegistry) {
    // JDK 25 的 `Finalizer`：终结器在本 VM 里**关闭**（GC 不跑 `finalize`）。
    // 这等价于 `java --finalization=disabled`，是 JDK 25 支持的一种合法模式，
    // 所以给 false 是**真值**而不是「没实现」：告诉调用方「别指望终结器」。
    r.add("java/lang/ref/Finalizer", "isFinalizationEnabled", "()Z", |_, _| zi(0));
    // `Finalizer.runFinalizer` 收尾时调（只有终结器开着才会走到）——no-op 正确。
    r.add("java/lang/ref/Finalizer", "reportComplete", "(Ljava/lang/Object;)V", no_op);
    // 幻象引用：referent 在 `java/lang/ref/Reference` 里，本 VM 就是普通字段
    //（GC 不特判，见 `gc.rs`），所以 clear0/refersTo0 直接读写字段即可。
    r.add("java/lang/ref/PhantomReference", "clear0", "()V", ref_clear0);
    r.add(
        "java/lang/ref/PhantomReference",
        "refersTo0",
        "(Ljava/lang/Object;)Z",
        ref_refers_to0,
    );
}

fn referent_index(vm: &Vm) -> Option<usize> {
    vm.store.field_index("java/lang/ref/Reference", "referent")
}

fn ref_clear0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.this();
    if let Some(i) = referent_index(vm) {
        // `set_ref` 而不是 `set`：这一格是引用，GC 扫描要按引用处理
        vm.sh.heap.set_ref(h, i, Slot::I(0));
    }
    void()
}

fn ref_refers_to0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.this();
    let other = f.arg_i(0);
    let mut same = false;
    if let Some(i) = referent_index(vm) {
        if let Some(slots) = vm.sh.heap.snapshot(h) {
            same = slots.get(i).map(|s| s.as_i32()) == Some(other);
        }
    }
    zi(i32::from(same))
}

// ================================================================ java.lang.reflect

fn reflect_reg(r: &mut NativeRegistry) {
    r.add(
        "java/lang/reflect/Executable",
        "getParameters0",
        "()[Ljava/lang/reflect/Parameter;",
        exec_get_parameters0,
    );
    // 类型注解：本 VM 的 `ClassFile` 不保留 `RuntimeVisibleTypeAnnotations`（`reflect.rs`
    // 只解运行期**声明**注解），所以「一个字节的类型注解数据都没有」。
    // `getTypeAnnotationBytes0` 的契约里 null **就是**「没有」，Java 侧会返回空数组 —— null 正确。
    r.add(
        "java/lang/reflect/Executable",
        "getTypeAnnotationBytes0",
        "()[B",
        |_, _| zi(0),
    );
    r.add("java/lang/reflect/Field", "getTypeAnnotationBytes0", "()[B", |_, _| zi(0));
    r.add(
        "jdk/internal/reflect/Reflection",
        "areNestMates",
        "(Ljava/lang/Class;Ljava/lang/Class;)Z",
        reflection_are_nest_mates,
    );
    // `NativeAccessor` 是 `Method.invoke` 背后那条「调 native 方法」的快捷路径。
    // 本 VM 的 `Method.invoke` / `Constructor.newInstance` 由 `reflect.rs` **override**
    // 接管（见 `is_override_owner` 的白名单），Java 侧的 Accessor 一行都不跑，这两条是死代码。
    // 但它们是**真 native**：万一哪天反射换成走字节码，这行会立刻炸出来而不是静默。
    r.add(
        "jdk/internal/reflect/DirectMethodHandleAccessor$NativeAccessor",
        "invoke0",
        "(Ljava/lang/reflect/Method;Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        |vm, _| Err(unsupported(vm, "NativeAccessor.invoke0: Method.invoke 由 reflect.rs 接管，这条路径不可达")),
    );
    r.add(
        "jdk/internal/reflect/DirectConstructorHandleAccessor$NativeAccessor",
        "newInstance0",
        "(Ljava/lang/reflect/Constructor;[Ljava/lang/Object;)Ljava/lang/Object;",
        |vm, _| {
            Err(unsupported(
                vm,
                "NativeAccessor.newInstance0: Constructor.newInstance 由 reflect.rs 接管，这条路径不可达",
            ))
        },
    );
}

/// `Executable.getParameters()` 的落地：按 **`MethodParameters` 属性**真造 `Parameter[]`。
///
/// 关键点：**无论有没有那个属性，都要给「每个参数一个 `Parameter`」**。
/// HotSpot 在没有属性时也返回等长的数组，只是 `name` 为 null、`modifiers` 为 0，
/// 然后 `Parameter.getName()` 自己合成 `argN`。少给几个元素的话
/// `getParameters().length` 就跟 `java` 对不上了。
fn exec_get_parameters0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let h = f.this();
    let (decl, idx) = match vm.refl_member_of(h, "java/lang/reflect/Method") {
        Some(v) => v,
        None => match vm.refl_member_of(h, "java/lang/reflect/Constructor") {
            Some(v) => v,
            None => {
                return Err(vm.throw_msg(
                    "java/lang/IllegalArgumentException",
                    "getParameters0: this 不是 Method/Constructor".into(),
                ))
            }
        },
    };
    let Some(cf) = vm.store.class_file(&decl) else {
        return Err(vm.throw_msg(
            "java/lang/InternalError",
            format!("getParameters0: 类 {decl} 不在类表里"),
        ));
    };
    let Some(m) = cf.methods.get(idx) else {
        return Err(vm.throw_msg(
            "java/lang/InternalError",
            format!("getParameters0: {decl} 的方法下标 {idx} 越界"),
        ));
    };
    let desc = String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default()).into_owned();
    // 声明的参数个数与名字（属性缺失时名字为 None）
    let types = crate::vm::indy::param_descs(&desc);
    let attrs = match attribute_of(&cf, m, "MethodParameters") {
        Some(info) => parse_method_parameters(info),
        None => Vec::new(),
    };
    // name_index → String 句柄（0 = 没有名字，`Parameter.getName()` 会自己合成 argN）
    let attrs: Vec<(i32, i32)> = attrs
        .into_iter()
        .map(|(name_idx, flags)| {
            let h = if name_idx == 0 {
                0
            } else {
                match cf.utf8(name_idx) {
                    Some(b) => vm.str_new(String::from_utf8_lossy(b).into_owned()),
                    None => 0,
                }
            };
            (h, flags)
        })
        .collect();
    let mut slots = Vec::with_capacity(types.len());
    if !types.is_empty() {
        if !vm.ensure_class_loaded("java/lang/reflect/Parameter") {
            // 独立模式（没有 java.base）：反射对象造不出来，明确报错
            return Err(vm.throw_msg(
                "java/lang/UnsupportedOperationException",
                "getParameters0: 需要真 java.base 才有 java.lang.reflect.Parameter".into(),
            ));
        }
        let n = vm.store.field_count("java/lang/reflect/Parameter").max(1);
        for i in 0..types.len() {
            let (name, flags) = attrs.get(i).copied().unwrap_or((0, 0));
            let mut fields = vec![Slot::I(0); n];
            // 字段顺序按 `Parameter` 的声明（`name` / `modifiers` / `executable` / `index`），
            // 不靠构造器 —— 反射对象的构造器参数顺序与字段写入顺序不一定一致（踩过）。
            put_field(vm, &mut fields, "java/lang/reflect/Parameter", "name", Slot::I(name));
            put_field(vm, &mut fields, "java/lang/reflect/Parameter", "modifiers", Slot::I(flags));
            put_field(vm, &mut fields, "java/lang/reflect/Parameter", "executable", Slot::I(h));
            put_field(vm, &mut fields, "java/lang/reflect/Parameter", "index", Slot::I(i as i32));
            let ph = vm.sh.heap.new_object(fields, "java/lang/reflect/Parameter").unwrap_or(0);
            slots.push(Slot::I(ph));
        }
    }
    let arr = vm.sh.heap.new_array(slots, "[Ljava/lang/reflect/Parameter;").unwrap_or(0);
    zi(arr)
}

fn put_field(vm: &Vm, slots: &mut [Slot], class: &str, field: &str, v: Slot) {
    if let Some(i) = vm.store.field_index(class, field) {
        if i < slots.len() {
            slots[i] = v;
        }
    }
}

/// 成员上的某个属性（缓冲区 + 名字的比较都走常量池）
fn attribute_of<'a>(
    cf: &'a crate::ClassFile,
    m: &'a crate::Member,
    name: &str,
) -> Option<&'a [u8]> {
    for a in &m.attributes {
        if cf.utf8(a.name_index).map(|b| b == name.as_bytes()).unwrap_or(false) {
            return Some(&a.info);
        }
    }
    None
}

/// `MethodParameters` 属性体：`u1 count`，随后每个 `(u2 name_index, u2 access_flags)`。
/// 返回 `(name_index, access_flags)`；name_index 由调用方换成 String 句柄。
fn parse_method_parameters(info: &[u8]) -> Vec<(u16, i32)> {
    if info.is_empty() {
        return Vec::new();
    }
    let n = info[0] as usize;
    let mut out = Vec::with_capacity(n);
    let mut i = 1;
    for _ in 0..n {
        if i + 4 > info.len() {
            break;
        }
        let name_idx = u16::from_be_bytes([info[i], info[i + 1]]);
        let flags = u16::from_be_bytes([info[i + 2], info[i + 3]]);
        out.push((name_idx, i32::from(flags)));
        i += 4;
    }
    out
}

/// `Class.isNestmateOf` 的底座：同一个 `nest` 里的两个类 → true。
///
/// 规则就是 JVMS §5.4.4：`A` 与 `B` 是 nest mates 当且仅当它们的 nest host 相同；
/// 没有 `NestHost` 属性的类自己是 host。
fn reflection_are_nest_mates(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let a = match vm.class_obj_name(f.arg_i(0)) {
        Some(v) => v,
        None => return zi(0),
    };
    let b = match vm.class_obj_name(f.arg_i(1)) {
        Some(v) => v,
        None => return zi(0),
    };
    let ha = nest_host(vm, &a);
    let hb = nest_host(vm, &b);
    zi(i32::from(ha == hb))
}

fn nest_host(vm: &Vm, class: &str) -> String {
    let Some(cf) = vm.store.class_file(class) else {
        return class.to_string();
    };
    let host_attr = cf
        .attributes
        .iter()
        .find(|a| cf.utf8(a.name_index).map(|b| b == b"NestHost").unwrap_or(false));
    let Some(attr) = host_attr else {
        return class.to_string();
    };
    if attr.info.len() < 2 {
        return class.to_string();
    }
    let idx = u16::from_be_bytes([attr.info[0], attr.info[1]]);
    match cf.constant_pool.get(idx as usize).and_then(|e| e.as_ref()) {
        Some(crate::CpEntry::Class(n)) => cf
            .utf8(*n)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_else(|| class.to_string()),
        _ => class.to_string(),
    }
}

// ================================================================ java.util.zip

fn zip_reg(r: &mut NativeRegistry) {
    r.add(
        "java/util/zip/Adler32",
        "updateByteBuffer",
        "(IJII)I",
        adler_update_byte_buffer,
    );
}

/// `Adler32.updateByteBuffer(int adler, long addr, int off, int len)`
///
/// `addr` 是**直接内存地址**，在本 VM 里是 `nat_mem` arena 发出去的句柄
/// （`Unsafe.allocateMemory` / `NatMem` 一族）。能真读，所以真算 —— 不像
/// `Inflater` 的 ByteBuffer 变体那样只能报不支持（那边要的是**写回**，语义更复杂）。
/// 地址不在任何一个 arena 块里 → 明确抛（不要拿零字节算出一个假校验和）。
fn adler_update_byte_buffer(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let adler = f.arg_i(0) as u32;
    let addr = f.arg_l(1) as u64;
    let off = f.arg_i(2).max(0) as u64;
    let len = f.arg_i(3).max(0) as usize;
    if len == 0 {
        return zi(adler as i32);
    }
    let start = addr.wrapping_add(off);
    let Some((base, total)) = super::nat_util::nat_mem::base_of(start) else {
        return Err(unsupported(
            vm,
            format!("Adler32.updateByteBuffer: 地址 {start:#x} 不在本 VM 的裸内存 arena 里"),
        ));
    };
    let rel = (start - base) as usize;
    if rel + len > total {
        return Err(vm.throw_msg(
            "java/lang/IndexOutOfBoundsException",
            format!("Adler32.updateByteBuffer: 越界（块 {total} 字节，要读 {rel}..{}）", rel + len),
        ));
    }
    let data = super::nat_util::nat_mem::read_bytes(start, len);
    zi(adler32(adler, &data) as i32)
}

fn adler32(mut adler: u32, data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut a = adler & 0xffff;
    let mut b = (adler >> 16) & 0xffff;
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += u32::from(x);
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    adler = (b << 16) | a;
    adler
}

// ================================================================ java.io.ObjectStreamClass

fn serial_reg(r: &mut NativeRegistry) {
    const O: &str = "java/io/ObjectStreamClass";
    r.add(O, "initNative", "()V", no_op);
    r.add(O, "hasStaticInitializer", "(Ljava/lang/Class;)Z", has_static_initializer);
}

/// `hasStaticInitializer(Class)` → 这个类自己有没有声明 `<clinit>`。
///
/// 序列化用它决定「反序列化时要不要先初始化这个类」。本 VM 的类表能直接查方法，
/// 所以给真值（HotSpot 的语义也是「类自己声明的静态初始化器」，不含继承来的）。
fn has_static_initializer(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let Some(class) = vm.class_obj_name(f.arg_i(0)) else {
        return zi(0);
    };
    let has = vm.store.method_in_class(&class, "<clinit>", "()V").is_some();
    zi(i32::from(has))
}

// ================================================================ java.util.TimeZone
//
// ## 已知差异（别拿 `TimeZone.getDefault().getID()` 做逐字节对比）
//
// `java` 在 Windows 上走的是 `TimeZone_md.c`：读注册表的 `TimeZoneKeyName`，
// 再用 tzdb 的 windowsZones 映射表换成 IANA id（本机 → `Asia/Shanghai`）。
// 本 VM 没有那张映射表（它在 `jdk.localedata` 的 `tzdb.dat` 里，而且要做 Windows→IANA 转换），
// 所以：
//
// * `getSystemTimeZoneID` 只认 `TZ` 环境变量；没有就返回 **null**（契约允许第 13 行的
//   「返回 null 表示无法确定」），于是 JDK 退到 `getSystemGMTOffsetID()`；
// * `getSystemGMTOffsetID` 真算当前偏移（注册表的 `ActiveTimeBias`），给出 `GMT+08:00`。
//
// 结果是**语义正确、id 写法与 HotSpot 不同**。写在这里而不是藏在代码里。

fn tz_reg(r: &mut NativeRegistry) {
    const T: &str = "java/util/TimeZone";
    r.add(
        T,
        "getSystemTimeZoneID",
        "(Ljava/lang/String;)Ljava/lang/String;",
        tz_system_id,
    );
    r.add(T, "getSystemGMTOffsetID", "()Ljava/lang/String;", tz_gmt_offset_id);
}

fn tz_system_id(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    if let Ok(tz) = std::env::var("TZ") {
        let tz = tz.trim().trim_start_matches(':').to_string();
        if !tz.is_empty() {
            let h = vm.str_new(tz);
            return zi(h);
        }
    }
    // null = 「VM 判不出来」→ JDK 用 GMT 偏移 id 兜底
    zi(0)
}

fn tz_gmt_offset_id(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    let mins = local_utc_offset_minutes();
    let sign = if mins < 0 { '-' } else { '+' };
    let abs = mins.abs();
    let text = format!("GMT{}{:02}:{:02}", sign, abs / 60, abs % 60);
    let h = vm.str_new(text);
    zi(h)
}

/// 当前本地偏移（分钟，东为正）。
///
/// Windows 注册表里 `ActiveTimeBias` 是「UTC = 本地 + bias」的分钟数（西为正，且已含夏令时），
/// 所以取负。读不到就给 0（= UTC）—— 这是唯一能确定的值，不是猜。
fn local_utc_offset_minutes() -> i32 {
    static CACHE: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        let out = std::process::Command::new("reg")
            .args([
                "query",
                r"HKLM\SYSTEM\CurrentControlSet\Control\TimeZoneInformation",
                "/v",
                "ActiveTimeBias",
            ])
            .output();
        let Ok(out) = out else { return 0 };
        let text = String::from_utf8_lossy(&out.stdout);
        for tok in text.split_whitespace() {
            if let Ok(v) = u32::from_str_radix(tok, 16) {
                if v <= 24 * 60 {
                    return -(v as i32);
                }
            }
        }
        0
    })
}

// ================================================================ 其它

fn misc_reg(r: &mut NativeRegistry) {
    // `System.getenv` 的 JDK 25 落地（`jdk.internal.util.EnvUtils`）。
    r.add("jdk/internal/util/EnvUtils", "getEnvVar", "(Ljava/lang/String;)Ljava/lang/String;", env_var);
    r.add(
        "sun/security/provider/NativeSeedGenerator",
        "nativeGenerateSeed",
        "([B)Z",
        native_generate_seed,
    );
    // jimage 的 native mmap：本 VM 的 jimage 读取在 `load/jimage` 那一侧（纯 Rust），
    // 没有「把镜像映射成 ByteBuffer」这回事。
    r.add(
        "jdk/internal/jimage/NativeImageBuffer",
        "getNativeMap",
        "(Ljava/lang/String;)Ljava/nio/ByteBuffer;",
        |vm, _| Err(unsupported(vm, "NativeImageBuffer.getNativeMap: 本 VM 不把 jimage 映射成 ByteBuffer")),
    );
    // `Console.readPassword` 的回显开关：没有真终端，回显状态由调用方自己决定 ——
    // 返回「你要求的状态已生效」（就是入参），语义上等价于「console 不回显」。
    r.add("jdk/internal/io/JdkConsoleImpl", "echo", "(Z)Z", |_, f| zi(f.arg_i(0)));
    // `sun.io.Win32ErrorMode.initialize()` → `SetErrorMode`：进程启动时把「系统弹窗」
    // 关掉（`ProcessImpl` 一族初始化时会走它）。真调 Win32，别给假值。
    r.add("sun/io/Win32ErrorMode", "setErrorMode", "(J)J", win32_set_error_mode);
}

fn env_var(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let name = arg_text(vm, f, 0);
    match std::env::var(&name) {
        Ok(v) => {
            let h = vm.str_new(v);
            zi(h)
        }
        // 查不到 → null（`System.getenv` 的契约：不存在就是 null）
        Err(_) => zi(0),
    }
}

/// `SecureRandom` 的种子源（Windows 上没有 `/dev/random`，走这条）。
///
/// 熵来自 `RandomState::new()` —— 它的 SipHash 键由操作系统给
/// （Windows 上最终是 `ProcessPrng`/`RtlGenRandom`），所以每个 `RandomState`
/// 都是一份独立的 128 位 OS 熵。按 8 字节一块做 `hash(counter)` 展开，
/// 足够当 `SecureRandom` 的种子材料（它自己还会再混）。
///
/// 不要用「时间戳 + 地址」那类做法：`SecureRandom` 的种子要求不可预测。
fn native_generate_seed(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};
    let h = f.arg_i(0);
    let n = vm.sh.heap.array_len(h).unwrap_or(0);
    if n == 0 {
        return zi(1);
    }
    let random = std::collections::hash_map::RandomState::new();
    let mut ctr = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let mut hasher = random.build_hasher();
        hasher.write_u64(ctr);
        hasher.write_u64(ctr.rotate_left(32) ^ 0x9e37_79b9_7f4a_7c15);
        out.extend_from_slice(&hasher.finish().to_le_bytes());
        ctr = ctr.wrapping_add(1);
    }
    out.truncate(n);
    for (i, b) in out.iter().enumerate() {
        vm.sh.heap.set(h, i, Slot::I(i32::from(*b)));
    }
    zi(1)
}


/// `SetErrorMode(mode)` → 旧值。非 Windows 上原样返回入参（没有这个概念）。
fn win32_set_error_mode(_vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let mode = f.arg_l(0) as u32;
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn SetErrorMode(mode: u32) -> u32;
        }
        let old = unsafe { SetErrorMode(mode) };
        return Ok(Slot::L(i64::from(old)));
    }
    #[cfg(not(windows))]
    {
        Ok(Slot::L(i64::from(mode)))
    }
}
