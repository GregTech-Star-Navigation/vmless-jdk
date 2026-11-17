//! JNI（Java Native Interface）的 C ABI 实现 —— **native 库看到的那一层**。
//!
//! 这个模块只做一件事：把 JNI 规范里的 `JNIEnv*` / `JavaVM*` 变成能真的被
//! C 动态库调用的东西。**函数表槽位按 `jni.h` 的声明顺序**（见 `SLOT_NAMES`，
//! 由 JDK 的 `include/jni.h` 逐行核对生成）：槽位错一位，native 库就会调到别的
//! 函数上，而且不会有任何报错，只会在某次调用里静默崩掉。
//!
//! ## 三层分工
//!
//! ```text
//! jni_lib.rs   动态库装载 / JNI_OnLoad / RegisterNatives / 短名绑定 / 反向调 Java
//! 本文件       JNIEnv 与 JNINativeInterface_（229+7 个槽位）、引用/字符串/数组/字段/方法
//! native_reg   `(类,名,描述符)` 查表；`ACC_NATIVE` 方法最终落到 `jni_lib` 的跳板
//! ```
//!
//! ## 句柄即引用
//!
//! 这个 VM 的引用句柄（`i32`：正数=对象、负数=数组、`0`=null、`>= STR_BASE`=字符串）
//! **不会移动、也没有 GC**（见 `gc.rs` 的说明），所以局部引用/全局引用可以就是句柄本身：
//! 它们天然在一次 native 调用内有效，而且永远不会悬垂。`NewGlobalRef` 额外把句柄登记进
//! 一张全局表（这样 `DeleteGlobalRef` 之后再 `GetObjectRefType` 能答"不是全局引用"，
//! 也让「有没有泄漏」可被测试断言，而不是靠猜）。
//!
//! ## 异常
//!
//! native 里 `ThrowNew` 只把句柄记进本次调用的上下文；**native 返回时**由
//! `jni_lib::trampoline` 把它变成 `VmError { exc: Some(句柄) }`，于是 Java 侧的
//! `catch` 能正常接住（和 HotSpot 的语义一致：异常在 native 返回前不会被 Java 看见）。
//!
//! ## panic 不能穿 FFI
//!
//! 每个导出函数都过 `catch_unwind`（`jni!` 宏），panic 变成"抛一个 RuntimeException"
//! 而不是让进程 abort。
#![allow(non_snake_case)]

use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use super::heap::RefKind;
use super::slot::Slot;
use super::{Vm, VmError};

// ============================================================ 基本类型（jni.h）

pub(crate) type jint = i32;
pub(crate) type jlong = i64;
pub(crate) type jfloat = f32;
pub(crate) type jdouble = f64;
pub(crate) type jboolean = u8;
pub(crate) type jbyte = i8;
pub(crate) type jchar = u16;
pub(crate) type jshort = i16;
pub(crate) type jsize = jint;
pub(crate) type jsize_t = i64;

pub(crate) type jobject = *mut c_void;
pub(crate) type jclass = jobject;
pub(crate) type jstring = jobject;
pub(crate) type jarray = jobject;
pub(crate) type jthrowable = jobject;
pub(crate) type jmethodID = *mut c_void;
pub(crate) type jfieldID = *mut c_void;
pub(crate) type jweak = jobject;

pub(crate) const JNI_OK: jint = 0;
pub(crate) const JNI_ERR: jint = -1;
pub(crate) const JNI_VERSION_1_6: jint = 0x0001_0006;
pub(crate) const JNI_ABORT: jint = 2;

pub(crate) const JNI_INVALID_REF: jint = 0;
pub(crate) const JNI_LOCAL_REF: jint = 1;
pub(crate) const JNI_GLOBAL_REF: jint = 2;
pub(crate) const JNI_WEAK_GLOBAL_REF: jint = 3;

/// `JNIEnv`：C++ 里 `JNIEnv_` 只有一个成员 `functions`，所以 `JNIEnv*` 就是
/// 「指向一个第一个成员是函数表指针的结构」。槽位不能错，见 `SLOT_NAMES`。
#[repr(C)]
pub(crate) struct JNIEnv {
    pub(crate) functions: *const c_void,
}

/// `JavaVM`：同理，第一个成员是 `JNIInvokeInterface_*`。
#[repr(C)]
pub(crate) struct JavaVM {
    pub(crate) functions: *const c_void,
}

