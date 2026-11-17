//! native 实现分组：nat_intrinsics
//!
//! 由 `native_reg.rs` 的 `register_all` 末尾调用 `register(r)`。
//! 这样几条并行工作线各改各的文件，不用碰 `native_reg.rs`。
//!
//! 覆盖面：java.base 里「内存与反射核心」那一组 native。
//!
//! | 类 | native 数 | 说明 |
//! | --- | --- | --- |
//! | `jdk/internal/misc/Unsafe` | 68/68 | 内存读写 + 裸内存（malloc 替身）+ CAS + 栅栏 |
//! | `java/lang/reflect/Array` | 21/21 | **`newArray` 是 `Arrays.copyOf` / `String.join` 的关键** |
//! | `java/lang/Class` | 28/28 | 补齐 `reflect.rs` 没管的 12 个 |
//! | `jdk/internal/reflect/ConstantPool` | 18/18 | 默认值（本 VM 的 `getConstantPool` 不承载常量池） |
//! | `java/lang/invoke/{VarHandle,MethodHandle,MethodHandleNatives}` | **0** | 故意不注册，见文件里那节说明（`invoke_api` 抢先截走） |
//!
//! ## 两个地址空间
//!
//! 我们的堆是 `Vec<Slot>`，**没有裸地址**。所以 `Unsafe` 的 `offset` 按对象分两种口径：
//!
//! * `o != null` → `offset` 是**槽下标**（对象字段）或**字节偏移**（数组，见 `slot_index`）；
//! * `o == null` → `offset` 是**裸地址**，落进本文件末尾那个 arena。
//!
//! 第二条是「`allocateMemory` + `putLong`/`getLong` + `freeMemory` 往返」的前提 ——
//! `Unsafe.putLong(long address, long x)` 这些单参数重载在 JDK 里就是
//! `putLong(null, address, x)` 的包装，所以 `o == null` 必须当作裸内存而不是「越界」。

use super::{Frame, NativeRegistry, RefKind, Slot, Vm, VmError};
use std::sync::atomic::{fence, Ordering};

pub(super) fn register(r: &mut NativeRegistry) {
    unsafe_reg(r);
    array_reg(r);
    class_reg(r);
    constant_pool_reg(r);
}

// ================================================================ 裸内存 arena
//
// `Unsafe.allocateMemory` 在 HotSpot 里就是 `malloc`。我们没有裸指针可用
// （要过 GC / 多线程 / 不许 UB），所以发出去的「地址」是**句柄**：`BASE + 块内偏移`。
// 块首按 16 字节对齐，`getLong`/`getDouble` 不会跨块。
// 共享的「假」native 内存 arena —— **全 VM 一份**（`src/vm/nat_mem.rs`，由 `nat_util.rs` 声明）。
// 原来这里是本文件私有的一份，`nat_os` 组读不到同一张表，`Files.readAllBytes` /
// `sun.nio.ch.*` 全废。现在两边落到同一张块表上：谁分配的地址谁都能读写。
use super::nat_util::nat_mem as arena;

// ================================================================ Unsafe — 读写底座

/// 内存序。本 VM 的共享状态都走互斥量（本身是全屏障），所以这里的栅栏是
/// **双保险 + 明确的编译器屏障** —— 不能靠「反正加了锁」的隐式结论，
/// `volatile` 的语义要求「不重排、立即可见」，得写出来。
#[derive(Clone, Copy, PartialEq)]
enum Ord {
    Plain,
    /// Java `volatile` = `SeqCst`
    Volatile,
    /// Java `getAcquire`
    Acquire,
    /// Java `setRelease`
    Release,
    /// Java `opaque` = `Relaxed`
    Opaque,
}

fn ordering(o: Ord) -> Ordering {
    match o {
        Ord::Volatile => Ordering::SeqCst,
        Ord::Acquire => Ordering::Acquire,
        Ord::Release => Ordering::Release,
        Ord::Opaque | Ord::Plain => Ordering::Relaxed,
    }
}

/// 对象字段的**声明类型描述符**（`I`/`J`/`D`/`F`/`Ljava/lang/String;` …）。
///
/// 为什么要它：本 VM 的槽是**带类型**的（`long` 存 `Slot::L`、`double` 存 `Slot::F`、
/// `float` 存 `Slot::Fl`）。`Unsafe.putDouble(o, off, x)` 只拿到宽度，光凭宽度把它存成
/// `Slot::L`，紧接着的 `getfield` 读 `Slot::F` 就会拿到 `2.25 → 4.6e18` 这种垃圾。
/// 所以按 `(类, 槽下标)` 反查一次声明类型 —— 顺序与 `load.rs::build_metas` 完全一致
/// （继承链从根往本类、每个类按声明顺序、跳过静态字段）；查不到就走老路。
fn field_type(vm: &Vm, class: &str, index: usize) -> Option<String> {
    let mut n = 0usize;
    for c in vm.store.chain(class).iter().rev() {
        let cf = vm.store.class_file(c)?;
        for f in &cf.fields {
            if f.access_flags & 0x0008 != 0 {
                continue; // 静态字段不在对象里
            }
            if n == index {
                return cf.utf8(f.descriptor_index).map(|b| String::from_utf8_lossy(b).into_owned());
            }
            n += 1;
        }
    }
    None
}

/// 字段描述符的字节宽度（判「这次访问宽度与字段宽度是否一致」）
fn desc_width(d: &str) -> usize {
    match d {
        "Z" | "B" => 1,
        "C" | "S" => 2,
        "I" | "F" => 4,
        _ => 8,
    }
}

/// 读 `width` 字节：(null, addr) → 裸内存；(o, offset) → 堆（对象字段按声明类型取值）
fn get_mem(vm: &Vm, obj: i32, off: i64, width: usize) -> u64 {
    if obj == 0 {
        return arena::read(off as u64, width);
    }
    if let (RefKind::Obj(cls), true) = (vm.ref_kind(obj), off >= 0) {
        if let Some(ft) = field_type(vm, &cls, off as usize) {
            if desc_width(&ft) == width {
                let s = super::unsafe_slot(vm, obj, off).unwrap_or(Slot::I(0));
                return match ft.as_str() {
                    "D" => s.as_f64().to_bits(),
                    "F" => s.as_f32().to_bits() as u64,
                    _ => s.as_i64() as u64,
                };
            }
        }
    }
    // `read_mem` 收的是**原始偏移**（它自己再按元素宽度除一次，别在这除）
    super::read_mem(vm, obj, off.max(0) as usize, width)
}

fn put_mem(vm: &mut Vm, obj: i32, off: i64, width: usize, v: u64) {
    if obj == 0 {
        arena::write(off as u64, width, v);
        return;
    }
    if let (RefKind::Obj(cls), true) = (vm.ref_kind(obj), off >= 0) {
        if let Some(ft) = field_type(vm, &cls, off as usize) {
            if desc_width(&ft) == width {
                let cell = match ft.as_str() {
                    "D" => Slot::F(f64::from_bits(v)),
                    "F" => Slot::Fl(f32::from_bits(v as u32)),
                    "J" => Slot::L(v as i64),
                    "Z" => Slot::I(i32::from(v != 0)),
                    _ => Slot::I(v as u32 as i32),
                };
                super::unsafe_put_impl(vm, obj, off, cell);
                return;
            }
        }
    }
    super::write_mem(vm, obj, off.max(0) as usize, width, v)
}

/// 读：`Acquire`/`Volatile` 在读**之后**下栅栏（后面的读写不得提前）
fn load(vm: &Vm, f: &Frame, w: usize, o: Ord) -> u64 {
    let v = get_mem(vm, f.arg_i(0), f.arg_l(1), w);
    if o != Ord::Plain {
        fence(ordering(o));
    }
    v
}

/// 写：`Release`/`Volatile` 在写**之前**下栅栏（前面的读写必须已落地）
fn store(vm: &mut Vm, f: &Frame, w: usize, v: u64, o: Ord) {
    if o != Ord::Plain {
        fence(ordering(o));
    }
    put_mem(vm, f.arg_i(0), f.arg_l(1), w, v);
}

// ---- 取值（返回值类型决定符号扩展）；落值（把 Java 值打成原始位）----

