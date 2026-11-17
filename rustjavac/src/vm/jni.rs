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
#![allow(non_camel_case_types)]
#![allow(unsafe_op_in_unsafe_fn)]

use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_void, CStr, CString};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::vm::heap::RefKind;
use crate::vm::Slot;
use crate::vm::{Vm, VmError};

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
    if ROOT_VM.load(Ordering::Relaxed) == 0 && !vm.is_null() {
        ROOT_VM.store(vm as usize, Ordering::Relaxed);
    }
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
    unsafe {
        if let Some(vm) = with_vm(|vm| vm as *mut Vm) {
            let h = crate::vm::Vm::java_exception(&mut *vm, "java/lang/RuntimeException", msg);
            set_pending(h.exc.unwrap_or(0));
        }
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
    h >= crate::vm::heap::STR_BASE
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
    let (loaded, has_cp) = unsafe {
        with_vm(|vm| {
            let ok = if internal.starts_with('[') || internal.len() == 1 {
                true
            } else {
                vm.ensure_class_loaded(internal)
            };
            // 判据不是"有没有 classpath"（独立模式也有），而是"java.base 有没有真字节码"：
            // 有 -> 真字节码模式，找不到类就是真找不到；没有 -> java.* 是内建替身
            let has_cp = vm.store.has_class("java/lang/Object");
            (ok, has_cp)
        })
    }
    .unwrap_or((false, false));
    // 独立模式（classpath 上没有 java.base）下 java.* 是内建替身：它们没有 .class，
    // 但"类存在"这件事必须成立，否则 native 库连 `FindClass("java/lang/RuntimeException")`
    // 都失败。真字节码模式下找不到就是真找不到 —— 照规范报错。
    if !loaded && has_cp && !internal.starts_with('[') && internal.len() != 1 {
        let h = unsafe { with_vm(|vm| throw_new(vm, "java/lang/NoClassDefFoundError", internal)) }.unwrap_or(0);
        unsafe { set_pending(h) };
        return std::ptr::null_mut();
    }
    match unsafe { with_vm(|vm| vm.class_obj(internal)) } {
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

jni!(GetStaticMethodID, (env: *mut JNIEnv, clazz: jclass, name: *const c_char, sig: *const c_char) => jmethodID, std::ptr::null_mut(), {
    let _ = env;
    get_method_id(clazz, name, sig, true)
});

unsafe fn get_method_id(clazz: jclass, name: *const c_char, sig: *const c_char, want_static: bool) -> jmethodID {
    if name.is_null() || sig.is_null() {
        return std::ptr::null_mut();
    }
    let mname = CStr::from_ptr(name).to_string_lossy().into_owned();
    let mdesc = CStr::from_ptr(sig).to_string_lossy().into_owned();
    let Some(cls) = with_vm(|vm| class_of_class_obj(vm, clazz)).flatten() else {
        return std::ptr::null_mut();
    };
    let owner = with_vm(|vm| find_method_in_chain(vm, &cls, &mname, &mdesc));
    let Some(Some(owner)) = owner else {
        let h = with_vm(|vm| throw_new(vm, "java/lang/NoSuchMethodError", &format!("{cls}.{mname}{mdesc}")))
            .unwrap_or(0);
        set_pending(h);
        return std::ptr::null_mut();
    };
    intern_method_id(MethodId { class: owner, name: mname, desc: mdesc, is_static: want_static, _priv: () })
}

jni!(GetFieldID, (env: *mut JNIEnv, clazz: jclass, name: *const c_char, sig: *const c_char) => jfieldID, std::ptr::null_mut(), {
    let _ = env;
    get_field_id(clazz, name, sig, false)
});

jni!(GetStaticFieldID, (env: *mut JNIEnv, clazz: jclass, name: *const c_char, sig: *const c_char) => jfieldID, std::ptr::null_mut(), {
    let _ = env;
    get_field_id(clazz, name, sig, true)
});

unsafe fn get_field_id(clazz: jclass, name: *const c_char, sig: *const c_char, want_static: bool) -> jfieldID {
    if name.is_null() || sig.is_null() {
        return std::ptr::null_mut();
    }
    let fname = CStr::from_ptr(name).to_string_lossy().into_owned();
    let fdesc = CStr::from_ptr(sig).to_string_lossy().into_owned();
    let Some(cls) = with_vm(|vm| class_of_class_obj(vm, clazz)).flatten() else {
        return std::ptr::null_mut();
    };
    let owner = with_vm(|vm| find_field_owner(vm, &cls, &fname, &fdesc));
    let Some(Some(owner)) = owner else {
        let h = with_vm(|vm| throw_new(vm, "java/lang/NoSuchFieldError", &format!("{cls}.{fname}:{fdesc}")))
            .unwrap_or(0);
        set_pending(h);
        return std::ptr::null_mut();
    };
    let slot = with_vm(|vm| vm.store.field_index(&owner, &fname)).unwrap_or(None).unwrap_or(0);
    intern_field_id(FieldId { class: owner, name: fname, desc: fdesc, slot, is_static: want_static })
}

// ============================================================ 字段读写

unsafe fn get_field_slot(obj: jobject, fid: jfieldID) -> Slot {
    let Some(f) = field_id(fid) else { return Slot::I(0) };
    let h = p2h(obj);
    if h == 0 || f.is_static {
        return Slot::I(0);
    }
    with_vm(|vm| vm.sh.heap.get(h, f.slot).unwrap_or(Slot::I(0))).unwrap_or(Slot::I(0))
}

unsafe fn set_field_slot(obj: jobject, fid: jfieldID, v: Slot) {
    let Some(f) = field_id(fid) else { return };
    let h = p2h(obj);
    if h == 0 || f.is_static {
        return;
    }
    let is_ref = f.desc.starts_with('L') || f.desc.starts_with('[');
    with_vm(|vm| {
        if is_ref {
            vm.sh.heap.set_ref(h, f.slot, v);
        } else {
            vm.sh.heap.set(h, f.slot, v);
        }
    });
}

unsafe fn get_static_slot(fid: jfieldID) -> Slot {
    let Some(f) = field_id(fid) else { return Slot::I(0) };
    if !f.is_static {
        return Slot::I(0);
    }
    with_vm(|vm| {
        vm.sh
            .statics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(f.class.clone(), f.name.clone()))
            .copied()
            .unwrap_or(Slot::I(0))
    })
    .unwrap_or(Slot::I(0))
}

unsafe fn set_static_slot(fid: jfieldID, v: Slot) {
    let Some(f) = field_id(fid) else { return };
    if !f.is_static {
        return;
    }
    with_vm(|vm| {
        vm.sh
            .statics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((f.class.clone(), f.name.clone()), v);
    });
}

/// 生成「按类型读写字段」的 24 个函数：
/// 实例读 / 实例写 / 静态读 / 静态写，每种基本类型 + Object
macro_rules! field_ops {
    ($(($gf:ident, $sf:ident, $gsf:ident, $ssf:ident, $ty:ty, |$v:ident: Slot| $rd:expr, |$w:ident: $wk:ident| $mk:expr);)*) => {
        $(
            jni!($gf, (env: *mut JNIEnv, obj: jobject, fid: jfieldID) => $ty, 0 as $ty, {
                let _ = env;
                let $v: Slot = get_field_slot(obj, fid);
                $rd
            });

            jni!($sf, (env: *mut JNIEnv, obj: jobject, fid: jfieldID, val: $ty) => (), (), {
                let _ = env;
                let $w: $ty = val;
                set_field_slot(obj, fid, $mk);
            });

            jni!($gsf, (env: *mut JNIEnv, clazz: jclass, fid: jfieldID) => $ty, 0 as $ty, {
                let _ = env;
                let $v: Slot = get_static_slot(fid);
                $rd
            });

            jni!($ssf, (env: *mut JNIEnv, clazz: jclass, fid: jfieldID, val: $ty) => (), (), {
                let _ = env;
                let $w: $ty = val;
                set_static_slot(fid, $mk);
            });
        )*
    };
}

field_ops! {
    (GetObjectField, SetObjectField, GetStaticObjectField, SetStaticObjectField, jobject,
     |v: Slot| v.as_i32() as jobject,
     |w: jobject| Slot::I(w as usize as i32));
    (GetBooleanField, SetBooleanField, GetStaticBooleanField, SetStaticBooleanField, jboolean,
     |v: Slot| u8::from(v.as_i32() != 0),
     |w: jboolean| Slot::I(i32::from(w)));
    (GetByteField, SetByteField, GetStaticByteField, SetStaticByteField, jbyte,
     |v: Slot| v.as_i32() as i8,
     |w: jbyte| Slot::I(w as i32));
    (GetCharField, SetCharField, GetStaticCharField, SetStaticCharField, jchar,
     |v: Slot| v.as_i32() as u16,
     |w: jchar| Slot::I(w as i32));
    (GetShortField, SetShortField, GetStaticShortField, SetStaticShortField, jshort,
     |v: Slot| v.as_i32() as i16,
     |w: jshort| Slot::I(w as i32));
    (GetIntField, SetIntField, GetStaticIntField, SetStaticIntField, jint,
     |v: Slot| v.as_i32(),
     |w: jint| Slot::I(w));
    (GetLongField, SetLongField, GetStaticLongField, SetStaticLongField, jlong,
     |v: Slot| v.as_i64(),
     |w: jlong| Slot::L(w));
    (GetFloatField, SetFloatField, GetStaticFloatField, SetStaticFloatField, jfloat,
     |v: Slot| v.as_f32(),
     |w: jfloat| Slot::Fl(w));
    (GetDoubleField, SetDoubleField, GetStaticDoubleField, SetStaticDoubleField, jdouble,
     |v: Slot| v.as_f64(),
     |w: jdouble| Slot::F(w));
}

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
        return Err(crate::vm::exception::thrown(h));
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
            Err(crate::vm::exception::thrown(h))
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
                    // 8 字节一个槽位（和 va_core 的 spill[4..8] 一一对应）
                    "movq qword ptr [rsp + 0x40], xmm0",
                    "movq qword ptr [rsp + 0x48], xmm1",
                    "movq qword ptr [rsp + 0x50], xmm2",
                    "movq qword ptr [rsp + 0x58], xmm3",
                    "mov qword ptr [rsp + 0x80], 0",
                    "mov qword ptr [rsp + 0x88], 0",
                    "mov rcx, {op}",
                    "lea rdx, [rsp + 0x20]",
                    "lea r8, [rsp + 0x130]",
                    "lea r9, [rsp + 0x80]",
                    "call {core}",
                    "movq xmm0, qword ptr [rsp + 0x80]",
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
    ($(($ca:ident, $csa:ident, $ty:ty, $conv:expr, $default:expr);)*) => {
        $(
            jni!($ca, (env: *mut JNIEnv, obj: jobject, mid: jmethodID, args: *const u64) => $ty, $default, {
                let _ = env;
                let Some(m) = method_id(mid) else { return $default };
                let types = split_params(&m.desc);
                let s = jvalues_to_slots(args, &types);
                match call_java(p2h(obj), m, s, false) {
                    Ok(v) => { let f: fn(Slot) -> $ty = $conv; f(v) }
                    Err(_) => $default,
                }
            });

            jni!($csa, (env: *mut JNIEnv, clazz: jclass, mid: jmethodID, args: *const u64) => $ty, $default, {
                let _ = env;
                let Some(m) = method_id(mid) else { return $default };
                let types = split_params(&m.desc);
                let s = jvalues_to_slots(args, &types);
                match call_java(p2h(clazz), m, s, false) {
                    Ok(v) => { let f: fn(Slot) -> $ty = $conv; f(v) }
                    Err(_) => $default,
                }
            });
        )*
    };
}