// ============================================================ native 调用上下文

/// 一次 native 调用（`Java_...` 或 `RegisterNatives` 注册的函数指针）的上下文。
///
/// 每次调用 push 一个，返回时 pop。**只放在线程本地**：JNI 的 `env` 本来就是
/// 每线程一份，native 代码在别的线程上用的必须自己 `AttachCurrentThread`。
pub(crate) struct Ctx {
    /// 当前线程的 VM（`&mut Vm` 被转成裸指针存进来，调用期间独占）
    pub(crate) vm: *mut Vm,
    /// 尚未被 Java 看见的异常句柄（`ThrowNew` / `Throw` 写它）
    pub(crate) pending: i32,
    /// `GetStringUTFChars` / `GetStringChars` 发出去的缓冲区：指针 → 拥有者
    strbufs: Vec<(usize, CString)>,
    strbufs16: Vec<(usize, Vec<u16>)>,
    /// `Get*ArrayElements` 发出去的缓冲区
    arrbufs: Vec<ArrBuf>,
    /// `PushLocalFrame` 的深度（只用于语义记录：局部引用本来就不会失效）
    local_frames: Vec<usize>,
    /// 本次调用里创建过的局部引用个数（诊断用）
    local_count: usize,
}

unsafe impl Send for Ctx {}

impl Ctx {
    fn new(vm: *mut Vm) -> Ctx {
        Ctx {
            vm,
            pending: 0,
            strbufs: Vec::new(),
            strbufs16: Vec::new(),
            arrbufs: Vec::new(),
            local_frames: Vec::new(),
            local_count: 0,
        }
    }
}

thread_local! {
    static STACK: RefCell<Vec<Box<Ctx>>> = const { RefCell::new(Vec::new()) };
    static ENV: Cell<*mut JNIEnv> = const { Cell::new(std::ptr::null_mut()) };
}

/// 当前线程的 `JNIEnv*`（每线程一份，第一次用时分配）
pub(crate) fn env_ptr() -> *mut JNIEnv {
    ENV.with(|e| {
        let mut p = e.get();
        if p.is_null() {
            let b = Box::new(JNIEnv { functions: native_interface() });
            p = Box::into_raw(b);
            e.set(p);
        }
        p
    })
}

/// 进一次 native 调用（`jni_lib` 的跳板调）
pub(crate) fn ctx_push(vm: *mut Vm) {
    STACK.with(|s| s.borrow_mut().push(Box::new(Ctx::new(vm))));
}

/// 出一次 native 调用，返回「还没被 Java 看见的异常句柄」（0 = 没有）
pub(crate) fn ctx_pop() -> i32 {
    STACK.with(|s| {
        let mut g = s.borrow_mut();
        let pending = g.last().map(|c| c.pending).unwrap_or(0);
        g.pop();
        pending
    })
}

/// 把 native 代码抛的异常转给别人看（`jni_lib` 用）
pub(crate) fn ctx_pending() -> i32 {
    STACK.with(|s| s.borrow().last().map(|c| c.pending).unwrap_or(0))
}

#[inline]
unsafe fn ctx() -> Option<*mut Ctx> {
    STACK.with(|s| s.borrow().last().map(|b| &**b as *const Ctx as *mut Ctx))
}

#[inline]
unsafe fn with_vm<R>(f: impl FnOnce(&mut Vm) -> R) -> Option<R> {
    let c = ctx()?;
    Some(f(&mut *(*c).vm))
}

unsafe fn set_pending(h: i32) {
    if let Some(c) = ctx() {
        (*c).pending = h;
    }
}

unsafe fn take_pending() -> i32 {
    match ctx() {
        Some(c) => std::mem::replace(&mut (*c).pending, 0),
        None => 0,
    }
}

/// panic 进 FFI 之前拦下来：打一行日志并**让调用方抛 RuntimeException**
fn note_panic() {
    let msg = "native 库调用过程中 Rust 侧 panic（已被 JNI 边界拦下）";
    eprintln!("[jni] {msg}");
    if let Some(vm) = unsafe { with_vm(|vm| vm as *mut Vm) } {
        let h = unsafe { crate::vm::Vm::java_exception(&mut *vm, "java/lang/RuntimeException", msg) };
        set_pending(h.exc.unwrap_or(0));
    }
}