fn s_bool(v: u64) -> Slot {
    Slot::I(i32::from(v as u8 != 0))
}
fn s_byte(v: u64) -> Slot {
    Slot::I(v as u8 as i8 as i32)
}
fn s_char(v: u64) -> Slot {
    Slot::I(v as u16 as i32)
}
fn s_short(v: u64) -> Slot {
    Slot::I(v as u16 as i16 as i32)
}
fn s_int(v: u64) -> Slot {
    Slot::I(v as u32 as i32)
}
fn s_long(v: u64) -> Slot {
    Slot::L(v as i64)
}
fn s_float(v: u64) -> Slot {
    Slot::Fl(f32::from_bits(v as u32))
}
fn s_double(v: u64) -> Slot {
    Slot::F(f64::from_bits(v))
}
fn s_ref(v: u64) -> Slot {
    Slot::I(v as u32 as i32)
}

fn w_bool(s: Slot) -> u64 {
    u64::from(s.as_i32() != 0)
}
fn w_byte(s: Slot) -> u64 {
    s.as_i32() as u8 as u64
}
fn w_char(s: Slot) -> u64 {
    s.as_i32() as u16 as u64
}
fn w_short(s: Slot) -> u64 {
    s.as_i32() as u16 as u64
}
fn w_int(s: Slot) -> u64 {
    s.as_i32() as u32 as u64
}
fn w_long(s: Slot) -> u64 {
    s.as_i64() as u64
}
fn w_float(s: Slot) -> u64 {
    s.as_f32().to_bits() as u64
}
fn w_double(s: Slot) -> u64 {
    s.as_f64().to_bits()
}
fn w_ref(s: Slot) -> u64 {
    s.as_i32() as u32 as u64
}

// 每种宽度 × 每种内存序一个**两行**的 native。用宏是为了让「宽度、内存序、
// 符号扩展」三件事写在同一行里 —— 手抄 40 个必然抄错一个，而描述符差一个字符
// 就匹配不上（注册那半边仍然逐个手写，因为那才是验收要核对的东西）。
macro_rules! accessors {
    ($($get:ident, $put:ident, $w:expr, $gord:ident, $pord:ident, $s:ident, $wrap:ident;)*) => {
        $(
            fn $get(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
                Ok($s(load(vm, f, $w, Ord::$gord)))
            }
            fn $put(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
                store(vm, f, $w, ($wrap)(f.arg(2)), Ord::$pord);
                super::void()
            }
        )*
    };
}

accessors! {
    // ---- 普通 ----
    u_get_boolean, u_put_boolean, 1, Plain, Plain, s_bool, w_bool;
    u_get_byte, u_put_byte, 1, Plain, Plain, s_byte, w_byte;
    u_get_char, u_put_char, 2, Plain, Plain, s_char, w_char;
    u_get_short, u_put_short, 2, Plain, Plain, s_short, w_short;
    u_get_int, u_put_int, 4, Plain, Plain, s_int, w_int;
    u_get_long, u_put_long, 8, Plain, Plain, s_long, w_long;
    u_get_float, u_put_float, 4, Plain, Plain, s_float, w_float;
    u_get_double, u_put_double, 8, Plain, Plain, s_double, w_double;
    u_get_ref, u_put_ref, 8, Plain, Plain, s_ref, w_ref;
    // ---- volatile（SeqCst）----
    u_get_boolean_v, u_put_boolean_v, 1, Volatile, Volatile, s_bool, w_bool;
    u_get_byte_v, u_put_byte_v, 1, Volatile, Volatile, s_byte, w_byte;
    u_get_char_v, u_put_char_v, 2, Volatile, Volatile, s_char, w_char;
    u_get_short_v, u_put_short_v, 2, Volatile, Volatile, s_short, w_short;
    u_get_int_v, u_put_int_v, 4, Volatile, Volatile, s_int, w_int;
    u_get_long_v, u_put_long_v, 8, Volatile, Volatile, s_long, w_long;
    u_get_float_v, u_put_float_v, 4, Volatile, Volatile, s_float, w_float;
    u_get_double_v, u_put_double_v, 8, Volatile, Volatile, s_double, w_double;
    u_get_ref_v, u_put_ref_v, 8, Volatile, Volatile, s_ref, w_ref;
    // ---- opaque（Relaxed）/ acquire / release：JDK 25 的 Unsafe 不再声明这几个，
    //      但老版本与 `VarHandle` 语义要用，留着按名字注册（多注册不会误伤）----
    u_get_int_o, u_put_int_o, 4, Opaque, Opaque, s_int, w_int;
    u_get_ref_o, u_put_ref_o, 8, Opaque, Opaque, s_ref, w_ref;
    u_get_int_a, u_put_int_r, 4, Acquire, Release, s_int, w_int;
    u_get_ref_a, u_put_ref_r, 8, Acquire, Release, s_ref, w_ref;
}

// ---- CAS / 原子算术 ----

/// `compareAndSet{Int,Long,Reference}`：比较 + 写，返回是否交换成功
fn u_cas_int(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, off) = (f.arg_i(0), f.arg_l(1));
    let (exp, new) = (f.arg_i(2) as u32 as u64, f.arg_i(3) as u32 as u64);
    let cur = get_mem(vm, obj, off, 4);
    if cur == exp {
        put_mem(vm, obj, off, 4, new);
        fence(Ordering::SeqCst);
        return Ok(Slot::I(1));
    }
    Ok(Slot::I(0))
}

fn u_cas_long(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, off) = (f.arg_i(0), f.arg_l(1));
    let (exp, new) = (f.arg_l(2) as u64, f.arg_l(3) as u64);
    let cur = get_mem(vm, obj, off, 8);
    if cur == exp {
        put_mem(vm, obj, off, 8, new);
        fence(Ordering::SeqCst);
        return Ok(Slot::I(1));
    }
    Ok(Slot::I(0))
}

fn u_cas_ref(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, off) = (f.arg_i(0), f.arg_l(1));
    let (exp, new) = (f.arg_i(2) as u32 as u64, f.arg_i(3) as u32 as u64);
    // 句柄是 32 位：get_mem 读 8 字节会符号扩展，比较前必须裁到 32 位
    let cur = get_mem(vm, obj, off, 8) as u32 as u64;
    if cur == exp {
        put_mem(vm, obj, off, 8, new);
        fence(Ordering::SeqCst);
        return Ok(Slot::I(1));
    }
    Ok(Slot::I(0))
}

/// `compareAndExchange{Int,Long,Reference}`：返回**交换前**的值
fn u_cax_int(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, off) = (f.arg_i(0), f.arg_l(1));
    let (exp, new) = (f.arg_i(2) as u32 as u64, f.arg_i(3) as u32 as u64);
    let cur = get_mem(vm, obj, off, 4);
    if cur == exp {
        put_mem(vm, obj, off, 4, new);
    }
    fence(Ordering::SeqCst);
    Ok(s_int(cur))
}

fn u_cax_long(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, off) = (f.arg_i(0), f.arg_l(1));
    let (exp, new) = (f.arg_l(2) as u64, f.arg_l(3) as u64);
    let cur = get_mem(vm, obj, off, 8);
    if cur == exp {
        put_mem(vm, obj, off, 8, new);
    }
    fence(Ordering::SeqCst);
    Ok(s_long(cur))
}

fn u_cax_ref(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, off) = (f.arg_i(0), f.arg_l(1));
    let (exp, new) = (f.arg_i(2) as u32 as u64, f.arg_i(3) as u32 as u64);
    let cur = get_mem(vm, obj, off, 8) as u32 as u64;
    if cur == exp {
        put_mem(vm, obj, off, 8, new);
    }
    fence(Ordering::SeqCst);
    Ok(s_ref(cur))
}

/// `getAndAdd{Int,Long}`：返回**加之前**的值
fn u_get_add_int(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, off) = (f.arg_i(0), f.arg_l(1));
    let cur = get_mem(vm, obj, off, 4);
    let next = (cur as u32).wrapping_add(f.arg_i(2) as u32);
    put_mem(vm, obj, off, 4, next as u64);
    fence(Ordering::SeqCst);
    Ok(s_int(cur))
}