call_a_ops! {
    (CallObjectMethodA, CallStaticObjectMethodA, jobject, |v: Slot| v.as_i32() as jobject, std::ptr::null_mut());
    (CallBooleanMethodA, CallStaticBooleanMethodA, jboolean, |v: Slot| u8::from(v.as_i32() != 0), 0);
    (CallByteMethodA, CallStaticByteMethodA, jbyte, |v: Slot| v.as_i32() as i8, 0);
    (CallCharMethodA, CallStaticCharMethodA, jchar, |v: Slot| v.as_i32() as u16, 0);
    (CallShortMethodA, CallStaticShortMethodA, jshort, |v: Slot| v.as_i32() as i16, 0);
    (CallIntMethodA, CallStaticIntMethodA, jint, |v: Slot| v.as_i32(), 0);
    (CallLongMethodA, CallStaticLongMethodA, jlong, |v: Slot| v.as_i64(), 0);
    (CallFloatMethodA, CallStaticFloatMethodA, jfloat, |v: Slot| v.as_f32(), 0.0);
    (CallDoubleMethodA, CallStaticDoubleMethodA, jdouble, |v: Slot| v.as_f64(), 0.0);
    (CallVoidMethodA, CallStaticVoidMethodA, (), |_v: Slot| (), ());
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
    ($(($c:ident, $ty:ty, $conv:expr, $default:expr);)*) => {
        $(
            jni!($c, (env: *mut JNIEnv, obj: jobject, clazz: jclass, mid: jmethodID, args: *const u64) => $ty,
                 $default, {
                let _ = (env, clazz);
                let Some(m) = method_id(mid) else { return $default };
                let types = split_params(&m.desc);
                let s = jvalues_to_slots(args, &types);
                match call_java(p2h(obj), m, s, true) {
                    Ok(v) => { let f: fn(Slot) -> $ty = $conv; f(v) }
                    Err(_) => $default,
                }
            });
        )*
    };
}