/// 每个 JNI 导出函数的包装：`catch_unwind` + 出错时给默认返回值
macro_rules! jni {
    ($fname:ident, ($($a:ident : $t:ty),* $(,)?) => $ret:ty, $default:expr, $body:block) => {
        pub(crate) unsafe extern "C" fn $fname($($a: $t),*) -> $ret {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || $body)) {
                Ok(v) => v,
                Err(_) => {
                    note_panic();
                    $default
                }
            }
        }
    };
}

// ============================================================ 句柄 ↔ 指针

#[inline]
pub(crate) fn h2p(h: i32) -> jobject {
    h as usize as jobject
}

#[inline]
pub(crate) unsafe fn p2h(p: jobject) -> i32 {
    p as usize as i32
}

// ============================================================ 引用表

/// 全局引用表：句柄 → 引用次数。
///
/// 句柄本身不会失效（没有移动式 GC），这张表的作用是：① `GetObjectRefType` 能区分
/// 局部/全局；② 测试能断言 `DeleteGlobalRef` 之后表回到基线（**泄漏可测量**）。
static GLOBALS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

pub(crate) fn global_ref_count() -> usize {
    GLOBALS.lock().map(|g| g.len()).unwrap_or(0)
}

fn global_add(h: i32) {
    if let Ok(mut g) = GLOBALS.lock() {
        g.push(h);
    }
}

fn global_del(h: i32) {
    if let Ok(mut g) = GLOBALS.lock() {
        if let Some(i) = g.iter().position(|x| *x == h) {
            g.remove(i);
        }
    }
}

fn is_global(h: i32) -> bool {
    GLOBALS.lock().map(|g| g.contains(&h)).unwrap_or(false)
}

// ============================================================ 方法 ID / 字段 ID

/// `jmethodID` 指向的东西。JNI 里它是不透明指针，native 库会长期缓存。
pub(crate) struct MethodId {
    pub(crate) class: String,
    pub(crate) name: String,
    pub(crate) desc: String,
    pub(crate) is_static: bool,
    /// 参数槽位宽度（long/double 占两槽）—— 调用时按描述符算，这里存一份省重复解析
    pub(crate) _priv: (),
}

/// `jfieldID`
pub(crate) struct FieldId {
    /// **声明**该字段的类（静态字段按 `(声明类, 名字)` 存，见 `shared.rs::statics`）
    pub(crate) class: String,
    pub(crate) name: String,
    pub(crate) desc: String,
    /// 实例字段在对象里的槽位（布局「父类在前」，所以继承链上一致）
    pub(crate) slot: usize,
    pub(crate) is_static: bool,
}

static METHOD_IDS: OnceLock<Mutex<Vec<&'static MethodId>>> = OnceLock::new();
static FIELD_IDS: OnceLock<Mutex<Vec<&'static FieldId>>> = OnceLock::new();

fn intern_method_id(m: MethodId) -> jmethodID {
    let table = METHOD_IDS.get_or_init(|| Mutex::new(Vec::new()));
    let mut g = table.lock().unwrap_or_else(|e| e.into_inner());
    for old in g.iter() {
        if old.class == m.class && old.name == m.name && old.desc == m.desc && old.is_static == m.is_static {
            return *old as *const MethodId as jmethodID;
        }
    }
    let leaked: &'static MethodId = Box::leak(Box::new(m));
    g.push(leaked);
    leaked as *const MethodId as jmethodID
}

fn intern_field_id(f: FieldId) -> jfieldID {
    let table = FIELD_IDS.get_or_init(|| Mutex::new(Vec::new()));
    let mut g = table.lock().unwrap_or_else(|e| e.into_inner());
    for old in g.iter() {
        if old.class == f.class && old.name == f.name && old.desc == f.desc && old.is_static == f.is_static {
            return *old as *const FieldId as jfieldID;
        }
    }
    let leaked: &'static FieldId = Box::leak(Box::new(f));
    g.push(leaked);
    leaked as *const FieldId as jfieldID
}

#[inline]
unsafe fn method_id<'a>(id: jmethodID) -> Option<&'a MethodId> {
    if id.is_null() {
        None
    } else {
        Some(&*(id as *const MethodId))
    }
}

#[inline]
unsafe fn field_id<'a>(id: jfieldID) -> Option<&'a FieldId> {
    if id.is_null() {
        None
    } else {
        Some(&*(id as *const FieldId))
    }
}