fn u_get_add_long(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, off) = (f.arg_i(0), f.arg_l(1));
    let cur = get_mem(vm, obj, off, 8);
    let next = cur.wrapping_add(f.arg_l(2) as u64);
    put_mem(vm, obj, off, 8, next);
    fence(Ordering::SeqCst);
    Ok(s_long(cur))
}

/// `getAndSet{Int,Long,Reference}`：返回**替换前**的值
fn u_get_set_int(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, off) = (f.arg_i(0), f.arg_l(1));
    let cur = get_mem(vm, obj, off, 4);
    put_mem(vm, obj, off, 4, f.arg_i(2) as u32 as u64);
    fence(Ordering::SeqCst);
    Ok(s_int(cur))
}

fn u_get_set_long(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, off) = (f.arg_i(0), f.arg_l(1));
    let cur = get_mem(vm, obj, off, 8);
    put_mem(vm, obj, off, 8, f.arg_l(2) as u64);
    fence(Ordering::SeqCst);
    Ok(s_long(cur))
}

fn u_get_set_ref(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, off) = (f.arg_i(0), f.arg_l(1));
    let cur = get_mem(vm, obj, off, 8);
    put_mem(vm, obj, off, 8, f.arg_i(2) as u32 as u64);
    fence(Ordering::SeqCst);
    Ok(s_ref(cur))
}

// ---- 类 / 对象 ----

/// `Unsafe.registerNatives()`：顺带补上 `UnsafeConstants` 的那几个**构建期生成**的常量。
///
/// 为什么必须在这里做：`Unsafe.<clinit>` 的第一句是 `runtimeSetup()` → `registerNatives()`，
/// 之后才用 `UnsafeConstants.ADDRESS_SIZE0` 算出 `ADDRESS_SIZE`、`ARRAY_*_BASE_OFFSET` 等
/// 一整套静态量。而 `UnsafeConstants.<clinit>` 在**没打补丁**的镜像里全写 0
/// （那五个常量都是 JDK 构建时填进去的，`jimage` 里那份是占位的 0）。
///
/// `ADDRESS_SIZE = 0` 的后果很具体：`Unsafe.alignToHeapWordSize` 变成
/// `(bytes - 1) & ~(0 - 1) = 0`，于是 `allocateMemory` 直接返回 0 —— 一句 native 都还没走到。
/// `pageSize() = 0` 同理。
///
/// 做法：把正确值直接写进静态字段表，并把 `UnsafeConstants` 标记为**已初始化** ——
/// 它那个只会写 0 的 `<clinit>` 就不用跑了（它没有别的副作用，只有一个私有构造器）。
fn u_register_natives(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    const UC: &str = "jdk/internal/misc/UnsafeConstants";
    // 口径 = HotSpot x86-64：8 字节地址、4K 页、小端、允许非对齐访问、64 字节缓存行
    for (name, v) in [
        ("ADDRESS_SIZE0", 8),
        ("PAGE_SIZE", 4096),
        ("BIG_ENDIAN", 0),
        ("UNALIGNED_ACCESS", 1),
        ("DATA_CACHE_LINE_FLUSH_SIZE", 64),
    ] {
        super::lk(&vm.sh.statics).insert((UC.to_string(), name.to_string()), Slot::I(v));
    }
    super::lk(&vm.sh.initialized).insert(UC.to_string());
    super::void()
}

fn u_should_be_initialized(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let cls = vm.class_obj_name(f.arg_i(0)).unwrap_or_default();
    // 原始类型 / 数组没有 `<clinit>`：`Class.ensureInitialized` 对它们直接返回
    let needs = cls.len() > 1
        && !cls.starts_with('[')
        && !super::lk(&vm.sh.initialized).contains(&cls);
    Ok(Slot::I(if needs { 1 } else { 0 }))
}

fn u_define_class(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Err(vm.java_exception(
        "java/lang/UnsupportedOperationException",
        "Unsafe.defineClass 在本 VM 不支持（没有类加载器，运行时不能定义新类）",
    ))
}

/// `getUncompressedObject(long)`：压缩 oop 的还原。本 VM 的句柄不是压缩指针 → null。
fn u_uncompressed(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::I(0))
}

/// 写回屏障（持久内存 / `VarHandle.storeLoadFence` 的底座）：本 VM 没有需要写回的缓存
fn u_noop(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    super::void()
}

/// `getLoadAverage0(double[] loadavg, int nelems)`：本 VM 不提供系统负载
/// （HotSpot 在 Windows 上也返回 -1，调用方据此抛 `UnsupportedOperationException`）
fn u_load_average(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::I(-1))
}

// ---- 裸内存 ----

fn u_alloc_memory(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::L(arena::alloc(f.arg_l(0) as u64) as i64))
}

fn u_realloc_memory(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::L(arena::realloc(f.arg_l(0) as u64, f.arg_l(1) as u64) as i64))
}

fn u_free_memory(_: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    arena::free(f.arg_l(0) as u64);
    super::void()
}

/// `setMemory0(Object o, long offset, long bytes, byte value)`
fn u_set_memory(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (obj, off, len, v) = (f.arg_i(0), f.arg_l(1), f.arg_l(2), f.arg_i(3) as u8);
    for k in 0..len.max(0) as usize {
        put_mem(vm, obj, off + k as i64, 1, v as u64);
    }
    super::void()
}

/// `copyMemory0(Object src, long srcOff, Object dst, long dstOff, long bytes)`
///
/// 先全部读出来再写 —— 源与目标重叠时「边读边写」会把没读到的部分提前盖掉。
fn u_copy_memory(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (so, soff, dobj, doff, len) = (f.arg_i(0), f.arg_l(1), f.arg_i(2), f.arg_l(3), f.arg_l(4));
    let bytes: Vec<u64> = (0..len.max(0) as usize).map(|k| get_mem(vm, so, soff + k as i64, 1)).collect();
    for (k, b) in bytes.into_iter().enumerate() {
        put_mem(vm, dobj, doff + k as i64, 1, b);
    }
    super::void()
}

/// `copySwapMemory0(src, srcOff, dst, dstOff, bytes, elemSize)`：逐元素字节反转
/// （`ByteBuffer` / `StringUTF16` 的大小端互换）
fn u_copy_swap_memory(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (so, soff, dobj, doff, len, esz) =
        (f.arg_i(0), f.arg_l(1), f.arg_i(2), f.arg_l(3), f.arg_l(4), f.arg_l(5));
    let n = len.max(0) as usize;
    let w = esz.max(0) as usize;
    let bytes: Vec<u64> = (0..n).map(|k| get_mem(vm, so, soff + k as i64, 1)).collect();
    for k in 0..n {
        // 元素内第 (k % w) 个字节，来自元素末尾往前数
        let src = (k / w) * w + (w - 1 - k % w);
        let v = bytes.get(src).copied().unwrap_or(0);
        put_mem(vm, dobj, doff + k as i64, 1, v);
    }
    super::void()
}