call_nonvirt_a_ops! {
    (CallNonvirtualObjectMethodA, jobject, |v: Slot| v.as_i32() as jobject, std::ptr::null_mut());
    (CallNonvirtualBooleanMethodA, jboolean, |v: Slot| u8::from(v.as_i32() != 0), 0);
    (CallNonvirtualByteMethodA, jbyte, |v: Slot| v.as_i32() as i8, 0);
    (CallNonvirtualCharMethodA, jchar, |v: Slot| v.as_i32() as u16, 0);
    (CallNonvirtualShortMethodA, jshort, |v: Slot| v.as_i32() as i16, 0);
    (CallNonvirtualIntMethodA, jint, |v: Slot| v.as_i32(), 0);
    (CallNonvirtualLongMethodA, jlong, |v: Slot| v.as_i64(), 0);
    (CallNonvirtualFloatMethodA, jfloat, |v: Slot| v.as_f32(), 0.0);
    (CallNonvirtualDoubleMethodA, jdouble, |v: Slot| v.as_f64(), 0.0);
    (CallNonvirtualVoidMethodA, (), |_v: Slot| (), ());
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

// ============================================================ 字符串

jni!(NewStringUTF, (env: *mut JNIEnv, bytes: *const c_char) => jstring, std::ptr::null_mut(), {
    let _ = env;
    if bytes.is_null() {
        return std::ptr::null_mut();
    }
    let raw = CStr::from_ptr(bytes).to_bytes();
    let text = java_from_utf8(raw);
    match with_vm(|vm| vm.str_new(text)) {
        Some(h) => h2p(h),
        None => std::ptr::null_mut(),
    }
});

jni!(NewString, (env: *mut JNIEnv, chars: *const jchar, len: jsize) => jstring, std::ptr::null_mut(), {
    let _ = env;
    if chars.is_null() || len < 0 {
        return std::ptr::null_mut();
    }
    let units: Vec<u16> = (0..len as usize).map(|i| *chars.add(i)).collect();
    let text = String::from_utf16_lossy(&units);
    match with_vm(|vm| vm.str_new(text)) {
        Some(h) => h2p(h),
        None => std::ptr::null_mut(),
    }
});

jni!(GetStringLength, (env: *mut JNIEnv, s: jstring) => jsize, 0, {
    let _ = env;
    let h = p2h(s);
    if h == 0 {
        return 0;
    }
    with_vm(|vm| match text_of(vm, h) {
        t => utf16_len(&t) as jsize,
    })
    .unwrap_or(0)
});

jni!(GetStringUTFLength, (env: *mut JNIEnv, s: jstring) => jsize, 0, {
    let _ = env;
    let h = p2h(s);
    if h == 0 {
        return 0;
    }
    with_vm(|vm| utf8_from_java(&text_of(vm, h)).len() as jsize).unwrap_or(0)
});

jni!(GetStringUTFLengthAsLong, (env: *mut JNIEnv, s: jstring) => jsize_t, 0, {
    let _ = env;
    let h = p2h(s);
    if h == 0 {
        return 0;
    }
    with_vm(|vm| utf8_from_java(&text_of(vm, h)).len() as jsize_t).unwrap_or(0)
});

jni!(GetStringUTFChars, (env: *mut JNIEnv, s: jstring, is_copy: *mut jboolean) => *const c_char, std::ptr::null(), {
    let _ = env;
    if !is_copy.is_null() {
        *is_copy = 1; // 总是拷贝（没有「钉住 Java 内部缓冲」这回事）
    }
    let h = p2h(s);
    if h == 0 {
        return std::ptr::null();
    }
    let Some(text) = with_vm(|vm| text_of(vm, h)) else { return std::ptr::null() };
    let bytes = utf8_from_java(&text);
    // modified UTF-8 里 NUL 编成 C0 80，所以 CString::new 不会失败
    let c = CString::new(bytes).unwrap_or_else(|_| CString::new("").unwrap());
    let ptr = c.as_ptr();
    if let Some(ctxp) = ctx() {
        (*ctxp).strbufs.push((ptr as usize, c));
    }
    ptr
});

jni!(ReleaseStringUTFChars, (env: *mut JNIEnv, s: jstring, chars: *const c_char) => (), (), {
    let _ = (env, s);
    if let Some(c) = ctx() {
        let key = chars as usize;
        if let Some(i) = (*c).strbufs.iter().position(|(p, _)| *p == key) {
            (*c).strbufs.remove(i);
        }
    }
});

jni!(GetStringChars, (env: *mut JNIEnv, s: jstring, is_copy: *mut jboolean) => *const jchar, std::ptr::null(), {
    let _ = env;
    if !is_copy.is_null() {
        *is_copy = 1;
    }
    let h = p2h(s);
    if h == 0 {
        return std::ptr::null();
    }
    let Some(text) = with_vm(|vm| text_of(vm, h)) else { return std::ptr::null() };
    let units: Vec<u16> = text.encode_utf16().collect();
    let ptr = units.as_ptr();
    if let Some(ctxp) = ctx() {
        (*ctxp).strbufs16.push((ptr as usize, units));
    }
    ptr
});

jni!(ReleaseStringChars, (env: *mut JNIEnv, s: jstring, chars: *const jchar) => (), (), {
    let _ = (env, s);
    if let Some(c) = ctx() {
        let key = chars as usize;
        if let Some(i) = (*c).strbufs16.iter().position(|(p, _)| *p == key) {
            (*c).strbufs16.remove(i);
        }
    }
});

jni!(GetStringCritical, (env: *mut JNIEnv, s: jstring, is_copy: *mut jboolean) => *const jchar, std::ptr::null(), {
    GetStringChars(env, s, is_copy)
});

jni!(ReleaseStringCritical, (env: *mut JNIEnv, s: jstring, chars: *const jchar) => (), (), {
    ReleaseStringChars(env, s, chars)
});

jni!(GetStringRegion, (env: *mut JNIEnv, s: jstring, start: jsize, len: jsize, buf: *mut jchar) => (), (), {
    let _ = env;
    let h = p2h(s);
    if h == 0 || buf.is_null() || start < 0 || len < 0 {
        return;
    }
    let Some(text) = with_vm(|vm| text_of(vm, h)) else { return };
    let units: Vec<u16> = text.encode_utf16().collect();
    for i in 0..len as usize {
        let idx = start as usize + i;
        *buf.add(i) = units.get(idx).copied().unwrap_or(0);
    }
});

jni!(GetStringUTFRegion, (env: *mut JNIEnv, s: jstring, start: jsize, len: jsize, buf: *mut c_char) => (), (), {
    let _ = env;
    let h = p2h(s);
    if h == 0 || buf.is_null() || start < 0 || len < 0 {
        return;
    }
    let Some(text) = with_vm(|vm| text_of(vm, h)) else { return };
    let units: Vec<u16> = text.encode_utf16().collect();
    let sub: String = units
        .iter()
        .skip(start as usize)
        .take(len as usize)
        .map(|u| char::from_u32(*u as u32).unwrap_or('\u{fffd}'))
        .collect();
    let bytes = utf8_from_java(&sub);
    for (i, b) in bytes.iter().enumerate() {
        *buf.add(i) = *b as c_char;
    }
});

// ============================================================ 数组

jni!(GetArrayLength, (env: *mut JNIEnv, arr: jarray) => jsize, 0, {
    let _ = env;
    let h = p2h(arr);
    if h == 0 {
        return 0;
    }
    with_vm(|vm| vm.sh.heap.array_len(h)).unwrap_or(None).unwrap_or(0) as jsize
});

/// 数组句柄 → 元素类型（不匹配返回 None）
fn array_kind(vm: &Vm, h: i32, want: ElemKind) -> Option<Vec<Slot>> {
    let desc = vm.class_of(h)?;
    let expect = match want {
        ElemKind::Object => desc.starts_with("[L") || desc.starts_with("[[") || desc == "[Ljava/lang/Object;",
        _ => desc == format!("[{}", want.desc()),
    };
    if !expect {
        return None;
    }
    vm.sh.heap.snapshot(h)
}

macro_rules! array_ops {
    ($(($newa:ident, $geta:ident, $rela:ident, $getr:ident, $setr:ident, $kind:expr, $ty:ty);)*) => {
        $(
            jni!($newa, (env: *mut JNIEnv, len: jsize) => jarray, std::ptr::null_mut(), {
                let _ = env;
                if len < 0 {
                    return std::ptr::null_mut();
                }
                let desc = format!("[{}", $kind.desc());
                match with_vm(|vm| vm.sh.heap.alloc_array(len as usize, &desc, Slot::I(0))) {
                    Some(Some(h)) => h2p(h),
                    _ => std::ptr::null_mut(),
                }
            });

            jni!($geta, (env: *mut JNIEnv, arr: jarray, is_copy: *mut jboolean) => *mut $ty,
                 std::ptr::null_mut(), {
                let _ = env;
                if !is_copy.is_null() {
                    *is_copy = 1;
                }
                let h = p2h(arr);
                if h == 0 {
                    return std::ptr::null_mut();
                }
                let Some(src) = with_vm(|vm| array_kind(vm, h, $kind)).flatten() else {
                    return std::ptr::null_mut();
                };
                let mut buf = ArrBuf::alloc($kind, src.len(), &src);
                buf.obj = h;
                let ptr = buf.ptr() as *mut $ty;
                if let Some(c) = ctx() {
                    (*c).arrbufs.push(buf);
                }
                ptr
            });

            jni!($rela, (env: *mut JNIEnv, arr: jarray, elems: *mut $ty, mode: jint) => (), (), {
                let _ = env;
                let h = p2h(arr);
                let key = elems as usize;
                let mut buf = None;
                if let Some(c) = ctx() {
                    if let Some(i) = (*c).arrbufs.iter().position(|b| b.obj == h && b.ptr() as usize == key) {
                        buf = Some((*c).arrbufs.remove(i));
                    }
                }
                let Some(mut buf) = buf else { return };
                if mode != JNI_ABORT {
                    let vals = buf.as_slice();
                    with_vm(|vm| {
                        for (i, v) in vals.iter().enumerate() {
                            vm.sh.heap.set(h, i, *v);
                        }
                    });
                }
                buf.released = true;
            });

            jni!($getr, (env: *mut JNIEnv, arr: jarray, start: jsize, len: jsize, buf: *mut $ty) => (), (), {
                let _ = env;
                let h = p2h(arr);
                if h == 0 || buf.is_null() || start < 0 || len < 0 {
                    return;
                }
                let Some(vals) = with_vm(|vm| array_kind(vm, h, $kind)).flatten() else { return };
                for i in 0..len as usize {
                    let v = vals.get(start as usize + i).copied().unwrap_or(Slot::I(0));
                    *buf.add(i) = match $kind {
                        ElemKind::Long => v.as_i64() as $ty,
                        ElemKind::Float => v.as_f32() as $ty,
                        ElemKind::Double => v.as_f64() as $ty,
                        _ => v.as_i32() as $ty,
                    };
                }
            });

            jni!($setr, (env: *mut JNIEnv, arr: jarray, start: jsize, len: jsize, buf: *const $ty) => (), (), {
                let _ = env;
                let h = p2h(arr);
                if h == 0 || buf.is_null() || start < 0 || len < 0 {
                    return;
                }
                if with_vm(|vm| array_kind(vm, h, $kind)).flatten().is_none() {
                    return;
                }
                with_vm(|vm| {
                    for i in 0..len as usize {
                        let raw = *buf.add(i);
                        let v = match $kind {
                            ElemKind::Long => Slot::L(raw as i64),
                            ElemKind::Float => Slot::Fl(raw as f32),
                            ElemKind::Double => Slot::F(raw as f64),
                            _ => Slot::I(raw as i32),
                        };
                        vm.sh.heap.set(h, start as usize + i, v);
                    }
                });
            });
        )*
    };
}

array_ops! {
    (NewBooleanArray, GetBooleanArrayElements, ReleaseBooleanArrayElements, GetBooleanArrayRegion, SetBooleanArrayRegion, ElemKind::Bool, jboolean);
    (NewByteArray, GetByteArrayElements, ReleaseByteArrayElements, GetByteArrayRegion, SetByteArrayRegion, ElemKind::Byte, jbyte);
    (NewCharArray, GetCharArrayElements, ReleaseCharArrayElements, GetCharArrayRegion, SetCharArrayRegion, ElemKind::Char, jchar);
    (NewShortArray, GetShortArrayElements, ReleaseShortArrayElements, GetShortArrayRegion, SetShortArrayRegion, ElemKind::Short, jshort);
    (NewIntArray, GetIntArrayElements, ReleaseIntArrayElements, GetIntArrayRegion, SetIntArrayRegion, ElemKind::Int, jint);
    (NewLongArray, GetLongArrayElements, ReleaseLongArrayElements, GetLongArrayRegion, SetLongArrayRegion, ElemKind::Long, jlong);
    (NewFloatArray, GetFloatArrayElements, ReleaseFloatArrayElements, GetFloatArrayRegion, SetFloatArrayRegion, ElemKind::Float, jfloat);
    (NewDoubleArray, GetDoubleArrayElements, ReleaseDoubleArrayElements, GetDoubleArrayRegion, SetDoubleArrayRegion, ElemKind::Double, jdouble);
}

jni!(NewObjectArray, (env: *mut JNIEnv, len: jsize, elem: jclass, init: jobject) => jarray, std::ptr::null_mut(), {
    let _ = env;
    if len < 0 {
        return std::ptr::null_mut();
    }
    let Some(elem_name) = with_vm(|vm| class_of_class_obj(vm, elem)).flatten() else {
        return std::ptr::null_mut();
    };
    let desc = if elem_name.starts_with('[') {
        format!("[{elem_name}")
    } else {
        format!("[L{elem_name};")
    };
    let init_h = p2h(init);
    match with_vm(|vm| vm.sh.heap.alloc_array(len as usize, &desc, Slot::I(init_h))) {
        Some(Some(h)) => h2p(h),
        _ => std::ptr::null_mut(),
    }
});

jni!(GetObjectArrayElement, (env: *mut JNIEnv, arr: jarray, i: jsize) => jobject, std::ptr::null_mut(), {
    let _ = env;
    let h = p2h(arr);
    if h == 0 || i < 0 {
        return std::ptr::null_mut();
    }
    match with_vm(|vm| vm.sh.heap.get(h, i as usize)) {
        Some(Some(Slot::I(v))) if v != 0 => h2p(v),
        _ => std::ptr::null_mut(),
    }
});

jni!(SetObjectArrayElement, (env: *mut JNIEnv, arr: jarray, i: jsize, val: jobject) => (), (), {
    let _ = env;
    let h = p2h(arr);
    if h == 0 || i < 0 {
        return;
    }
    with_vm(|vm| vm.sh.heap.set_ref(h, i as usize, Slot::I(p2h(val))));
});

jni!(GetPrimitiveArrayCritical, (env: *mut JNIEnv, arr: jarray, is_copy: *mut jboolean) => *mut c_void, std::ptr::null_mut(), {
    let _ = env;
    if !is_copy.is_null() {
        *is_copy = 1;
    }
    let h = p2h(arr);
    if h == 0 {
        return std::ptr::null_mut();
    }
    let Some(desc) = with_vm(|vm| vm.class_of(h)).flatten() else { return std::ptr::null_mut() };
    let kind = match desc.as_str() {
        "[Z" => ElemKind::Bool,
        "[B" => ElemKind::Byte,
        "[C" => ElemKind::Char,
        "[S" => ElemKind::Short,
        "[I" => ElemKind::Int,
        "[J" => ElemKind::Long,
        "[F" => ElemKind::Float,
        "[D" => ElemKind::Double,
        _ => return std::ptr::null_mut(),
    };
    let Some(src) = with_vm(|vm| vm.sh.heap.snapshot(h)).flatten() else { return std::ptr::null_mut() };
    let mut buf = ArrBuf::alloc(kind, src.len(), &src);
    buf.obj = h;
    let ptr = buf.ptr();
    if let Some(c) = ctx() {
        (*c).arrbufs.push(buf);
    }
    ptr
});

jni!(ReleasePrimitiveArrayCritical, (env: *mut JNIEnv, arr: jarray, elems: *mut c_void, mode: jint) => (), (), {
    let _ = env;
    let h = p2h(arr);
    let key = elems as usize;
    let mut buf = None;
    if let Some(c) = ctx() {
        if let Some(i) = (*c).arrbufs.iter().position(|b| b.obj == h && b.ptr() as usize == key) {
            buf = Some((*c).arrbufs.remove(i));
        }
    }
    let Some(buf) = buf else { return };
    if mode != JNI_ABORT {
        let vals = buf.as_slice();
        with_vm(|vm| {
            for (i, v) in vals.iter().enumerate() {
                vm.sh.heap.set(h, i, *v);
            }
        });
    }
});

// ============================================================ 监视器 / 其它

jni!(MonitorEnter, (env: *mut JNIEnv, obj: jobject) => jint, JNI_ERR, {
    let _ = env;
    let h = p2h(obj);
    match with_vm(|vm| vm.monitor_enter(h)) {
        Some(Ok(())) => JNI_OK,
        Some(Err(e)) => {
            if let Some(x) = e.exc {
                set_pending(x);
            }
            JNI_ERR
        }
        None => JNI_ERR,
    }
});

jni!(MonitorExit, (env: *mut JNIEnv, obj: jobject) => jint, JNI_ERR, {
    let _ = env;
    let h = p2h(obj);
    match with_vm(|vm| vm.monitor_exit(h)) {
        Some(Ok(())) => JNI_OK,
        Some(Err(e)) => {
            if let Some(x) = e.exc {
                set_pending(x);
            }
            JNI_ERR
        }
        None => JNI_ERR,
    }
});

/// `JNINativeMethod`
#[repr(C)]
pub(crate) struct JNINativeMethod {
    pub(crate) name: *const c_char,
    pub(crate) signature: *const c_char,
    pub(crate) fnPtr: *mut c_void,
}

jni!(RegisterNatives, (env: *mut JNIEnv, clazz: jclass, methods: *const JNINativeMethod, n: jint) => jint, JNI_ERR, {
    let _ = env;
    let Some(cls) = with_vm(|vm| class_of_class_obj(vm, clazz)).flatten() else {
        return JNI_ERR;
    };
    if methods.is_null() || n < 0 {
        return JNI_ERR;
    }
    let mut list = Vec::with_capacity(n as usize);
    for i in 0..n as usize {
        let m = &*methods.add(i);
        if m.name.is_null() || m.signature.is_null() {
            return JNI_ERR;
        }
        let name = CStr::from_ptr(m.name).to_string_lossy().into_owned();
        let sig = CStr::from_ptr(m.signature).to_string_lossy().into_owned();
        list.push((name, sig, m.fnPtr as usize));
    }
    let count = super::jni_lib::register_natives(&cls, &list);
    if count != list.len() {
        return JNI_ERR;
    }
    JNI_OK
});

jni!(UnregisterNatives, (env: *mut JNIEnv, clazz: jclass) => jint, JNI_ERR, {
    let _ = env;
    let Some(cls) = with_vm(|vm| class_of_class_obj(vm, clazz)).flatten() else {
        return JNI_ERR;
    };
    super::jni_lib::unregister_natives(&cls);
    JNI_OK
});

/// 全局唯一的 `JavaVM`（`JNI_OnLoad` 拿到的那个）
static JAVA_VM: Mutex<Option<usize>> = Mutex::new(None);

pub(crate) fn java_vm_ptr() -> *mut JavaVM {
    let mut g = JAVA_VM.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        let b: &'static JavaVM = Box::leak(Box::new(JavaVM { functions: invoke_interface() }));
        *g = Some(b as *const JavaVM as *mut JavaVM as usize);
    }
    g.map(|v| v as *mut JavaVM).unwrap_or(std::ptr::null_mut())
}