// ============================================================ JNI 数组缓冲区

#[derive(Clone, Copy, PartialEq, Eq)]
enum ElemKind {
    Bool,
    Byte,
    Char,
    Short,
    Int,
    Long,
    Float,
    Double,
    Object,
}

impl ElemKind {
    fn size(self) -> usize {
        match self {
            ElemKind::Bool | ElemKind::Byte => 1,
            ElemKind::Char | ElemKind::Short => 2,
            ElemKind::Int | ElemKind::Float => 4,
            ElemKind::Long | ElemKind::Double | ElemKind::Object => 8,
        }
    }
    fn desc(self) -> &'static str {
        match self {
            ElemKind::Bool => "Z",
            ElemKind::Byte => "B",
            ElemKind::Char => "C",
            ElemKind::Short => "S",
            ElemKind::Int => "I",
            ElemKind::Long => "J",
            ElemKind::Float => "F",
            ElemKind::Double => "D",
            ElemKind::Object => "L",
        }
    }
}

/// `Get*ArrayElements` 发出去的一块内存：C 侧直接读写它，`Release` 时按 mode 写回。
struct ArrBuf {
    /// 对应的 Java 数组句柄
    obj: i32,
    kind: ElemKind,
    /// 元素个数
    len: usize,
    /// 以 u64 为单位分配（保证 8 字节对齐），实际按 `kind.size()` 紧凑排布
    mem: Vec<u64>,
    /// 是不是对象数组（元素是句柄，写回时要走写屏障）
    released: bool,
}

impl ArrBuf {
    fn alloc(kind: ElemKind, len: usize, src: &[Slot]) -> ArrBuf {
        let bytes = (kind.size() * len).max(8);
        let words = (bytes + 7) / 8;
        let mut mem = vec![0u64; words];
        let base = mem.as_mut_ptr() as *mut u8;
        for i in 0..len {
            let v = src.get(i).copied().unwrap_or(Slot::I(0));
            unsafe { store_elem(base, kind, i, v) };
        }
        ArrBuf { obj: 0, kind, len, mem, released: false }
    }
    fn ptr(&self) -> *mut c_void {
        self.mem.as_ptr() as *mut c_void
    }
    fn as_slice(&self) -> Vec<Slot> {
        let base = self.mem.as_ptr() as *const u8;
        (0..self.len).map(|i| unsafe { load_elem(base, self.kind, i) }).collect()
    }
}

#[inline]
unsafe fn store_elem(base: *mut u8, kind: ElemKind, i: usize, v: Slot) {
    let p = base.add(i * kind.size());
    match kind {
        ElemKind::Bool | ElemKind::Byte => *(p as *mut i8) = v.as_i32() as i8,
        ElemKind::Char | ElemKind::Short => *(p as *mut i16) = v.as_i32() as i16,
        ElemKind::Int => *(p as *mut i32) = v.as_i32(),
        ElemKind::Float => *(p as *mut f32) = v.as_f32(),
        ElemKind::Long => *(p as *mut i64) = v.as_i64(),
        ElemKind::Double => *(p as *mut f64) = v.as_f64(),
        ElemKind::Object => *(p as *mut i64) = v.as_i32() as i64,
    }
}

#[inline]
unsafe fn load_elem(base: *const u8, kind: ElemKind, i: usize) -> Slot {
    let p = base.add(i * kind.size());
    match kind {
        ElemKind::Bool | ElemKind::Byte => Slot::I(*(p as *const i8) as i32),
        ElemKind::Char | ElemKind::Short => Slot::I(*(p as *const i16) as i32),
        ElemKind::Int => Slot::I(*(p as *const i32)),
        ElemKind::Float => Slot::Fl(*(p as *const f32)),
        ElemKind::Long => Slot::L(*(p as *const i64)),
        ElemKind::Double => Slot::F(*(p as *const f64)),
        ElemKind::Object => Slot::I(*(p as *const i64) as i32),
    }
}

// ============================================================ 字符串（modified UTF-8）