fn unsafe_reg(r: &mut NativeRegistry) {
    let u = "jdk/internal/misc/Unsafe";
    // `Unsafe.<clinit>` 第一句就是 `runtimeSetup()` → `registerNatives()`，
    // 所以这里是**唯一**能赶在 `ADDRESS_SIZE` 之类的静态量算出来之前动手的钩子（见下面 `u_register_natives`）
    r.add(u, "registerNatives", "()V", u_register_natives);

    // ---- 字段 / 数组布局 ----
    // 我们的堆没有裸地址，「偏移」= **对象里的槽下标** / 数组里的**字节偏移**
    r.add(u, "objectFieldOffset0", "(Ljava/lang/reflect/Field;)J", super::unsafe_field_offset);
    r.add(
        u,
        "objectFieldOffset1",
        "(Ljava/lang/Class;Ljava/lang/String;)J",
        super::unsafe_field_offset_by_name,
    );
    // 静态字段存在 `sh.statics` 里（按 `(类,名)` 索引），没有「基址 + 偏移」这种布局
    r.add(u, "staticFieldOffset0", "(Ljava/lang/reflect/Field;)J", |_, _| Ok(Slot::L(0)));
    r.add(u, "staticFieldBase0", "(Ljava/lang/reflect/Field;)Ljava/lang/Object;", |_, _| Ok(Slot::I(0)));
    r.add(u, "shouldBeInitialized0", "(Ljava/lang/Class;)Z", u_should_be_initialized);
    r.add(u, "ensureClassInitialized0", "(Ljava/lang/Class;)V", super::unsafe_ensure_init);
    r.add(u, "arrayBaseOffset0", "(Ljava/lang/Class;)I", |_, _| Ok(Slot::I(0)));
    r.add(u, "arrayIndexScale0", "(Ljava/lang/Class;)I", super::unsafe_array_scale);

    // ---- 读（宽度决定打包几个字节：`[B` 的机器字比较靠它）----
    r.add(u, "getBoolean", "(Ljava/lang/Object;J)Z", u_get_boolean);
    r.add(u, "getByte", "(Ljava/lang/Object;J)B", u_get_byte);
    r.add(u, "getChar", "(Ljava/lang/Object;J)C", u_get_char);
    r.add(u, "getShort", "(Ljava/lang/Object;J)S", u_get_short);
    r.add(u, "getInt", "(Ljava/lang/Object;J)I", u_get_int);
    r.add(u, "getLong", "(Ljava/lang/Object;J)J", u_get_long);
    r.add(u, "getFloat", "(Ljava/lang/Object;J)F", u_get_float);
    r.add(u, "getDouble", "(Ljava/lang/Object;J)D", u_get_double);
    r.add(u, "getReference", "(Ljava/lang/Object;J)Ljava/lang/Object;", u_get_ref);
    // ---- 写 ----
    r.add(u, "putBoolean", "(Ljava/lang/Object;JZ)V", u_put_boolean);
    r.add(u, "putByte", "(Ljava/lang/Object;JB)V", u_put_byte);
    r.add(u, "putChar", "(Ljava/lang/Object;JC)V", u_put_char);
    r.add(u, "putShort", "(Ljava/lang/Object;JS)V", u_put_short);
    r.add(u, "putInt", "(Ljava/lang/Object;JI)V", u_put_int);
    r.add(u, "putLong", "(Ljava/lang/Object;JJ)V", u_put_long);
    r.add(u, "putFloat", "(Ljava/lang/Object;JF)V", u_put_float);
    r.add(u, "putDouble", "(Ljava/lang/Object;JD)V", u_put_double);
    r.add(u, "putReference", "(Ljava/lang/Object;JLjava/lang/Object;)V", u_put_ref);
    // ---- volatile：Read = SeqCst ----
    r.add(u, "getBooleanVolatile", "(Ljava/lang/Object;J)Z", u_get_boolean_v);
    r.add(u, "getByteVolatile", "(Ljava/lang/Object;J)B", u_get_byte_v);
    r.add(u, "getCharVolatile", "(Ljava/lang/Object;J)C", u_get_char_v);
    r.add(u, "getShortVolatile", "(Ljava/lang/Object;J)S", u_get_short_v);
    r.add(u, "getIntVolatile", "(Ljava/lang/Object;J)I", u_get_int_v);
    r.add(u, "getLongVolatile", "(Ljava/lang/Object;J)J", u_get_long_v);
    r.add(u, "getFloatVolatile", "(Ljava/lang/Object;J)F", u_get_float_v);
    r.add(u, "getDoubleVolatile", "(Ljava/lang/Object;J)D", u_get_double_v);
    r.add(u, "getReferenceVolatile", "(Ljava/lang/Object;J)Ljava/lang/Object;", u_get_ref_v);
    r.add(u, "putBooleanVolatile", "(Ljava/lang/Object;JZ)V", u_put_boolean_v);
    r.add(u, "putByteVolatile", "(Ljava/lang/Object;JB)V", u_put_byte_v);
    r.add(u, "putCharVolatile", "(Ljava/lang/Object;JC)V", u_put_char_v);
    r.add(u, "putShortVolatile", "(Ljava/lang/Object;JS)V", u_put_short_v);
    r.add(u, "putIntVolatile", "(Ljava/lang/Object;JI)V", u_put_int_v);
    r.add(u, "putLongVolatile", "(Ljava/lang/Object;JJ)V", u_put_long_v);
    r.add(u, "putFloatVolatile", "(Ljava/lang/Object;JF)V", u_put_float_v);
    r.add(u, "putDoubleVolatile", "(Ljava/lang/Object;JD)V", u_put_double_v);
    r.add(u, "putReferenceVolatile", "(Ljava/lang/Object;JLjava/lang/Object;)V", u_put_ref_v);
    // ---- opaque / acquire / release（老版本与语义对齐用；JDK 25 已不再声明成 native）----
    r.add(u, "getIntOpaque", "(Ljava/lang/Object;J)I", u_get_int_o);
    r.add(u, "putIntOpaque", "(Ljava/lang/Object;JI)V", u_put_int_o);
    r.add(u, "getReferenceOpaque", "(Ljava/lang/Object;J)Ljava/lang/Object;", u_get_ref_o);
    r.add(u, "putReferenceOpaque", "(Ljava/lang/Object;JLjava/lang/Object;)V", u_put_ref_o);
    r.add(u, "getIntAcquire", "(Ljava/lang/Object;J)I", u_get_int_a);
    r.add(u, "putIntRelease", "(Ljava/lang/Object;JI)V", u_put_int_r);
    r.add(u, "getReferenceAcquire", "(Ljava/lang/Object;J)Ljava/lang/Object;", u_get_ref_a);
    r.add(u, "putReferenceRelease", "(Ljava/lang/Object;JLjava/lang/Object;)V", u_put_ref_r);

    // ---- CAS / 原子算术 ----
    r.add(u, "compareAndSetInt", "(Ljava/lang/Object;JII)Z", u_cas_int);
    r.add(u, "compareAndSetLong", "(Ljava/lang/Object;JJJ)Z", u_cas_long);
    r.add(
        u,
        "compareAndSetReference",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
        u_cas_ref,
    );
    r.add(u, "compareAndExchangeInt", "(Ljava/lang/Object;JII)I", u_cax_int);
    r.add(u, "compareAndExchangeLong", "(Ljava/lang/Object;JJJ)J", u_cax_long);
    r.add(
        u,
        "compareAndExchangeReference",
        "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        u_cax_ref,
    );
    r.add(u, "getAndAddInt", "(Ljava/lang/Object;JI)I", u_get_add_int);
    r.add(u, "getAndAddLong", "(Ljava/lang/Object;JJ)J", u_get_add_long);
    r.add(u, "getAndSetInt", "(Ljava/lang/Object;JI)I", u_get_set_int);
    r.add(u, "getAndSetLong", "(Ljava/lang/Object;JJ)J", u_get_set_long);
    r.add(u, "getAndSetReference", "(Ljava/lang/Object;JLjava/lang/Object;)Ljava/lang/Object;", u_get_set_ref);

    // ---- 类 / 对象 ----
    r.add(u, "allocateInstance", "(Ljava/lang/Class;)Ljava/lang/Object;", super::unsafe_allocate);
    r.add(u, "throwException", "(Ljava/lang/Throwable;)V", super::unsafe_throw);
    r.add(
        u,
        "defineClass0",
        "(Ljava/lang/String;[BIILjava/lang/ClassLoader;Ljava/security/ProtectionDomain;)Ljava/lang/Class;",
        u_define_class,
    );
    r.add(u, "getUncompressedObject", "(J)Ljava/lang/Object;", u_uncompressed);
    r.add(u, "writeback0", "(J)V", u_noop);
    r.add(u, "writebackPreSync0", "()V", u_noop);
    r.add(u, "writebackPostSync0", "()V", u_noop);

    // ---- 内存块 ----
    r.add(u, "allocateMemory0", "(J)J", u_alloc_memory);
    r.add(u, "reallocateMemory0", "(JJ)J", u_realloc_memory);
    r.add(u, "freeMemory0", "(J)V", u_free_memory);
    r.add(u, "setMemory0", "(Ljava/lang/Object;JJB)V", u_set_memory);
    r.add(u, "copyMemory0", "(Ljava/lang/Object;JLjava/lang/Object;JJ)V", u_copy_memory);
    r.add(u, "copySwapMemory0", "(Ljava/lang/Object;JLjava/lang/Object;JJJ)V", u_copy_swap_memory);

    // ---- 线程停靠 / 栅栏 ----
    r.add(u, "park", "(ZJ)V", super::unsafe_park);
    r.add(u, "unpark", "(Ljava/lang/Object;)V", super::unsafe_unpark);
    r.add(u, "fullFence", "()V", super::fence_seqcst);
    r.add(u, "loadFence", "()V", super::fence_seqcst);
    r.add(u, "storeFence", "()V", super::fence_seqcst);
    r.add(u, "acquireFence", "()V", super::fence_seqcst);
    r.add(u, "releaseFence", "()V", super::fence_seqcst);
    r.add(u, "getLoadAverage0", "([DI)I", u_load_average);
}