jni!(GetJavaVM, (env: *mut JNIEnv, out: *mut *mut JavaVM) => jint, JNI_ERR, {
    let _ = env;
    if out.is_null() {
        return JNI_ERR;
    }
    *out = java_vm_ptr();
    JNI_OK
});

// ---- 直接缓冲区（native 侧 `NewDirectByteBuffer`；Java 侧 `allocateDirect` 还没接） ----

static DIRECT_BUFS: Mutex<Vec<(i32, usize, i64)>> = Mutex::new(Vec::new());

jni!(NewDirectByteBuffer, (env: *mut JNIEnv, addr: *mut c_void, cap: jlong) => jobject, std::ptr::null_mut(), {
    let _ = env;
    if addr.is_null() {
        return std::ptr::null_mut();
    }
    let h = with_vm(|vm| {
        let name = "java/nio/DirectByteBuffer";
        let n = vm.store.field_count(name).max(1);
        vm.sh.heap.new_object(vec![Slot::I(0); n], name)
    })
    .unwrap_or(None)
    .unwrap_or(0);
    if h == 0 {
        return std::ptr::null_mut();
    }
    if let Ok(mut g) = DIRECT_BUFS.lock() {
        g.push((h, addr as usize, cap));
    }
    h2p(h)
});

jni!(GetDirectBufferAddress, (env: *mut JNIEnv, buf: jobject) => *mut c_void, std::ptr::null_mut(), {
    let _ = env;
    let h = p2h(buf);
    if h == 0 {
        return std::ptr::null_mut();
    }
    let g = DIRECT_BUFS.lock().unwrap_or_else(|e| e.into_inner());
    match g.iter().find(|(x, _, _)| *x == h) {
        Some((_, a, _)) => *a as *mut c_void,
        None => std::ptr::null_mut(),
    }
});

jni!(GetDirectBufferCapacity, (env: *mut JNIEnv, buf: jobject) => jlong, -1, {
    let _ = env;
    let h = p2h(buf);
    if h == 0 {
        return -1;
    }
    let g = DIRECT_BUFS.lock().unwrap_or_else(|e| e.into_inner());
    match g.iter().find(|(x, _, _)| *x == h) {
        Some((_, _, c)) => *c,
        None => -1,
    }
});

// ---- JDK 21+ / 22+ 追加的三个槽位 ----

jni!(GetModule, (env: *mut JNIEnv, clazz: jclass) => jobject, std::ptr::null_mut(), {
    // 这个 VM 没有模块对象；返回 null 是合法的（`Class.getModule()` 不走它）
    let _ = (env, clazz);
    std::ptr::null_mut()
});

jni!(IsVirtualThread, (env: *mut JNIEnv, obj: jobject) => jboolean, 0, {
    let _ = (env, obj);
    0
});

// ---- 未实现的槽位：**不返回 0 之外的东西，也不改异常** ----
//
// 一个 stub 还会被当成"调用成功"（返回 0/null）。对 native 库来说这比崩溃好定位：
// 只有在真的用到那些功能时才出错，而且错误点就在库那一侧。
unsafe extern "C" fn unimplemented_slot() -> u64 {
    0
}

