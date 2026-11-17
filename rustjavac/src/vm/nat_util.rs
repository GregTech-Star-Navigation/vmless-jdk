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
//! | F | `ScopedMemoryAccess` / `Unsafe` 内存族 | 直接内存（`ZipUtils.defaultBuf` 用 `allocateDirect`） |
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

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
    for (n, d) in [("getuid", "()J"), ("geteuid", "()J"), ("getgid", "()J"), ("getegid", "()J")] {
        r.add(vm, n, d, |_, _| Ok(Slot::L(0)));
    }
    // 启动参数：我们没有 JVM 参数（`-D` 走属性表），给空数组而不是 null
    r.add(vm, "getRuntimeArguments", "()[Ljava/lang/String;", |vm, _| {
        Ok(Slot::I(vm.sh.heap.new_array(Vec::new(), "[Ljava/lang/String;").unwrap_or(0)))
    });

    // `System.lineSeparator()` 读的是 `System.lineSeparator` **静态字段**，而它由
    // `System.initPhase1`（启动流程）赋值 —— 我们没有那段启动流程，字段永远是 null。
    // 属性表里 `line.separator` 是对的，所以这里按属性值直接给（等同 initPhase1 的那一步）。
    r.add_override("java/lang/System", "lineSeparator", "()Ljava/lang/String;", |vm, _| {
        let sep = crate::vm::property("line.separator").unwrap_or_else(|| "
".to_string());
        let h = vm.str_new(sep);
        Ok(Slot::I(h))
    });

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
    r.add(t, "start0", "()V", thread_start0);
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
    /// 位读取器残留（跨调用）
    bitbuf: u32,
    bitcnt: u32,
    /// stored 块剩余字节（`NONE` = 还没读 LEN/NLEN）
    stored_left: u32,
    lit: Huff,
    dist: Huff,
    /// 回溯窗口（最多 32K）；`setDictionary` 时当成前缀
    win: Vec<u8>,
}

const STORED_NONE: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq)]
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
struct Bits<'a> {
    input: &'a [u8],
    pos: usize,
    buf: u32,
    cnt: u32,
}

impl<'a> Bits<'a> {
    fn refill(&mut self) {
        while self.cnt <= 24 && self.pos < self.input.len() {
            self.buf |= (self.input[self.pos] as u32) << self.cnt;
            self.pos += 1;
            self.cnt += 8;
        }
    }

    /// 取 `n`（≤16）位。`None` = 输入不够（**不消耗**任何位，所以调用点可以安全重试）。
    fn take(&mut self, n: u32) -> Option<u32> {
        if n == 0 {
            return Some(0);
        }
        self.refill();
        if self.cnt < n {
            return None;
        }
        let v = self.buf & ((1u32 << n) - 1);
        self.buf >>= n;
        self.cnt -= n;
        Some(v)
    }