// ================================================================ java.lang.reflect.Array

/// 元素类别。`Array.getX` 的合法性按它判（宽化规则见各 get/set）
#[derive(Clone, Copy, PartialEq)]
enum Elem {
    Bool,
    Byte,
    Char,
    Short,
    Int,
    Long,
    Float,
    Double,
    Ref,
}

fn elem_of(desc: &str) -> Option<Elem> {
    Some(match desc {
        "[Z" => Elem::Bool,
        "[B" => Elem::Byte,
        "[C" => Elem::Char,
        "[S" => Elem::Short,
        "[I" => Elem::Int,
        "[J" => Elem::Long,
        "[F" => Elem::Float,
        "[D" => Elem::Double,
        d if d.starts_with('[') => Elem::Ref,
        _ => return None,
    })
}

/// 参数必须是数组：null → NPE，非数组 → IllegalArgumentException
fn arr_desc(vm: &mut Vm, h: i32) -> Result<(String, usize), VmError> {
    match vm.ref_kind(h) {
        RefKind::Null => Err(vm.java_exception("java/lang/NullPointerException", "Array: 数组是 null")),
        RefKind::Arr(d) => {
            let n = vm.sh.heap.array_len(h).unwrap_or(0);
            Ok((d, n))
        }
        _ => Err(vm.java_exception("java/lang/IllegalArgumentException", "Array: 参数不是数组")),
    }
}

/// 下标检查 → 槽下标
fn arr_at(vm: &mut Vm, h: i32, i: i32) -> Result<(String, usize), VmError> {
    let (d, n) = arr_desc(vm, h)?;
    if i < 0 || i as usize >= n {
        return Err(vm.java_exception(
            "java/lang/ArrayIndexOutOfBoundsException",
            format!("Index {i} out of bounds for length {n}"),
        ));
    }
    Ok((d, i as usize))
}

/// 数组描述符 → 元素**类名**（`[Ljava/lang/String;` → `java/lang/String`、`[[I` → `[I`）。
/// 和 `class.rs::elem_class_name` 同一个口径 —— `type_assignable` 要的是内部类名，不是 `L…;`。
fn elem_class(desc: &str) -> &str {
    let d = desc.strip_prefix('[').unwrap_or(desc);
    if d.len() >= 2 && d.starts_with('L') && d.ends_with(';') {
        &d[1..d.len() - 1]
    } else {
        d
    }
}

/// 元素类别不符 → `IllegalArgumentException`（消息与 HotSpot 同形）
fn need(vm: &mut Vm, desc: &str, want: Elem) -> Result<Elem, VmError> {
    let got = elem_of(desc)
        .ok_or_else(|| vm.java_exception("java/lang/IllegalArgumentException", "Array: 不是数组"))?;
    if got == want {
        return Ok(got);
    }
    Err(vm.java_exception("java/lang/IllegalArgumentException", "Array: 数组元素类型不符"))
}

fn arr_get_length(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (_, n) = arr_desc(vm, f.arg_i(0))?;
    Ok(Slot::I(n as i32))
}

/// `Array.get(Object array, int index)Object` —— 基本类型数组返回**装箱**值
fn arr_get_object(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (desc, i) = arr_at(vm, f.arg_i(0), f.arg_i(1))?;
    let v = vm.sh.heap.get(f.arg_i(0), i).unwrap_or(Slot::I(0));
    match elem_of(&desc) {
        Some(Elem::Bool) => Ok(Slot::I(box_val(vm, "java/lang/Boolean", Slot::I(v.as_i32())))),
        Some(Elem::Byte) => Ok(Slot::I(box_val(vm, "java/lang/Byte", Slot::I(v.as_i32())))),
        Some(Elem::Char) => Ok(Slot::I(box_val(vm, "java/lang/Character", Slot::I(v.as_i32())))),
        Some(Elem::Short) => Ok(Slot::I(box_val(vm, "java/lang/Short", Slot::I(v.as_i32())))),
        Some(Elem::Int) => Ok(Slot::I(box_val(vm, "java/lang/Integer", Slot::I(v.as_i32())))),
        Some(Elem::Long) => Ok(Slot::I(box_val(vm, "java/lang/Long", Slot::L(v.as_i64())))),
        Some(Elem::Float) => Ok(Slot::I(box_val(vm, "java/lang/Float", Slot::Fl(v.as_f32())))),
        Some(Elem::Double) => Ok(Slot::I(box_val(vm, "java/lang/Double", Slot::F(v.as_f64())))),
        Some(Elem::Ref) => Ok(v),
        None => Err(vm.java_exception("java/lang/IllegalArgumentException", "Array: 不是数组")),
    }
}

/// 装箱一个基本类型值（`Integer.valueOf` 的语义；本 VM 的包装对象就是一个槽）
fn box_val(vm: &mut Vm, class: &str, v: Slot) -> i32 {
    vm.sh.heap.new_object(vec![v], class).unwrap_or(0)
}

/// `Array.getInt` 一族：**宽化**允许（byte/char/short → int），其余报 IAE
///
/// 只用于 `int` 及更窄的类型 —— `long`/`float`/`double` 的取值路径在下面单独写
/// （它们的值不能挤进 `as_i32`，那会把 2.5 读成 2.0）。
fn typed_get(vm: &mut Vm, f: &mut Frame, want: Elem) -> Result<Slot, VmError> {
    let (desc, i) = arr_at(vm, f.arg_i(0), f.arg_i(1))?;
    let v = vm.sh.heap.get(f.arg_i(0), i).unwrap_or(Slot::I(0)).as_i32();
    let got = elem_of(&desc)
        .ok_or_else(|| vm.java_exception("java/lang/IllegalArgumentException", "Array: 不是数组"))?;
    let ok = got == want
        || (want == Elem::Int && matches!(got, Elem::Byte | Elem::Char | Elem::Short));
    if !ok {
        return Err(vm.java_exception("java/lang/IllegalArgumentException", "Array: 数组元素类型不符"));
    }
    Ok(Slot::I(match got {
        Elem::Bool => i32::from(v != 0),
        Elem::Byte => v as i8 as i32,
        Elem::Char => v as u16 as i32,
        Elem::Short => v as i16 as i32,
        _ => v,
    }))
}

/// `Array.setInt` 一族：只允许**同一类型**（窄化会丢值 → IAE）
fn typed_set(vm: &mut Vm, f: &mut Frame, want: Elem) -> Result<Slot, VmError> {
    let (desc, i) = arr_at(vm, f.arg_i(0), f.arg_i(1))?;
    need(vm, &desc, want)?;
    let raw = f.arg_i(2);
    let v = match want {
        Elem::Bool => Slot::I(i32::from(raw != 0)),
        Elem::Byte | Elem::Char | Elem::Short | Elem::Int => Slot::I(raw),
        Elem::Long => Slot::L(f.arg_l(2)),
        Elem::Float => Slot::Fl(f.arg_f(2)),
        Elem::Double => Slot::F(f.arg_d(2)),
        Elem::Ref => Slot::I(raw),
    };
    vm.sh.heap.set(f.arg_i(0), i, v);
    super::void()
}