// ---- 非 x86_64-Windows 的 `...` 垫片占位 ----
#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
macro_rules! va_shims {
    ($(($name:ident, $op:expr);)*) => {
        $(unsafe extern "C" fn $name() -> u64 { va_list_unsupported() })*
    };
}

// ============================================================ JavaVM（JNIInvokeInterface_）

/// 进程里第一个见过的 VM（`AttachCurrentThread` 从它 fork）
static ROOT_VM: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// `AttachCurrentThread` 给这个线程建的 VM（第一次 attach 时 fork）
    static ATTACHED: RefCell<Option<*mut Vm>> = const { RefCell::new(None) };
}

/// `jint DestroyJavaVM(JavaVM*)`：我们只清掉当前线程的 attach 状态
unsafe extern "C" fn vm_destroy(_vm: *mut JavaVM) -> jint {
    ATTACHED.with(|a| *a.borrow_mut() = None);
    JNI_OK
}

/// `jint AttachCurrentThread(JavaVM*, void** penv, void* args)`
///
/// native 库自己起的线程要调 Java 就必须先 attach。这里给它 fork 一个 `Vm`
/// （共享同一个 `Shared`：堆/类表/监视器都是同一份），再准备好 `JNIEnv*`。
///
/// 源 VM 取**进程里第一个见过的 VM**（`ROOT_VM`）：attach 的线程通常不是当初发起
/// native 调用的那个线程，所以不能只看当前线程的上下文。
unsafe extern "C" fn vm_attach(_vm: *mut JavaVM, penv: *mut *mut c_void, _args: *mut c_void) -> jint {
    if penv.is_null() {
        return JNI_ERR;
    }
    let attached = ATTACHED.with(|a| a.borrow().is_some());
    if !attached {
        let root = ROOT_VM.load(Ordering::Relaxed);
        if root == 0 {
            return JNI_ERR; // 进程里还没有任何 VM，这个线程没法跑 Java
        }
        let vp: *mut Vm = Box::into_raw(Box::new((*(root as *mut Vm)).fork()));
        ATTACHED.with(|a| *a.borrow_mut() = Some(vp));
        ctx_push(vp);
    }
    *penv = env_ptr() as *mut c_void;
    JNI_OK
}

unsafe extern "C" fn vm_detach(_vm: *mut JavaVM) -> jint {
    let _ = ctx_pop();
    ATTACHED.with(|a| *a.borrow_mut() = None);
    JNI_OK
}

unsafe extern "C" fn vm_get_env(_vm: *mut JavaVM, penv: *mut *mut c_void, _version: jint) -> jint {
    if penv.is_null() {
        return JNI_ERR;
    }
    // 只有本线程真的在 native 调用里（或有 attach）才给 env
    if unsafe { ctx() }.is_none() {
        return -2; // JNI_EDETACHED
    }
    *penv = env_ptr() as *mut c_void;
    JNI_OK
}

unsafe extern "C" fn vm_attach_daemon(vm: *mut JavaVM, penv: *mut *mut c_void, args: *mut c_void) -> jint {
    vm_attach(vm, penv, args)
}

static INVOKE_TABLE: OnceLock<[usize; 8]> = OnceLock::new();

pub(crate) fn invoke_interface() -> *const c_void {
    let t = INVOKE_TABLE.get_or_init(|| {
        [
            0,
            0,
            0,
            vm_destroy as *const () as usize,
            vm_attach as *const () as usize,
            vm_detach as *const () as usize,
            vm_get_env as *const () as usize,
            vm_attach_daemon as *const () as usize,
        ]
    });
    t.as_ptr() as *const c_void
}

// ============================================================ 函数表