/// Java 的 *modified UTF-8*：`\0` 编成 `C0 80`，补码对（surrogate）各自按 3 字节编码。
fn utf8_from_java(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        let cp = c as u32;
        if cp == 0 {
            out.extend_from_slice(&[0xC0, 0x80]);
        } else if cp < 0x80 {
            out.push(cp as u8);
        } else if cp < 0x800 {
            out.push(0xC0 | (cp >> 6) as u8);
            out.push(0x80 | (cp & 0x3F) as u8);
        } else if cp < 0x10000 {
            out.push(0xE0 | (cp >> 12) as u8);
            out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
            out.push(0x80 | (cp & 0x3F) as u8);
        } else {
            // 增补平面：拆成两个 3 字节的 CESU-8（JNI 的 modified UTF-8 就是这么干的）
            let v = cp - 0x10000;
            let hi = 0xD800 + ((v >> 10) as u32);
            let lo = 0xDC00 + ((v & 0x3FF) as u32);
            for u in [hi, lo] {
                out.push(0xE0 | (u >> 12) as u8);
                out.push(0x80 | ((u >> 6) & 0x3F) as u8);
                out.push(0x80 | (u & 0x3F) as u8);
            }
        }
    }
    out
}

fn java_from_utf8(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c < 0x80 {
            out.push(c as char);
            i += 1;
        } else if c & 0xE0 == 0xC0 {
            if i + 1 >= b.len() {
                break;
            }
            let cp = (((c & 0x1F) as u32) << 6) | (b[i + 1] & 0x3F) as u32;
            out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
            i += 2;
        } else if c & 0xF0 == 0xE0 {
            if i + 2 >= b.len() {
                break;
            }
            let cp = (((c & 0x0F) as u32) << 12)
                | (((b[i + 1] & 0x3F) as u32) << 6)
                | (b[i + 2] & 0x3F) as u32;
            match char::from_u32(cp) {
                Some(ch) => out.push(ch),
                // 孤立代理：JNI 允许（CESU-8），换成替换字符
                None => out.push('\u{fffd}'),
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

/// Java 字符串（UTF-16 码元个数）
fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

// ============================================================ 从 VM 取东西的小工具

fn class_name_of(vm: &Vm, h: i32) -> Option<String> {
    vm.class_obj_name(h)
}

fn is_str(h: i32) -> bool {
    h >= super::heap::STR_BASE
}

/// 任意字符串句柄 → 文本
fn text_of(vm: &Vm, h: i32) -> String {
    vm.text_of(h)
}

/// 类对象句柄 → 类内部名（`java/lang/String` / `[I`）
pub(crate) fn class_of_class_obj(vm: &Vm, clazz: jclass) -> Option<String> {
    if clazz.is_null() {
        return None;
    }
    let h = unsafe { p2h(clazz) };
    class_name_of(vm, h).or_else(|| vm.class_of(h))
}

/// 对象句柄的运行时类名
fn runtime_class(vm: &Vm, h: i32) -> Option<String> {
    vm.class_of(h)
}

/// 抛一个 Java 异常并返回"默认值用"的 `Err`
fn throw_new(vm: &mut Vm, class: &str, msg: &str) -> i32 {
    let e = Vm::java_exception(vm, class, msg);
    e.exc.unwrap_or(0)
}

// ============================================================ 引用

jni!(NewLocalRef, (env: *mut JNIEnv, obj: jobject) => jobject, std::ptr::null_mut(), {
    let _ = env;
    let h = p2h(obj);
    if h != 0 {
        if let Some(c) = ctx() {
            (*c).local_count += 1;
        }
    }
    obj
});

jni!(DeleteLocalRef, (env: *mut JNIEnv, obj: jobject) => (), (), {
    // 句柄不会失效（无移动式 GC），这里只做「不再是本次调用新造的引用」的记录语义。
    let _ = (env, obj);
});

jni!(NewGlobalRef, (env: *mut JNIEnv, obj: jobject) => jobject, std::ptr::null_mut(), {
    let _ = env;
    let h = p2h(obj);
    if h == 0 {
        return std::ptr::null_mut();
    }
    global_add(h);
    obj
});

jni!(DeleteGlobalRef, (env: *mut JNIEnv, obj: jobject) => (), (), {
    let _ = env;
    let h = p2h(obj);
    if h != 0 {
        global_del(h);
    }
});

jni!(NewWeakGlobalRef, (env: *mut JNIEnv, obj: jobject) => jweak, std::ptr::null_mut(), {
    // 没有 GC 就没有弱引用的语义差别：按全局引用处理（不登记，避免和强引用混在一起）
    let _ = env;
    obj
});

jni!(DeleteWeakGlobalRef, (env: *mut JNIEnv, obj: jweak) => (), (), {
    let _ = (env, obj);
});

jni!(IsSameObject, (env: *mut JNIEnv, a: jobject, b: jobject) => jboolean, 0, {
    let _ = env;
    u8::from(p2h(a) == p2h(b))
});

jni!(PushLocalFrame, (env: *mut JNIEnv, cap: jint) => jint, JNI_ERR, {
    let _ = (env, cap);
    match ctx() {
        Some(c) => {
            (*c).local_frames.push(0);
            JNI_OK
        }
        None => JNI_ERR,
    }
});

jni!(PopLocalFrame, (env: *mut JNIEnv, res: jobject) => jobject, std::ptr::null_mut(), {
    let _ = env;
    if let Some(c) = ctx() {
        (*c).local_frames.pop();
    }
    res
});

jni!(EnsureLocalCapacity, (env: *mut JNIEnv, cap: jint) => jint, JNI_ERR, {
    let _ = (env, cap);
    JNI_OK
});

jni!(GetObjectRefType, (env: *mut JNIEnv, obj: jobject) => jint, JNI_INVALID_REF, {
    let _ = env;
    let h = p2h(obj);
    if h == 0 {
        JNI_INVALID_REF
    } else if is_global(h) {
        JNI_GLOBAL_REF
    } else {
        JNI_LOCAL_REF
    }
});

// ============================================================ 异常

jni!(Throw, (env: *mut JNIEnv, obj: jthrowable) => jint, JNI_ERR, {
    let _ = env;
    let h = p2h(obj);
    if h == 0 {
        return JNI_ERR;
    }
    set_pending(h);
    JNI_OK
});

jni!(ThrowNew, (env: *mut JNIEnv, clazz: jclass, msg: *const c_char) => jint, JNI_ERR, {
    let _ = env;
    let text = if msg.is_null() { String::new() } else { CStr::from_ptr(msg).to_string_lossy().into_owned() };
    let name = match with_vm(|vm| class_of_class_obj(vm, clazz)) {
        Some(Some(n)) => n,
        _ => return JNI_ERR,
    };
    let h = with_vm(|vm| throw_new(vm, &name, &text)).unwrap_or(0);
    set_pending(h);
    JNI_OK
});

jni!(ExceptionOccurred, (env: *mut JNIEnv) => jthrowable, std::ptr::null_mut(), {
    let _ = env;
    let h = ctx_pending();
    if h == 0 {
        std::ptr::null_mut()
    } else {
        h2p(h)
    }
});

jni!(ExceptionDescribe, (env: *mut JNIEnv) => (), (), {
    let _ = env;
    let h = ctx_pending();
    if h != 0 {
        if let Some(t) = with_vm(|vm| vm.exc_text(h)) {
            eprintln!("{t}");
        }
    }
});

jni!(ExceptionClear, (env: *mut JNIEnv) => (), (), {
    let _ = env;
    take_pending();
});

jni!(ExceptionCheck, (env: *mut JNIEnv) => jboolean, 0, {
    let _ = env;
    u8::from(ctx_pending() != 0)
});

jni!(FatalError, (env: *mut JNIEnv, msg: *const c_char) => (), (), {
    let _ = env;
    let text = if msg.is_null() { String::new() } else { CStr::from_ptr(msg).to_string_lossy().into_owned() };
    eprintln!("JNI fatal error: {text}");
    std::process::abort();
});

// ============================================================ 类

jni!(GetVersion, (env: *mut JNIEnv) => jint, 0, {
    let _ = env;
    JNI_VERSION_1_6
});

jni!(FindClass, (env: *mut JNIEnv, name: *const c_char) => jclass, std::ptr::null_mut(), {
    let _ = env;
    if name.is_null() {
        return std::ptr::null_mut();
    }
    let raw = CStr::from_ptr(name).to_string_lossy().into_owned();
    find_class(&raw)
});

/// `FindClass` 的本体（`jni_lib` 的 `JNI_OnLoad` 也要用）
pub(crate) fn find_class(name: &str) -> jclass {
    // JNI 传的是内部名（`java/lang/String`），数组是 `[I` / `[Ljava/lang/String;`
    let internal = name;
    let found = match with_vm(|vm| {
        if internal.starts_with('[') || internal.len() == 1 {
            true
        } else {
            vm.ensure_class_loaded(internal)
        }
    }) {
        Some(v) => v,
        None => false,
    };
    if !found {
        let h = with_vm(|vm| throw_new(vm, "java/lang/NoClassDefFoundError", internal)).unwrap_or(0);
        set_pending(h);
        return std::ptr::null_mut();
    }
    match with_vm(|vm| vm.class_obj(internal)) {
        Some(h) if h != 0 => h2p(h),
        _ => std::ptr::null_mut(),
    }
}

jni!(GetObjectClass, (env: *mut JNIEnv, obj: jobject) => jclass, std::ptr::null_mut(), {
    let _ = env;
    let h = p2h(obj);
    if h == 0 {
        return std::ptr::null_mut();
    }
    match with_vm(|vm| runtime_class(vm, h).map(|c| vm.class_obj(&c))) {
        Some(Some(c)) if c != 0 => h2p(c),
        _ => std::ptr::null_mut(),
    }
});

jni!(GetSuperclass, (env: *mut JNIEnv, clazz: jclass) => jclass, std::ptr::null_mut(), {
    let _ = env;
    let name = match with_vm(|vm| class_of_class_obj(vm, clazz)) {
        Some(Some(n)) => n,
        _ => return std::ptr::null_mut(),
    };
    match with_vm(|vm| match vm.store.super_of(&name) {
        Some(s) => Some(vm.class_obj(&s)),
        None => None,
    }) {
        Some(Some(h)) if h != 0 => h2p(h),
        _ => std::ptr::null_mut(),
    }
});

jni!(IsAssignableFrom, (env: *mut JNIEnv, sub: jclass, sup: jclass) => jboolean, 0, {
    let _ = env;
    let (Some(a), Some(b)) = (
        with_vm(|vm| class_of_class_obj(vm, sub)).flatten(),
        with_vm(|vm| class_of_class_obj(vm, sup)).flatten(),
    ) else {
        return 0;
    };
    u8::from(with_vm(|vm| vm.type_assignable(&a, &b)).unwrap_or(false))
});

jni!(IsInstanceOf, (env: *mut JNIEnv, obj: jobject, clazz: jclass) => jboolean, 0, {
    let _ = env;
    let h = p2h(obj);
    if h == 0 {
        return 0;
    }
    let Some(name) = with_vm(|vm| class_of_class_obj(vm, clazz)).flatten() else {
        return 0;
    };
    u8::from(with_vm(|vm| vm.is_instance(h, &name)).unwrap_or(false))
});

jni!(AllocObject, (env: *mut JNIEnv, clazz: jclass) => jobject, std::ptr::null_mut(), {
    let _ = env;
    let Some(name) = with_vm(|vm| class_of_class_obj(vm, clazz)).flatten() else {
        return std::ptr::null_mut();
    };
    match with_vm(|vm| {
        let n = vm.store.field_count(&name).max(1);
        vm.sh.heap.new_object(vec![Slot::I(0); n], &name)
    }) {
        Some(Some(h)) => h2p(h),
        _ => std::ptr::null_mut(),
    }
});

// ============================================================ 方法 / 字段 ID

/// 在 `class` 的继承链上找方法（含接口 default）
fn find_method_in_chain(vm: &Vm, class: &str, name: &str, desc: &str) -> Option<String> {
    for c in vm.store.chain(class) {
        if vm.store.method_in_class(&c, name, desc).is_some() {
            return Some(c);
        }
    }
    for c in vm.store.iface_closure(class) {
        if vm.store.method_in_class(&c, name, desc).is_some() {
            return Some(c);
        }
    }
    None
}

/// 在 `class` 的继承链上找**声明**该字段的类
fn find_field_owner(vm: &Vm, class: &str, name: &str, desc: &str) -> Option<String> {
    for c in vm.store.chain(class) {
        if let Some(cf) = vm.store.class_file(&c) {
            for f in &cf.fields {
                let n = String::from_utf8_lossy(cf.utf8(f.name_index).unwrap_or_default()).into_owned();
                let d = String::from_utf8_lossy(cf.utf8(f.descriptor_index).unwrap_or_default()).into_owned();
                if n == name && d == desc {
                    return Some(c);
                }
            }
        }
    }
    None
}

jni!(GetMethodID, (env: *mut JNIEnv, clazz: jclass, name: *const c_char, sig: *const c_char) => jmethodID, std::ptr::null_mut(), {
    let _ = env;
    get_method_id(clazz, name, sig, false)
});