/// `Array.set(Object array, int index, Object value)`：
/// 引用数组按 `IllegalArgumentException` 判（**不是** `ArrayStoreException`：`aastore` 才报 ASE，
/// `Array.set` 的 javadoc 明确写 IAE，HotSpot 实测也是 IAE）；基本类型数组按**包装类型**判
fn arr_set_object(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (desc, i) = arr_at(vm, f.arg_i(0), f.arg_i(1))?;
    let val = f.arg_i(2);
    let kind = elem_of(&desc)
        .ok_or_else(|| vm.java_exception("java/lang/IllegalArgumentException", "Array: 不是数组"))?;
    if kind == Elem::Ref {
        if val != 0 {
            let elem = elem_class(&desc);
            let from = vm.class_of(val).unwrap_or_default();
            if !vm.type_assignable(&from, elem) {
                return Err(vm.java_exception(
                    "java/lang/IllegalArgumentException",
                    format!("{from} 不能存进 {desc}"),
                ));
            }
        }
        vm.sh.heap.set_ref(f.arg_i(0), i, Slot::I(val));
        return super::void();
    }
    // 基本类型数组：value 必须是那个类型的包装（null → IllegalArgumentException）
    if val == 0 {
        return Err(vm.java_exception(
            "java/lang/IllegalArgumentException",
            "Array.set: 基本类型数组不接受 null",
        ));
    }
    let want_class = match kind {
        Elem::Bool => "java/lang/Boolean",
        Elem::Byte => "java/lang/Byte",
        Elem::Char => "java/lang/Character",
        Elem::Short => "java/lang/Short",
        Elem::Int => "java/lang/Integer",
        Elem::Long => "java/lang/Long",
        Elem::Float => "java/lang/Float",
        _ => "java/lang/Double",
    };
    let got = vm.class_of(val).unwrap_or_default();
    if got != want_class {
        return Err(vm.java_exception(
            "java/lang/IllegalArgumentException",
            format!("{got} 不能存进 {}", desc),
        ));
    }
    let raw = vm.sh.heap.get(val, 0).unwrap_or(Slot::I(0));
    let cell = match kind {
        Elem::Bool => Slot::I(i32::from(raw.as_i32() != 0)),
        Elem::Long => Slot::L(raw.as_i64()),
        Elem::Float => Slot::Fl(raw.as_f32()),
        Elem::Double => Slot::F(raw.as_f64()),
        _ => Slot::I(raw.as_i32()),
    };
    vm.sh.heap.set(f.arg_i(0), i, cell);
    super::void()
}

/// 组件类对象 → 数组描述符（`int` → `[I`、`java/lang/String` → `[Ljava/lang/String;`、`[I` → `[[I`）
fn array_desc_of_component(vm: &mut Vm, comp: i32) -> Result<String, VmError> {
    let c = match vm.class_obj_name(comp) {
        Some(c) if !c.is_empty() => c,
        _ if comp == 0 => {
            return Err(vm.java_exception("java/lang/NullPointerException", "Array: 组件类型是 null"))
        }
        _ => return Err(vm.java_exception("java/lang/IllegalArgumentException", "Array: 组件类型不是类对象")),
    };
    // `void` 不是合法的组件类型（`Array.newInstance(void.class, 1)` 在 HotSpot 里报 IAE）
    if c == "V" {
        return Err(vm.java_exception("java/lang/IllegalArgumentException", "Array: void 不是合法的组件类型"));
    }
    Ok(if c.len() == 1 || c.starts_with('[') { format!("[{c}") } else { format!("[L{c};") })
}

fn arr_new_array(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let desc = array_desc_of_component(vm, f.arg_i(0))?;
    let n = f.arg_i(1);
    if n < 0 {
        return Err(vm.java_exception(
            "java/lang/NegativeArraySizeException",
            format!("{n}"),
        ));
    }
    let zero = match desc.as_str() {
        "[J" => Slot::L(0),
        "[D" => Slot::F(0.0),
        "[F" => Slot::Fl(0.0),
        _ => Slot::I(0),
    };
    match vm.sh.heap.alloc_array(n as usize, &desc, zero) {
        Some(h) => Ok(Slot::I(h)),
        None => Err(vm.java_exception("java/lang/OutOfMemoryError", "数组堆用尽")),
    }
}

/// `Array.multiNewArray(Class componentType, int[] dimensions)`：
/// `componentType` 是**最外层数组的元素类型**（即 `new int[2][3]` 传 `int.class` 与 `{2,3}`）
fn arr_multi_new(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let comp = f.arg_i(0);
    let dims_h = f.arg_i(1);
    let dims: Vec<i32> = match vm.sh.heap.snapshot(dims_h) {
        Some(v) => v.iter().map(|s| s.as_i32()).collect(),
        None => return Err(vm.java_exception("java/lang/NullPointerException", "Array: 维度数组是 null")),
    };
    for d in &dims {
        if *d < 0 {
            return Err(vm.java_exception("java/lang/NegativeArraySizeException", format!("{d}")));
        }
    }
    if dims.is_empty() {
        return Err(vm.java_exception("java/lang/IllegalArgumentException", "Array: 维度为空"));
    }
    let h = build_multi(vm, comp, &dims)?;
    Ok(Slot::I(h))
}

fn build_multi(vm: &mut Vm, comp: i32, dims: &[i32]) -> Result<i32, VmError> {
    let one = array_desc_of_component(vm, comp)?;
    // 从最内层往外建：先建 dims[n-1] 长度的那一层，再逐个填进上一层
    let mut desc = one;
    for _ in dims.iter().skip(1) {
        desc = format!("[{desc}");
    }
    // 逐层递归构造（层数一般 ≤ 3，递归开销可忽略）
    fn build(vm: &mut Vm, desc: &str, dims: &[i32]) -> Result<i32, VmError> {
        let head = *dims.first().unwrap_or(&0);
        let rest = &dims[1..];
        let zero = match desc {
            "[J" => Slot::L(0),
            "[D" => Slot::F(0.0),
            "[F" => Slot::Fl(0.0),
            _ => Slot::I(0),
        };
        let Some(h) = vm.sh.heap.alloc_array(head as usize, desc, zero) else {
            return Err(vm.java_exception("java/lang/OutOfMemoryError", "数组堆用尽"));
        };
        if !rest.is_empty() {
            // `desc` 当前层是 `[<inner>`，内层描述符就是去掉一个 `[`
            let inner = &desc[1..];
            for k in 0..head as usize {
                let child = build(vm, inner, rest)?;
                vm.sh.heap.set_ref(h, k, Slot::I(child));
            }
        }
        Ok(h)
    }
    build(vm, &desc, dims)
}

fn array_reg(r: &mut NativeRegistry) {
    let a = "java/lang/reflect/Array";
    r.add(a, "getLength", "(Ljava/lang/Object;)I", arr_get_length);
    r.add(a, "newArray", "(Ljava/lang/Class;I)Ljava/lang/Object;", arr_new_array);
    r.add(a, "multiNewArray", "(Ljava/lang/Class;[I)Ljava/lang/Object;", arr_multi_new);
    r.add(a, "get", "(Ljava/lang/Object;I)Ljava/lang/Object;", arr_get_object);
    r.add(a, "set", "(Ljava/lang/Object;ILjava/lang/Object;)V", arr_set_object);

    // 基本类型 getter（宽化允许）
    r.add(a, "getBoolean", "(Ljava/lang/Object;I)Z", |vm, f| typed_get(vm, f, Elem::Bool));
    r.add(a, "getByte", "(Ljava/lang/Object;I)B", |vm, f| typed_get(vm, f, Elem::Byte));
    r.add(a, "getChar", "(Ljava/lang/Object;I)C", |vm, f| typed_get(vm, f, Elem::Char));
    r.add(a, "getShort", "(Ljava/lang/Object;I)S", |vm, f| typed_get(vm, f, Elem::Short));
    r.add(a, "getInt", "(Ljava/lang/Object;I)I", |vm, f| typed_get(vm, f, Elem::Int));
    r.add(a, "getFloat", "(Ljava/lang/Object;I)F", arr_get_float);
    r.add(a, "getDouble", "(Ljava/lang/Object;I)D", arr_get_double);
    // `getLong` 自己一份：`long` 的取值要走 Slot::L，不能挤在 i32 里
    r.add(a, "getLong", "(Ljava/lang/Object;I)J", arr_get_long);
    // 基本类型 setter（只允许同一类型）
    r.add(a, "setBoolean", "(Ljava/lang/Object;IZ)V", |vm, f| typed_set(vm, f, Elem::Bool));
    r.add(a, "setByte", "(Ljava/lang/Object;IB)V", |vm, f| typed_set(vm, f, Elem::Byte));
    r.add(a, "setChar", "(Ljava/lang/Object;IC)V", |vm, f| typed_set(vm, f, Elem::Char));
    r.add(a, "setShort", "(Ljava/lang/Object;IS)V", |vm, f| typed_set(vm, f, Elem::Short));
    r.add(a, "setInt", "(Ljava/lang/Object;II)V", |vm, f| typed_set(vm, f, Elem::Int));
    r.add(a, "setLong", "(Ljava/lang/Object;IJ)V", |vm, f| typed_set(vm, f, Elem::Long));
    r.add(a, "setFloat", "(Ljava/lang/Object;IF)V", |vm, f| typed_set(vm, f, Elem::Float));
    r.add(a, "setDouble", "(Ljava/lang/Object;ID)V", |vm, f| typed_set(vm, f, Elem::Double));
}