/// 名字 → 实现。**表按 `jni.h` 的声明顺序**（`SLOT_NAMES`），而名字到实现的映射是
/// 按名字查的 —— 所以"槽位错位"这类错误在这里不可能发生（顺序只有一处：`SLOT_NAMES`）。
fn slot_fn(name: &str) -> Option<usize> {
    macro_rules! m {
        ($($n:literal => $f:ident),* $(,)?) => {
            match name {
                $($n => Some($f as *const () as usize),)*
                _ => None,
            }
        };
    }
    m! {
        "GetVersion" => GetVersion,
        "FindClass" => FindClass,
        "GetObjectClass" => GetObjectClass,
        "GetSuperclass" => GetSuperclass,
        "IsAssignableFrom" => IsAssignableFrom,
        "IsInstanceOf" => IsInstanceOf,
        "AllocObject" => AllocObject,
        "Throw" => Throw,
        "ThrowNew" => ThrowNew,
        "ExceptionOccurred" => ExceptionOccurred,
        "ExceptionDescribe" => ExceptionDescribe,
        "ExceptionClear" => ExceptionClear,
        "ExceptionCheck" => ExceptionCheck,
        "FatalError" => FatalError,
        "PushLocalFrame" => PushLocalFrame,
        "PopLocalFrame" => PopLocalFrame,
        "NewGlobalRef" => NewGlobalRef,
        "DeleteGlobalRef" => DeleteGlobalRef,
        "DeleteLocalRef" => DeleteLocalRef,
        "NewLocalRef" => NewLocalRef,
        "IsSameObject" => IsSameObject,
        "EnsureLocalCapacity" => EnsureLocalCapacity,
        "NewWeakGlobalRef" => NewWeakGlobalRef,
        "DeleteWeakGlobalRef" => DeleteWeakGlobalRef,
        "GetObjectRefType" => GetObjectRefType,
        "GetMethodID" => GetMethodID,
        "GetStaticMethodID" => GetStaticMethodID,
        "GetFieldID" => GetFieldID,
        "GetStaticFieldID" => GetStaticFieldID,

        // ---- 字段读写（实例 + 静态） ----
        "GetObjectField" => GetObjectField,
        "SetObjectField" => SetObjectField,
        "GetBooleanField" => GetBooleanField,
        "SetBooleanField" => SetBooleanField,
        "GetByteField" => GetByteField,
        "SetByteField" => SetByteField,
        "GetCharField" => GetCharField,
        "SetCharField" => SetCharField,
        "GetShortField" => GetShortField,
        "SetShortField" => SetShortField,
        "GetIntField" => GetIntField,
        "SetIntField" => SetIntField,
        "GetLongField" => GetLongField,
        "SetLongField" => SetLongField,
        "GetFloatField" => GetFloatField,
        "SetFloatField" => SetFloatField,
        "GetDoubleField" => GetDoubleField,
        "SetDoubleField" => SetDoubleField,
        "GetStaticObjectField" => GetStaticObjectField,
        "SetStaticObjectField" => SetStaticObjectField,
        "GetStaticBooleanField" => GetStaticBooleanField,
        "SetStaticBooleanField" => SetStaticBooleanField,
        "GetStaticByteField" => GetStaticByteField,
        "SetStaticByteField" => SetStaticByteField,
        "GetStaticCharField" => GetStaticCharField,
        "SetStaticCharField" => SetStaticCharField,
        "GetStaticShortField" => GetStaticShortField,
        "SetStaticShortField" => SetStaticShortField,
        "GetStaticIntField" => GetStaticIntField,
        "SetStaticIntField" => SetStaticIntField,
        "GetStaticLongField" => GetStaticLongField,
        "SetStaticLongField" => SetStaticLongField,
        "GetStaticFloatField" => GetStaticFloatField,
        "SetStaticFloatField" => SetStaticFloatField,
        "GetStaticDoubleField" => GetStaticDoubleField,
        "SetStaticDoubleField" => SetStaticDoubleField,

        // ---- 调用（`...` 走裸汇编垫片，`A` 直接实现，`V` 明确不支持） ----
        "NewObject" => va_new_object,
        "NewObjectV" => va_list_unsupported,
        "NewObjectA" => NewObjectA,
        "CallObjectMethod" => va_call_object,
        "CallObjectMethodV" => va_list_unsupported,
        "CallObjectMethodA" => CallObjectMethodA,
        "CallBooleanMethod" => va_call_boolean,
        "CallBooleanMethodV" => va_list_unsupported,
        "CallBooleanMethodA" => CallBooleanMethodA,
        "CallByteMethod" => va_call_byte,
        "CallByteMethodV" => va_list_unsupported,
        "CallByteMethodA" => CallByteMethodA,
        "CallCharMethod" => va_call_char,
        "CallCharMethodV" => va_list_unsupported,
        "CallCharMethodA" => CallCharMethodA,
        "CallShortMethod" => va_call_short,
        "CallShortMethodV" => va_list_unsupported,
        "CallShortMethodA" => CallShortMethodA,
        "CallIntMethod" => va_call_int,
        "CallIntMethodV" => va_list_unsupported,
        "CallIntMethodA" => CallIntMethodA,
        "CallLongMethod" => va_call_long,
        "CallLongMethodV" => va_list_unsupported,
        "CallLongMethodA" => CallLongMethodA,
        "CallFloatMethod" => va_call_float,
        "CallFloatMethodV" => va_list_unsupported,
        "CallFloatMethodA" => CallFloatMethodA,
        "CallDoubleMethod" => va_call_double,
        "CallDoubleMethodV" => va_list_unsupported,
        "CallDoubleMethodA" => CallDoubleMethodA,
        "CallVoidMethod" => va_call_void,
        "CallVoidMethodV" => va_list_unsupported,
        "CallVoidMethodA" => CallVoidMethodA,
        "CallNonvirtualObjectMethod" => va_call_nonvirtual_object,
        "CallNonvirtualObjectMethodV" => va_list_unsupported,
        "CallNonvirtualObjectMethodA" => CallNonvirtualObjectMethodA,
        "CallNonvirtualBooleanMethod" => va_call_nonvirtual_boolean,
        "CallNonvirtualBooleanMethodV" => va_list_unsupported,
        "CallNonvirtualBooleanMethodA" => CallNonvirtualBooleanMethodA,
        "CallNonvirtualByteMethod" => va_call_nonvirtual_byte,
        "CallNonvirtualByteMethodV" => va_list_unsupported,
        "CallNonvirtualByteMethodA" => CallNonvirtualByteMethodA,
        "CallNonvirtualCharMethod" => va_call_nonvirtual_char,
        "CallNonvirtualCharMethodV" => va_list_unsupported,
        "CallNonvirtualCharMethodA" => CallNonvirtualCharMethodA,
        "CallNonvirtualShortMethod" => va_call_nonvirtual_short,
        "CallNonvirtualShortMethodV" => va_list_unsupported,
        "CallNonvirtualShortMethodA" => CallNonvirtualShortMethodA,
        "CallNonvirtualIntMethod" => va_call_nonvirtual_int,
        "CallNonvirtualIntMethodV" => va_list_unsupported,
        "CallNonvirtualIntMethodA" => CallNonvirtualIntMethodA,
        "CallNonvirtualLongMethod" => va_call_nonvirtual_long,
        "CallNonvirtualLongMethodV" => va_list_unsupported,
        "CallNonvirtualLongMethodA" => CallNonvirtualLongMethodA,
        "CallNonvirtualFloatMethod" => va_call_nonvirtual_float,
        "CallNonvirtualFloatMethodV" => va_list_unsupported,
        "CallNonvirtualFloatMethodA" => CallNonvirtualFloatMethodA,
        "CallNonvirtualDoubleMethod" => va_call_nonvirtual_double,
        "CallNonvirtualDoubleMethodV" => va_list_unsupported,
        "CallNonvirtualDoubleMethodA" => CallNonvirtualDoubleMethodA,
        "CallNonvirtualVoidMethod" => va_call_nonvirtual_void,
        "CallNonvirtualVoidMethodV" => va_list_unsupported,
        "CallNonvirtualVoidMethodA" => CallNonvirtualVoidMethodA,
        "CallStaticObjectMethod" => va_call_static_object,
        "CallStaticObjectMethodV" => va_list_unsupported,
        "CallStaticObjectMethodA" => CallStaticObjectMethodA,
        "CallStaticBooleanMethod" => va_call_static_boolean,
        "CallStaticBooleanMethodV" => va_list_unsupported,
        "CallStaticBooleanMethodA" => CallStaticBooleanMethodA,
        "CallStaticByteMethod" => va_call_static_byte,
        "CallStaticByteMethodV" => va_list_unsupported,
        "CallStaticByteMethodA" => CallStaticByteMethodA,
        "CallStaticCharMethod" => va_call_static_char,
        "CallStaticCharMethodV" => va_list_unsupported,
        "CallStaticCharMethodA" => CallStaticCharMethodA,
        "CallStaticShortMethod" => va_call_static_short,
        "CallStaticShortMethodV" => va_list_unsupported,
        "CallStaticShortMethodA" => CallStaticShortMethodA,
        "CallStaticIntMethod" => va_call_static_int,
        "CallStaticIntMethodV" => va_list_unsupported,
        "CallStaticIntMethodA" => CallStaticIntMethodA,
        "CallStaticLongMethod" => va_call_static_long,
        "CallStaticLongMethodV" => va_list_unsupported,
        "CallStaticLongMethodA" => CallStaticLongMethodA,
        "CallStaticFloatMethod" => va_call_static_float,
        "CallStaticFloatMethodV" => va_list_unsupported,
        "CallStaticFloatMethodA" => CallStaticFloatMethodA,
        "CallStaticDoubleMethod" => va_call_static_double,
        "CallStaticDoubleMethodV" => va_list_unsupported,
        "CallStaticDoubleMethodA" => CallStaticDoubleMethodA,
        "CallStaticVoidMethod" => va_call_static_void,
        "CallStaticVoidMethodV" => va_list_unsupported,
        "CallStaticVoidMethodA" => CallStaticVoidMethodA,

        // ---- 字符串 ----
        "NewString" => NewString,
        "GetStringLength" => GetStringLength,
        "GetStringChars" => GetStringChars,
        "ReleaseStringChars" => ReleaseStringChars,
        "NewStringUTF" => NewStringUTF,
        "GetStringUTFLength" => GetStringUTFLength,
        "GetStringUTFChars" => GetStringUTFChars,
        "ReleaseStringUTFChars" => ReleaseStringUTFChars,
        "GetStringRegion" => GetStringRegion,
        "GetStringUTFRegion" => GetStringUTFRegion,
        "GetStringCritical" => GetStringCritical,
        "ReleaseStringCritical" => ReleaseStringCritical,
        "GetStringUTFLengthAsLong" => GetStringUTFLengthAsLong,

        // ---- 数组 ----
        "GetArrayLength" => GetArrayLength,
        "NewObjectArray" => NewObjectArray,
        "GetObjectArrayElement" => GetObjectArrayElement,
        "SetObjectArrayElement" => SetObjectArrayElement,
        "NewBooleanArray" => NewBooleanArray,
        "NewByteArray" => NewByteArray,
        "NewCharArray" => NewCharArray,
        "NewShortArray" => NewShortArray,
        "NewIntArray" => NewIntArray,
        "NewLongArray" => NewLongArray,
        "NewFloatArray" => NewFloatArray,
        "NewDoubleArray" => NewDoubleArray,
        "GetBooleanArrayElements" => GetBooleanArrayElements,
        "GetByteArrayElements" => GetByteArrayElements,
        "GetCharArrayElements" => GetCharArrayElements,
        "GetShortArrayElements" => GetShortArrayElements,
        "GetIntArrayElements" => GetIntArrayElements,
        "GetLongArrayElements" => GetLongArrayElements,
        "GetFloatArrayElements" => GetFloatArrayElements,
        "GetDoubleArrayElements" => GetDoubleArrayElements,
        "ReleaseBooleanArrayElements" => ReleaseBooleanArrayElements,
        "ReleaseByteArrayElements" => ReleaseByteArrayElements,
        "ReleaseCharArrayElements" => ReleaseCharArrayElements,
        "ReleaseShortArrayElements" => ReleaseShortArrayElements,
        "ReleaseIntArrayElements" => ReleaseIntArrayElements,
        "ReleaseLongArrayElements" => ReleaseLongArrayElements,
        "ReleaseFloatArrayElements" => ReleaseFloatArrayElements,
        "ReleaseDoubleArrayElements" => ReleaseDoubleArrayElements,
        "GetBooleanArrayRegion" => GetBooleanArrayRegion,
        "GetByteArrayRegion" => GetByteArrayRegion,
        "GetCharArrayRegion" => GetCharArrayRegion,
        "GetShortArrayRegion" => GetShortArrayRegion,
        "GetIntArrayRegion" => GetIntArrayRegion,
        "GetLongArrayRegion" => GetLongArrayRegion,
        "GetFloatArrayRegion" => GetFloatArrayRegion,
        "GetDoubleArrayRegion" => GetDoubleArrayRegion,
        "SetBooleanArrayRegion" => SetBooleanArrayRegion,
        "SetByteArrayRegion" => SetByteArrayRegion,
        "SetCharArrayRegion" => SetCharArrayRegion,
        "SetShortArrayRegion" => SetShortArrayRegion,
        "SetIntArrayRegion" => SetIntArrayRegion,
        "SetLongArrayRegion" => SetLongArrayRegion,
        "SetFloatArrayRegion" => SetFloatArrayRegion,
        "SetDoubleArrayRegion" => SetDoubleArrayRegion,
        "GetPrimitiveArrayCritical" => GetPrimitiveArrayCritical,
        "ReleasePrimitiveArrayCritical" => ReleasePrimitiveArrayCritical,

        // ---- 注册 / 监视器 / VM ----
        "RegisterNatives" => RegisterNatives,
        "UnregisterNatives" => UnregisterNatives,
        "MonitorEnter" => MonitorEnter,
        "MonitorExit" => MonitorExit,
        "GetJavaVM" => GetJavaVM,
        "GetModule" => GetModule,
        "IsVirtualThread" => IsVirtualThread,
        "NewDirectByteBuffer" => NewDirectByteBuffer,
        "GetDirectBufferAddress" => GetDirectBufferAddress,
        "GetDirectBufferCapacity" => GetDirectBufferCapacity,
    }
}