    /// 丢掉制裁到字节边界的残余位，并把「读进来但没用的整字节」还给 `pos`
    fn byte_align(&mut self) {
        let r = self.cnt % 8;
        if r > 0 {
            self.buf >>= r;
            self.cnt -= r;
        }
        let k = (self.cnt / 8) as usize;
        if k > 0 {
            self.cnt -= 8 * k as u32;
            self.pos = self.pos.saturating_sub(k);
            self.buf = if self.cnt == 0 { 0 } else { self.buf & ((1u32 << self.cnt) - 1) };
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
        let k = (self.cnt / 8) as usize;
        if k > 0 {
            self.cnt -= 8 * k as u32;
            self.pos = self.pos.saturating_sub(k);
            self.buf = if self.cnt == 0 { 0 } else { self.buf & ((1u32 << self.cnt) - 1) };
        }
        self.pos
    }
}

enum Dec {
    Sym(u16),
    NeedInput,
    Bad,
}

/// 规范哈夫曼解码（puff 的 counts/symbols 算法，按位读）
fn decode(b: &mut Bits, h: &Huff) -> Dec {
    if h.empty() {
        return Dec::Bad;
    }
    let mut code: i32 = 0;
    let mut first: i32 = 0;
    let mut index: i32 = 0;
    for len in 1..16usize {
        let bit = match b.take(1) {
            Some(v) => v,
            None => return Dec::NeedInput,
        };
        code |= bit as i32;
        let count = h.counts[len] as i32;
        if code - first < count {
            let i = (index + (code - first)) as usize;
            return match h.symbols.get(i) {
                Some(s) => Dec::Sym(*s),
                None => Dec::Bad,
            };
        }
        index += count;
        first = (first + count) << 1;
        code <<= 1;
    }
    Dec::Bad
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
                while produced < out.len() {
                    let sym = match decode(&mut b, &inf.lit) {
                        Dec::Sym(s) => s,
                        Dec::NeedInput => break,
                        Dec::Bad => {
                            err = Some(ZErr::Bad);
                            break;
                        }
                    };
                    if sym < 256 {
                        push_out(&mut inf.win, out, &mut produced, sym as u8);
                        continue;
                    }
                    if sym == 256 {
                        done_block = true;
                        break;
                    }
                    let li = (sym - 257) as usize;
                    if li >= 29 {
                        err = Some(ZErr::Bad);
                        break;
                    }
                    let len = LEN_BASE[li] as usize
                        + match b.take(LEN_EXTRA[li] as u32) {
                            Some(v) => v as usize,
                            None => break,
                        };
                    let dsym = match decode(&mut b, &inf.dist) {
                        Dec::Sym(s) => s as usize,
                        Dec::NeedInput => break,
                        Dec::Bad => {
                            err = Some(ZErr::Bad);
                            break;
                        }
                    };
                    if dsym >= 30 {
                        err = Some(ZErr::Bad);
                        break;
                    }
                    let dist = DIST_BASE[dsym] as usize
                        + match b.take(DIST_EXTRA[dsym] as u32) {
                            Some(v) => v as usize,
                            None => break,
                        };
                    if dist == 0 || dist > inf.win.len() {
                        err = Some(ZErr::Bad);
                        break;
                    }
                    for _ in 0..len {
                        if produced >= out.len() {
                            break;
                        }
                        let byte = inf.win[inf.win.len() - dist];
                        push_out(&mut inf.win, out, &mut produced, byte);
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
    let types = [
        "ffi_type_void",
        "ffi_type_uint8",
        "ffi_type_sint8",
        "ffi_type_uint16",
        "ffi_type_sint16",
        "ffi_type_uint32",
        "ffi_type_sint32",
        "ffi_type_uint64",
        "ffi_type_sint64",
        "ffi_type_float",
        "ffi_type_double",
        "ffi_type_pointer",
    ];
    // 注意：捕获变量的闭包转不成 `fn` 指针，所以逐个写死（id 只是「非 0 假地址」）
    for (i, n) in types.iter().enumerate() {
        let id = (0x1000_0000u64 + i as u64) as i64;
        r.add(fb, n, "()J", const_type_addr(id));
    }
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
    let arr = "(Ljava/lang/String;[Ljava/lang/String;)[Ljava/lang/String;";
    r.add(h, "getAmPmStrings", arr, hl_ampm);
    r.add(h, "getEras", arr, hl_eras);
    r.add(h, "getMonths", arr, hl_months);
    r.add(h, "getShortMonths", arr, hl_short_months);
    r.add(h, "getWeekdays", arr, hl_weekdays);
    r.add(h, "getShortWeekdays", arr, hl_short_weekdays);
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
// 这里只补两个 `Unsafe` 之外、但 zip 初始化就需要的点。

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
        assert_eq!(pack(0, 1, false, false), 1 << 31);
        assert_eq!(pack(0, 0, true, false), 1 << 62);
        assert_eq!(pack(0, 0, false, true), 1u64 as i64 << 63);
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
        let mut out = vec![0u8; src.len()];
        let (used, produced) = match inflate_run(&mut inf, &d.out_pending, &mut out) {
            Ok(v) => v,
            Err(_) => panic!("inflate 失败"),
        };
        assert_eq!(produced, src.len());
        assert_eq!(&out[..], &src[..]);
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
}