/// `Array.getLong`：`long[]` 的槽是 `Slot::L`，与 `typed_get` 的 i32 路径不同
fn arr_get_long(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (desc, i) = arr_at(vm, f.arg_i(0), f.arg_i(1))?;
    need(vm, &desc, Elem::Long)?;
    Ok(Slot::L(vm.sh.heap.get(f.arg_i(0), i).unwrap_or(Slot::L(0)).as_i64()))
}

/// `Array.getFloat` / `Array.getDouble`：值必须按 f32/f64 取（挤进 i32 会丢小数）
fn arr_get_float(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (desc, i) = arr_at(vm, f.arg_i(0), f.arg_i(1))?;
    need(vm, &desc, Elem::Float)?;
    Ok(Slot::Fl(vm.sh.heap.get(f.arg_i(0), i).unwrap_or(Slot::Fl(0.0)).as_f32()))
}

fn arr_get_double(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let (desc, i) = arr_at(vm, f.arg_i(0), f.arg_i(1))?;
    need(vm, &desc, Elem::Double)?;
    Ok(Slot::F(vm.sh.heap.get(f.arg_i(0), i).unwrap_or(Slot::F(0.0)).as_f64()))
}

// ================================================================ java.lang.Class
//
// `reflect.rs` 已经接管了 forName0 / getDeclaredXxx0 / getInterfaces0 /
// getDeclaringClass0 / getSimpleBinaryName0 / getGenericSignature0；
// 这里补它没管的那些。判据仍是「OpenJDK 里带 native 关键字」。

/// 成员类 / 局部类 / 匿名类的外层方法：本 VM 不解析 `EnclosingMethod` → null（= 不在方法里）
fn cls_enclosing_method(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::I(0))
}

/// 原始注解字节：本 VM 不解析注解属性 → null（`getAnnotations()` 因此返回空数组）
fn cls_raw_annos(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::I(0))
}

/// `getConstantPool()`：给一个**非 null** 的空 ConstantPool —— 调用方拿到 null 会 NPE，
/// 而我们的常量池读取一律返回默认值（见 `constant_pool_reg`）
fn cls_constant_pool(vm: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::I(vm.alloc_vmobj("jdk/internal/reflect/ConstantPool")))
}

/// class 文件里某个属性（按名字取原始字节）。`generic_signature` 之类都由 `reflect.rs` 另管，
/// 这里只用它读 `NestHost` / `NestMembers` / `InnerClasses` / `PermittedSubclasses` / `Record`。
fn cf_attr<'a>(cf: &'a crate::ClassFile, name: &str) -> Option<&'a [u8]> {
    cf.attributes.iter().find_map(|a| {
        let n = cf.utf8(a.name_index)?;
        (n == name.as_bytes()).then_some(a.info.as_slice())
    })
}

/// class 文件里第 `idx` 个常量池项如果是 `Class`，返回内部类名
fn cp_class_name(cf: &crate::ClassFile, idx: u16) -> Option<String> {
    match cf.constant_pool.get(idx as usize).and_then(|e| e.as_ref())? {
        crate::CpEntry::Class(n) => cf.utf8(*n).map(|b| String::from_utf8_lossy(b).into_owned()),
        _ => None,
    }
}

/// `u2 count` + `u2 entries[count]` 形态的属性（`NestMembers` / `PermittedSubclasses`）
fn class_list_attr(cf: &crate::ClassFile, name: &str) -> Option<Vec<String>> {
    let b = cf_attr(cf, name)?;
    let n = u16::from_be_bytes([*b.first()?, *b.get(1)?]) as usize;
    let mut out = Vec::with_capacity(n);
    for k in 0..n {
        let i = 2 + k * 2;
        let idx = u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]);
        out.extend(cp_class_name(cf, idx));
    }
    Some(out)
}

/// `getNestHost0()`：有 `NestHost` 属性就取它指的那个类，否则**是自己**
/// （顶级类没有 `NestHost` 属性，规范规定此时 nest host 就是自己）
fn cls_nest_host(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let me = vm.class_obj_name(f.this()).unwrap_or_default();
    Ok(Slot::I(vm.class_obj(&nest_host_of(vm, &me))))
}

/// `NestHost` 属性指向的类；没有该属性（顶级类 / nest 自己就是 host）→ 自己
fn nest_host_of(vm: &Vm, me: &str) -> String {
    vm.store
        .class_file(me)
        .and_then(|cf| {
            let b = cf_attr(&cf, "NestHost")?;
            if b.len() < 2 {
                return None;
            }
            cp_class_name(&cf, u16::from_be_bytes([b[0], b[1]]))
        })
        .unwrap_or_else(|| me.to_string())
}

/// `getNestMembers0()`：**第一位是 host 自己**，后面是 `NestMembers` 属性里的成员。
///
/// 顺序不能改：`Class.getNestMembers()` 就是把本 native 的结果原样返回
/// （`javap -c` 里没有拼接步骤），所以 host 必须由这里放进去。
/// 没有 `NestMembers` 属性（只有自己的 nest）→ 单元素数组。
fn cls_nest_members(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let me = vm.class_obj_name(f.this()).unwrap_or_default();
    let host = nest_host_of(vm, &me);
    let members = vm
        .store
        .class_file(&me)
        .and_then(|cf| class_list_attr(&cf, "NestMembers"))
        .unwrap_or_default();
    let mut slots = vec![Slot::I(vm.class_obj(&host))];
    for n in members {
        if n != host && n != me {
            slots.push(Slot::I(vm.class_obj(&n)));
        }
    }
    let h = vm.sh.heap.new_array(slots, "[Ljava/lang/Class;").unwrap_or(0);
    Ok(Slot::I(h))
}

/// `getDeclaredClasses0()`：`InnerClasses` 里 `outer_class_info_index == 自己` 的那些（= 成员类）
fn cls_declared_classes(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let me = vm.class_obj_name(f.this()).unwrap_or_default();
    let mut names: Vec<String> = Vec::new();
    if let Some(cf) = vm.store.class_file(&me) {
        let this_idx = cf.this_class;
        if let Some(b) = cf_attr(&cf, "InnerClasses") {
            let n = b.len().checked_sub(2).map(|_| u16::from_be_bytes([b[0], b[1]]) as usize).unwrap_or(0);
            for k in 0..n {
                let i = 2 + k * 8;
                if i + 8 > b.len() {
                    break;
                }
                let outer = u16::from_be_bytes([b[i + 2], b[i + 3]]);
                // 只收成员类：外部类是**自己**的那些
                if outer != this_idx {
                    continue;
                }
                let inner = u16::from_be_bytes([b[i], b[i + 1]]);
                names.extend(cp_class_name(&cf, inner));
            }
        }
    }
    let mut slots: Vec<Slot> = Vec::with_capacity(names.len());
    for n in names {
        slots.push(Slot::I(vm.class_obj(&n)));
    }
    let h = vm.sh.heap.new_array(slots, "[Ljava/lang/Class;").unwrap_or(0);
    Ok(Slot::I(h))
}

/// `isRecord0()`：看 class 文件有没有 `Record` 属性（有就是 record）
fn cls_is_record(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let me = vm.class_obj_name(f.this()).unwrap_or_default();
    let is = match vm.store.class_file(&me) {
        Some(cf) => cf_attr(&cf, "Record").is_some(),
        None => false,
    };
    Ok(Slot::I(if is { 1 } else { 0 }))
}