static NATIVE_INTERFACE: OnceLock<Box<[usize; SLOT_NAMES.len()]>> = OnceLock::new();

/// `JNINativeInterface_`：整个进程共用一张
pub(crate) fn native_interface() -> *const c_void {
    let t = NATIVE_INTERFACE.get_or_init(|| {
        let mut t = Box::new([0usize; SLOT_NAMES.len()]);
        for (i, name) in SLOT_NAMES.iter().enumerate() {
            t[i] = slot_fn(name).unwrap_or(unimplemented_slot as *const () as usize);
        }
        t
    });
    t.as_ptr() as *const c_void
}

/// 已经真实现的槽位个数（诊断/报告用：不是 0 也不是那几个 stub）
pub(crate) fn implemented_slots() -> usize {
    let mut n = 0;
    for name in SLOT_NAMES.iter() {
        match slot_fn(name) {
            Some(f) if f != va_list_unsupported as *const () as usize => n += 1,
            _ => {}
        }
    }
    n
}

/// 没实现的槽位名单（报告用）
pub(crate) fn missing_slots() -> Vec<&'static str> {
    SLOT_NAMES.iter().filter(|n| slot_fn(n).is_none()).copied().collect()
}

/// 显式声明「不支持」的槽位（CallXMethodV 这些 va_list 形态）
pub(crate) fn unsupported_slots() -> Vec<&'static str> {
    SLOT_NAMES
        .iter()
        .filter(|n| matches!(slot_fn(n), Some(f) if f == va_list_unsupported as *const () as usize))
        .copied()
        .collect()
}

// ============================================================ 调用 native（VM → C）

/// 一次 C 调用的参数帧。`gpr` = 整数/指针参数（rcx,rdx,r8,r9），`xmm` = 浮点参数，
/// `stack` = 第 5 个及以后的参数（**Win64 的约定：浮点参数超过 4 个也压栈**）。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[repr(C)]
pub(crate) struct CallFrame {
    pub(crate) fp: usize,
    pub(crate) n: usize,
    pub(crate) gpr: [u64; 4],
    pub(crate) xmm: [u64; 4],
    pub(crate) stack: [u64; 32],
    pub(crate) out: [u64; 2],
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
#[unsafe(naked)]
unsafe extern "C" fn call_stub(_f: *mut CallFrame) {
    core::arch::naked_asm!(
        "sub rsp, 0x148",
        "mov qword ptr [rsp + 0x120], rcx",
        // 浮点参数：xmm0..3
        "movq xmm0, qword ptr [rcx + 0x30]",
        "movq xmm1, qword ptr [rcx + 0x38]",
        "movq xmm2, qword ptr [rcx + 0x40]",
        "movq xmm3, qword ptr [rcx + 0x48]",
        // 第 5 个起压栈
        "mov r10, qword ptr [rcx + 0x08]",
        "sub r10, 4",
        "jle 1f",
        "xor r11d, r11d",
        "0:",
        "mov rax, qword ptr [rcx + r11*8 + 0x50]",
        "mov qword ptr [rsp + r11*8 + 0x20], rax",
        "inc r11",
        "cmp r11, r10",
        "jl 0b",
        "1:",
        // 整数参数：rcx,rdx,r8,r9（先存 fp，再把 gpr0 放回 rcx）
        "mov rax, qword ptr [rcx]",
        "mov qword ptr [rsp + 0x128], rax",
        "mov rax, qword ptr [rcx + 0x10]",
        "mov rdx, qword ptr [rcx + 0x18]",
        "mov r8,  qword ptr [rcx + 0x20]",
        "mov r9,  qword ptr [rcx + 0x28]",
        "mov rcx, rax",
        "call qword ptr [rsp + 0x128]",
        // 返回值：整数在 rax，浮点在 xmm0
        "mov r11, qword ptr [rsp + 0x120]",
        "mov qword ptr [r11 + 0x150], rax",
        "movq qword ptr [r11 + 0x158], xmm0",
        "add rsp, 0x148",
        "ret",
    )
}

/// 调一个 native 函数指针。`kinds[i]`：0 = 整数/指针，1 = f32，2 = f64。
///
/// 返回 `(整数返回值, 浮点返回值位模式)` —— 调用方按描述符取用。
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub(crate) unsafe fn call_native_ffi(fp: usize, args: &[u64], kinds: &[u8]) -> (u64, u64) {
    assert!(args.len() <= 36, "native 方法参数超过 36 个（不会发生）");
    let mut f = CallFrame {
        fp,
        n: args.len(),
        gpr: [0; 4],
        xmm: [0; 4],
        stack: [0; 32],
        out: [0; 2],
    };
    for (i, v) in args.iter().enumerate() {
        let k = kinds.get(i).copied().unwrap_or(0);
        let bits = if k == 1 { *v & 0xFFFF_FFFF } else { *v };
        if i < 4 {
            if k == 0 {
                f.gpr[i] = bits;
            } else {
                f.xmm[i] = bits;
            }
        } else {
            f.stack[i - 4] = bits;
        }
    }
    call_stub(&mut f);
    (f.out[0], f.out[1])
}

/// 其它架构：只能传整数/指针参数（浮点参数的寄存器规则各架构不同，这里不猜）。
#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
pub(crate) unsafe fn call_native_ffi(fp: usize, args: &[u64], kinds: &[u8]) -> (u64, u64) {
    if kinds.iter().any(|k| *k != 0) {
        eprintln!("[jni] 这个架构上不支持 native 方法的浮点参数（会传错，已拒绝）");
        return (0, 0);
    }
    let mut a = [0u64; 12];
    for (i, v) in args.iter().take(12).enumerate() {
        a[i] = *v;
    }
    type F = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
    let f: F = std::mem::transmute(fp);
    (f(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11]), 0)
}
pub(crate) const SLOT_NAMES: [&str; 236] = [
    "reserved0", "reserved1", "reserved2", "reserved3", "GetVersion", "DefineClass", "FindClass",
    "FromReflectedMethod", "FromReflectedField", "ToReflectedMethod", "GetSuperclass",
    "IsAssignableFrom", "ToReflectedField", "Throw", "ThrowNew", "ExceptionOccurred",
    "ExceptionDescribe", "ExceptionClear", "FatalError", "PushLocalFrame", "PopLocalFrame",
    "NewGlobalRef", "DeleteGlobalRef", "DeleteLocalRef", "IsSameObject", "NewLocalRef",
    "EnsureLocalCapacity", "AllocObject", "NewObject", "NewObjectV", "NewObjectA",
    "GetObjectClass", "IsInstanceOf", "GetMethodID", "CallObjectMethod", "CallObjectMethodV",
    "CallObjectMethodA", "CallBooleanMethod", "CallBooleanMethodV", "CallBooleanMethodA",
    "CallByteMethod", "CallByteMethodV", "CallByteMethodA", "CallCharMethod", "CallCharMethodV",
    "CallCharMethodA", "CallShortMethod", "CallShortMethodV", "CallShortMethodA", "CallIntMethod",
    "CallIntMethodV", "CallIntMethodA", "CallLongMethod", "CallLongMethodV", "CallLongMethodA",
    "CallFloatMethod", "CallFloatMethodV", "CallFloatMethodA", "CallDoubleMethod",
    "CallDoubleMethodV", "CallDoubleMethodA", "CallVoidMethod", "CallVoidMethodV",
    "CallVoidMethodA", "CallNonvirtualObjectMethod", "CallNonvirtualObjectMethodV",
    "CallNonvirtualObjectMethodA", "CallNonvirtualBooleanMethod", "CallNonvirtualBooleanMethodV",
    "CallNonvirtualBooleanMethodA", "CallNonvirtualByteMethod", "CallNonvirtualByteMethodV",
    "CallNonvirtualByteMethodA", "CallNonvirtualCharMethod", "CallNonvirtualCharMethodV",
    "CallNonvirtualCharMethodA", "CallNonvirtualShortMethod", "CallNonvirtualShortMethodV",
    "CallNonvirtualShortMethodA", "CallNonvirtualIntMethod", "CallNonvirtualIntMethodV",
    "CallNonvirtualIntMethodA", "CallNonvirtualLongMethod", "CallNonvirtualLongMethodV",
    "CallNonvirtualLongMethodA", "CallNonvirtualFloatMethod", "CallNonvirtualFloatMethodV",
    "CallNonvirtualFloatMethodA", "CallNonvirtualDoubleMethod", "CallNonvirtualDoubleMethodV",
    "CallNonvirtualDoubleMethodA", "CallNonvirtualVoidMethod", "CallNonvirtualVoidMethodV",
    "CallNonvirtualVoidMethodA", "GetFieldID", "GetObjectField", "GetBooleanField", "GetByteField",
    "GetCharField", "GetShortField", "GetIntField", "GetLongField", "GetFloatField",
    "GetDoubleField", "SetObjectField", "SetBooleanField", "SetByteField", "SetCharField",
    "SetShortField", "SetIntField", "SetLongField", "SetFloatField", "SetDoubleField",
    "GetStaticMethodID", "CallStaticObjectMethod", "CallStaticObjectMethodV",
    "CallStaticObjectMethodA", "CallStaticBooleanMethod", "CallStaticBooleanMethodV",
    "CallStaticBooleanMethodA", "CallStaticByteMethod", "CallStaticByteMethodV",
    "CallStaticByteMethodA", "CallStaticCharMethod", "CallStaticCharMethodV",
    "CallStaticCharMethodA", "CallStaticShortMethod", "CallStaticShortMethodV",
    "CallStaticShortMethodA", "CallStaticIntMethod", "CallStaticIntMethodV",
    "CallStaticIntMethodA", "CallStaticLongMethod", "CallStaticLongMethodV",
    "CallStaticLongMethodA", "CallStaticFloatMethod", "CallStaticFloatMethodV",
    "CallStaticFloatMethodA", "CallStaticDoubleMethod", "CallStaticDoubleMethodV",
    "CallStaticDoubleMethodA", "CallStaticVoidMethod", "CallStaticVoidMethodV",
    "CallStaticVoidMethodA", "GetStaticFieldID", "GetStaticObjectField", "GetStaticBooleanField",
    "GetStaticByteField", "GetStaticCharField", "GetStaticShortField", "GetStaticIntField",
    "GetStaticLongField", "GetStaticFloatField", "GetStaticDoubleField", "SetStaticObjectField",
    "SetStaticBooleanField", "SetStaticByteField", "SetStaticCharField", "SetStaticShortField",
    "SetStaticIntField", "SetStaticLongField", "SetStaticFloatField", "SetStaticDoubleField",
    "NewString", "GetStringLength", "GetStringChars", "ReleaseStringChars", "NewStringUTF",
    "GetStringUTFLength", "GetStringUTFChars", "ReleaseStringUTFChars", "GetArrayLength",
    "NewObjectArray", "GetObjectArrayElement", "SetObjectArrayElement", "NewBooleanArray",
    "NewByteArray", "NewCharArray", "NewShortArray", "NewIntArray", "NewLongArray",
    "NewFloatArray", "NewDoubleArray", "GetBooleanArrayElements", "GetByteArrayElements",
    "GetCharArrayElements", "GetShortArrayElements", "GetIntArrayElements", "GetLongArrayElements",
    "GetFloatArrayElements", "GetDoubleArrayElements", "ReleaseBooleanArrayElements",
    "ReleaseByteArrayElements", "ReleaseCharArrayElements", "ReleaseShortArrayElements",
    "ReleaseIntArrayElements", "ReleaseLongArrayElements", "ReleaseFloatArrayElements",
    "ReleaseDoubleArrayElements", "GetBooleanArrayRegion", "GetByteArrayRegion",
    "GetCharArrayRegion", "GetShortArrayRegion", "GetIntArrayRegion", "GetLongArrayRegion",
    "GetFloatArrayRegion", "GetDoubleArrayRegion", "SetBooleanArrayRegion", "SetByteArrayRegion",
    "SetCharArrayRegion", "SetShortArrayRegion", "SetIntArrayRegion", "SetLongArrayRegion",
    "SetFloatArrayRegion", "SetDoubleArrayRegion", "RegisterNatives", "UnregisterNatives",
    "MonitorEnter", "MonitorExit", "GetJavaVM", "GetStringRegion", "GetStringUTFRegion",
    "GetPrimitiveArrayCritical", "ReleasePrimitiveArrayCritical", "GetStringCritical",
    "ReleaseStringCritical", "NewWeakGlobalRef", "DeleteWeakGlobalRef", "ExceptionCheck",
    "NewDirectByteBuffer", "GetDirectBufferAddress", "GetDirectBufferCapacity", "GetObjectRefType",
    "GetModule", "IsVirtualThread", "GetStringUTFLengthAsLong",
];