/// `getRecordComponents0()`：record 的组件要造 `java.lang.reflect.RecordComponent` 对象
/// （每个还带 accessor `Method`）。本 VM 还没有这条构造链 —— **非 record 返回 null**
/// 与 JDK 一致；真是 record 时返回 null 会让 `getRecordComponents()` 给 null（HotSpot 给数组），
/// 这是已知缺口，见 `docs/native-缺口.md`。
fn cls_record_components(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::I(0))
}

/// `getPermittedSubclasses0()`：`PermittedSubclasses` 属性；没有该属性 → null（= 不是 sealed）
fn cls_permitted(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let me = vm.class_obj_name(f.this()).unwrap_or_default();
    let names = match vm.store.class_file(&me).and_then(|cf| class_list_attr(&cf, "PermittedSubclasses")) {
        Some(n) => n,
        None => return Ok(Slot::I(0)),
    };
    let mut slots: Vec<Slot> = Vec::with_capacity(names.len());
    for n in names {
        slots.push(Slot::I(vm.class_obj(&n)));
    }
    let h = vm.sh.heap.new_array(slots, "[Ljava/lang/Class;").unwrap_or(0);
    Ok(Slot::I(h))
}

/// `getClassFileVersion0()`：class 文件的 major version；原始类型 / 数组没有 class 文件 → 0
fn cls_file_version(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let name = vm.class_obj_name(f.this()).unwrap_or_default();
    let v = vm.store.class_file(&name).map(|cf| cf.major_version as i32).unwrap_or(0);
    Ok(Slot::I(v))
}

/// `getClassAccessFlagsRaw0()`：class 文件的 `access_flags` 原值（**不摘** ACC_SUPER，
/// 与 HotSpot 一致；摘标志是 `getModifiers` 的事）
fn cls_access_flags_raw(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let name = vm.class_obj_name(f.this()).unwrap_or_default();
    let flags = vm.store.class_file(&name).map(|cf| cf.access_flags as i32).unwrap_or(0);
    Ok(Slot::I(flags))
}

fn class_reg(r: &mut NativeRegistry) {
    const C: &str = "java/lang/Class";
    r.add(C, "getEnclosingMethod0", "()[Ljava/lang/Object;", cls_enclosing_method);
    r.add(C, "getRawAnnotations", "()[B", cls_raw_annos);
    r.add(C, "getRawTypeAnnotations", "()[B", cls_raw_annos);
    r.add(C, "getConstantPool", "()Ljdk/internal/reflect/ConstantPool;", cls_constant_pool);
    r.add(C, "getDeclaredClasses0", "()[Ljava/lang/Class;", cls_declared_classes);
    r.add(C, "getRecordComponents0", "()[Ljava/lang/reflect/RecordComponent;", cls_record_components);
    r.add(C, "isRecord0", "()Z", cls_is_record);
    r.add(C, "getNestHost0", "()Ljava/lang/Class;", cls_nest_host);
    r.add(C, "getNestMembers0", "()[Ljava/lang/Class;", cls_nest_members);
    r.add(C, "getPermittedSubclasses0", "()[Ljava/lang/Class;", cls_permitted);
    r.add(C, "getClassFileVersion0", "()I", cls_file_version);
    r.add(C, "getClassAccessFlagsRaw0", "()I", cls_access_flags_raw);
    // 注意：`reflect.rs` 已经接管了 forName0 / getDeclared{Fields,Methods,Constructors}0 /
    // getInterfaces0 / getDeclaringClass0 / getSimpleBinaryName0 / getGenericSignature0，
    // 这里**不重复登记**（重复登记会顶掉它那份实现，得不偿失）。
}

// ================================================================ java.lang.invoke.VarHandle（**不注册**）
//
// VarHandle 的 31 个 native **故意一个都不注册**，理由是实测出来的、不是省事：
//
// 1. **主路径在查注册表之前就被截走了**。`exec.rs` 的 invokevirtual / invokestatic /
//    invokespecial 三个分支都是先判 `is_invoke_api_class(&info.class)`
//    （`indy.rs:1418`：`class.starts_with("java/lang/invoke/")`），命中就交给
//    `indy::invoke_api`，**根本走不到 `require_native`**。VarHandle 的
//    `get/set/compareAndSet/…` 在 `indy::varhandle_op`（`indy.rs:1293` 起）里按
//    **调用点描述符**（签名多态）实现，字段 / 静态 / 数组元素 / 字节视图四种形态都在那边。
//
// 2. **唯一走得到的路径是反射**（`Method.invoke(vh, (Object) new Object[]{holder})`），
//    而那条路上 HotSpot 自己就拒绝：`VarHandle.get` 是签名多态方法，反射调用它会抛
//    `InvocationTargetException`（实测 JDK 25；本 VM 里注册过时会返回 5，等于**造出一个
//    和 HotSpot 不一致的行为**）。见 `D:/vmless/nat/VhReflect.java`。
//
// 3. 所以注册在这里是**死代码**，而且会误导下一个人「VarHandle 是 native 实现的」。
//    要改 VarHandle 语义，改 `indy.rs`，不要改本文件。
//
// 同理 `java.lang.invoke.MethodHandle`（8 个）与 `MethodHandleNatives`（12 个）也不注册：
// 它们的调用全被 `invoke_api` 截走，注册了永远不会被调用。

// ================================================================ jdk.internal.reflect.ConstantPool
//
// 只在「有人从 `Class.getConstantPool()` 拿池子直接读」时才会用到。本 VM 没有把
// 常量池暴露成 Java 对象（`Class.getConstantPool()` 给的是空壳），所以这里一律给
// **默认值**：`getSize0` = 0（其余读接口调用方都会先看 size）。返回垃圾数据比返回 0 更糟。

fn cp_zero(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::I(0))
}
fn cp_zero_long(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::L(0))
}
fn cp_zero_float(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::Fl(0.0))
}
fn cp_zero_double(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::F(0.0))
}
fn cp_null(_: &mut Vm, _: &mut Frame) -> Result<Slot, VmError> {
    Ok(Slot::I(0))
}

fn constant_pool_reg(r: &mut NativeRegistry) {
    const P: &str = "jdk/internal/reflect/ConstantPool";
    r.add(P, "getSize0", "(Ljava/lang/Object;)I", cp_zero);
    r.add(P, "getIntAt0", "(Ljava/lang/Object;I)I", cp_zero);
    r.add(P, "getTagAt0", "(Ljava/lang/Object;I)B", cp_zero);
    r.add(P, "getClassRefIndexAt0", "(Ljava/lang/Object;I)I", cp_zero);
    r.add(P, "getNameAndTypeRefIndexAt0", "(Ljava/lang/Object;I)I", cp_zero);
    r.add(P, "getLongAt0", "(Ljava/lang/Object;I)J", cp_zero_long);
    r.add(P, "getFloatAt0", "(Ljava/lang/Object;I)F", cp_zero_float);
    r.add(P, "getDoubleAt0", "(Ljava/lang/Object;I)D", cp_zero_double);
    r.add(P, "getClassAt0", "(Ljava/lang/Object;I)Ljava/lang/Class;", cp_null);
    r.add(P, "getClassAtIfLoaded0", "(Ljava/lang/Object;I)Ljava/lang/Class;", cp_null);
    r.add(P, "getMethodAt0", "(Ljava/lang/Object;I)Ljava/lang/reflect/Member;", cp_null);
    r.add(P, "getMethodAtIfLoaded0", "(Ljava/lang/Object;I)Ljava/lang/reflect/Member;", cp_null);
    r.add(P, "getFieldAt0", "(Ljava/lang/Object;I)Ljava/lang/reflect/Field;", cp_null);
    r.add(P, "getFieldAtIfLoaded0", "(Ljava/lang/Object;I)Ljava/lang/reflect/Field;", cp_null);
    r.add(P, "getStringAt0", "(Ljava/lang/Object;I)Ljava/lang/String;", cp_null);
    r.add(P, "getUTF8At0", "(Ljava/lang/Object;I)Ljava/lang/String;", cp_null);
    r.add(P, "getMemberRefInfoAt0", "(Ljava/lang/Object;I)[Ljava/lang/String;", cp_null);
    r.add(P, "getNameAndTypeRefInfoAt0", "(Ljava/lang/Object;I)[Ljava/lang/String;", cp_null);
}