// ============================================================ 给 jni_lib 用的入口

/// 当前线程的 VM（`jni_lib` 判断 native 方法是不是 static 时用）
pub(crate) unsafe fn current_vm() -> Option<*mut Vm> {
    ctx().map(|c| (*c).vm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_table_matches_jni_h_ordering() {
        // 槽位顺序的自检：`SLOT_NAMES` 是从 JDK 的 `include/jni.h` 里按声明顺序抓的，
        // 这里钉住几个关键锚点（2008 年以来没变过的位置）—— 它们一错就会让 native
        // 库调到别的函数上，而且不会有任何报错。
        assert_eq!(SLOT_NAMES.len(), 236);
        assert_eq!(SLOT_NAMES[4], "GetVersion");
        assert_eq!(SLOT_NAMES[6], "FindClass");
        assert_eq!(SLOT_NAMES[21], "NewGlobalRef");
        assert_eq!(SLOT_NAMES[33], "GetMethodID");
        assert_eq!(SLOT_NAMES[49], "CallIntMethod");
        assert_eq!(SLOT_NAMES[94], "GetFieldID");
        assert_eq!(SLOT_NAMES[163], "NewString");
        assert_eq!(SLOT_NAMES[169], "GetStringUTFChars");
        assert_eq!(SLOT_NAMES[179], "NewIntArray");
        assert_eq!(SLOT_NAMES[187], "GetIntArrayElements");
        assert_eq!(SLOT_NAMES[195], "ReleaseIntArrayElements");
        assert_eq!(SLOT_NAMES[215], "RegisterNatives");
        assert_eq!(SLOT_NAMES[219], "GetJavaVM");
        assert_eq!(SLOT_NAMES[232], "GetObjectRefType");
    }

    #[test]
    fn table_has_no_null_slots() {
        // 空槽位会让 native 库在调用时直接跳到 0 —— 比"调到错的函数"更难查
        let t = SLOT_NAMES.len();
        let p = native_interface() as *const usize;
        for i in 0..t {
            let f = unsafe { *p.add(i) };
            assert_ne!(f, 0, "槽位 {i}（{}）是空指针", SLOT_NAMES[i]);
        }
    }

    #[test]
    fn reports_implemented_vs_missing_slots() {
        let n = implemented_slots();
        let missing = missing_slots();
        // 至少这几个是必须真实现的（验收项直接依赖它们）
        for must in [
            "FindClass",
            "GetMethodID",
            "NewGlobalRef",
            "DeleteGlobalRef",
            "ThrowNew",
            "ExceptionCheck",
            "NewStringUTF",
            "GetStringUTFChars",
            "GetIntArrayElements",
            "ReleaseIntArrayElements",
            "RegisterNatives",
            "GetJavaVM",
        ] {
            assert!(!missing.contains(&must), "{must} 没实现");
        }
        // `CallXMethodV`（va_list 形态）既不算实现、也不在「缺」里：它是显式不支持
        assert!(n + missing.len() <= SLOT_NAMES.len());
        println!("实现的槽位 {n} / {}，没实现 {} 个：{:?}", SLOT_NAMES.len(), missing.len(), missing);
    }

    #[test]
    fn global_ref_table_returns_to_baseline() {
        // 句柄不会失效（没有 GC），但"建了没删"必须能被这张表量出来
        let base = global_ref_count();
        let fake = 42usize as jobject;
        let env = std::ptr::null_mut();
        for _ in 0..1000 {
            let g = unsafe { NewGlobalRef(env, fake) };
            assert_eq!(g, fake);
            unsafe { DeleteGlobalRef(env, g) };
        }
        assert_eq!(global_ref_count(), base, "全局引用表没有回到基线 = 泄漏");
        // 追加两次，确认计数确实是每次都涨（不是"表其实没记"）
        unsafe {
            NewGlobalRef(env, fake);
            NewGlobalRef(env, fake);
        }
        assert_eq!(global_ref_count(), base + 2);
        unsafe {
            DeleteGlobalRef(env, fake);
            DeleteGlobalRef(env, fake);
        }
        assert_eq!(global_ref_count(), base);
    }

    // ---- 调用垫片（Win64 调用约定） ----

    extern "C" fn c_add(a: i64, b: i64) -> i64 {
        a + b
    }
    extern "C" fn c_mix(a: i32, x: f32, y: f64, b: i64) -> f64 {
        a as f64 + x as f64 + y + b as f64
    }
    extern "C" fn c_many(a: i64, b: i64, c: i64, d: i64, e: i64, f: i64, g: i64) -> i64 {
        a + b + c + d + e + f + g
    }

    #[test]
    fn calls_c_with_integer_args() {
        let (rax, _) = unsafe { call_native_ffi(c_add as usize, &[2, 3], &[0, 0]) };
        assert_eq!(rax as i64, 5);
    }

    #[test]
    fn calls_c_with_mixed_float_args() {
        // 浮点参数必须进 xmm —— 这条是 GLFW 的 cursorPos 回调（两个 jdouble）的前提
        let (_, xmm) = unsafe {
            call_native_ffi(
                c_mix as usize,
                &[1, (1.5f32).to_bits() as u64, (0.25f64).to_bits(), 10],
                &[0, 1, 2, 0],
            )
        };
        let r = f64::from_bits(xmm);
        assert!((r - 12.75).abs() < 1e-9, "得到 {r}");
    }

    #[test]
    fn calls_c_with_stack_args() {
        let (rax, _) = unsafe { call_native_ffi(c_many as usize, &[1, 2, 3, 4, 5, 6, 7], &[0; 7]) };
        assert_eq!(rax as i64, 28);
    }
}
