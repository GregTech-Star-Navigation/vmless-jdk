//! 动态库装载与 native 方法绑定 —— JNI 的**流程**那一半。
//!
//! ```text
//! System.loadLibrary("foo") / System.load(path)
//!      ↓  LoadLibraryW / dlopen
//! JNI_OnLoad(JavaVM*, void*)  ← 库自己一次性注册（LWJGL 走这条）
//!      ↓  RegisterNatives(clazz, methods[], n)
//! (类, 名, 描述符) → C 函数指针         ← 本文件的 TARGETS
//!      ↓  exec 遇到 ACC_NATIVE 先查 native_reg，未命中再查这里
//! 跳板 trampoline：把 Frame 里的槽位拍成 C 的参数、调用、再拍回返回值
//! ```
//!
//! **短名绑定**（`Java_com_example_Foo_bar`）是第二条路：没被 `RegisterNatives` 注册的
//! native 方法，第一次被调用时按 JNI 规范的改名规则去每个已装载的库里 `GetProcAddress`。
//!
//! ## 为什么跳板要"先 push 再调用"
//!
//! JNI 的每个函数第一个参数是 `JNIEnv*`，而 `JNIEnv` 本身不带状态（它只有一个函数表
//! 指针）。所有状态（当前线程的 `Vm`、未决异常、字符串/数组缓冲区）都在
//! `jni.rs` 的线程本地栈里 —— 跳板在调用前后 push/pop 一次，native 代码里就能
//! 通过 `env` 找回 VM。

#![allow(unsafe_op_in_unsafe_fn)]

use std::collections::HashMap;
use std::ffi::{c_void, CString};
use std::sync::{Mutex, OnceLock};

use super::jni;
use super::NativeFn;
use crate::vm::frame::Frame;
use crate::vm::slot::Slot;
use crate::vm::{property, Vm, VmError};

// ============================================================ 平台：装载动态库

#[cfg(windows)]
mod sys {
    use std::ffi::{c_void, CStr};
    #[link(name = "kernel32")]
    extern "system" {
        pub fn LoadLibraryW(name: *const u16) -> *mut c_void;
        pub fn GetProcAddress(h: *mut c_void, name: *const u8) -> *mut c_void;
        pub fn FreeLibrary(h: *mut c_void) -> i32;
    }
    pub fn load(path: &str) -> *mut c_void {
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe { LoadLibraryW(wide.as_ptr()) }
    }
    pub fn sym(handle: *mut c_void, name: &CStr) -> *mut c_void {
        unsafe { GetProcAddress(handle, name.as_ptr() as *const u8) }
    }
    pub fn unload(handle: *mut c_void) {
        unsafe {
            FreeLibrary(handle);
        }
    }
    pub fn lib_name(base: &str) -> String {
        if base.ends_with(".dll") {
            base.to_string()
        } else {
            format!("{base}.dll")
        }
    }
}

#[cfg(not(windows))]
mod sys {
    use std::ffi::{c_void, CStr};
    #[link(name = "dl")]
    extern "C" {
        fn dlopen(path: *const std::os::raw::c_char, flag: i32) -> *mut c_void;
        fn dlsym(h: *mut c_void, name: *const std::os::raw::c_char) -> *mut c_void;
        fn dlclose(h: *mut c_void) -> i32;
        fn dlerror() -> *const std::os::raw::c_char;
    }
    const RTLD_NOW: i32 = 2;
    pub fn load(path: &str) -> *mut c_void {
        let c = std::ffi::CString::new(path).unwrap_or_default();
        unsafe { dlopen(c.as_ptr(), RTLD_NOW) }
    }
    pub fn sym(handle: *mut c_void, name: &CStr) -> *mut c_void {
        unsafe { dlsym(handle, name.as_ptr()) }
    }
    pub fn unload(handle: *mut c_void) {
        unsafe {
            dlclose(handle);
        }
    }
    pub fn lib_name(base: &str) -> String {
        if base.ends_with(".so") || base.ends_with(".dylib") {
            base.to_string()
        } else if cfg!(target_os = "macos") {
            format!("lib{base}.dylib")
        } else {
            format!("lib{base}.so")
        }
    }
}

// ============================================================ 已装载的库

struct Lib {
    path: String,
    handle: *mut c_void,
    /// `JNI_OnLoad` 返回的版本（0 = 库没导出它）
    version: i32,
}

unsafe impl Send for Lib {}

static LIBS: Mutex<Vec<Lib>> = Mutex::new(Vec::new());

pub(crate) fn loaded_libraries() -> Vec<String> {
    LIBS.lock().map(|g| g.iter().map(|l| l.path.clone()).collect()).unwrap_or_default()
}

// ============================================================ 绑定表

/// 一个已绑定的 native 目标
pub(crate) struct Target {
    pub(crate) class: String,
    pub(crate) name: String,
    pub(crate) desc: String,
    pub(crate) is_static: bool,
    pub(crate) fp: usize,
}

static TARGETS: OnceLock<Mutex<HashMap<String, &'static Target>>> = OnceLock::new();
/// 短名解析失败过的键（库不会中途多出符号，失败结果可以永久缓存）
static MISSES: OnceLock<Mutex<HashMap<String, ()>>> = OnceLock::new();

fn key(class: &str, name: &str, desc: &str) -> String {
    format!("{class}\u{1}{name}\u{1}{desc}")
}

fn targets() -> &'static Mutex<HashMap<String, &'static Target>> {
    TARGETS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn misses() -> &'static Mutex<HashMap<String, ()>> {
    MISSES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn insert_target(t: Target) -> &'static Target {
    let k = key(&t.class, &t.name, &t.desc);
    let leaked: &'static Target = Box::leak(Box::new(t));
    targets().lock().unwrap_or_else(|e| e.into_inner()).insert(k, leaked);
    leaked
}

/// `RegisterNatives`：把 `{name, sig, fnPtr}` 装进绑定表。返回成功条数。
pub(crate) fn register_natives(class: &str, list: &[(String, String, usize)]) -> usize {
    let mut n = 0;
    for (name, sig, fp) in list {
        let is_static = with_store_flags(class, name, sig);
        insert_target(Target {
            class: class.to_string(),
            name: name.clone(),
            desc: sig.clone(),
            is_static,
            fp: *fp,
        });
        n += 1;
    }
    n
}

pub(crate) fn unregister_natives(class: &str) {
    let mut g = targets().lock().unwrap_or_else(|e| e.into_inner());
    g.retain(|k, _| !k.starts_with(&format!("{class}\u{1}")));
}

/// 这个方法是不是 static（native 方法的第一个参数是 `jclass` 还是 `jobject` 由它决定）
fn with_store_flags(class: &str, name: &str, desc: &str) -> bool {
    const ACC_STATIC: u16 = 0x0008;
    unsafe {
        let Some(p) = vm_of_thread() else { return false };
        let vm = &*p;
        let Some((ci, mi)) = vm.store.resolve_method(class, name, desc) else { return false };
        vm.store.classes[ci].methods[mi].access_flags & ACC_STATIC != 0
    }
}

/// 当前线程的 VM（只在 native 调用期间有效）
unsafe fn vm_of_thread() -> Option<*mut Vm> {
    jni::current_vm()
}

/// `(类, 名, 描述符)` → 已绑定的目标（`RegisterNatives` 或短名绑定过）
pub(crate) fn bound_target(class: &str, name: &str, desc: &str) -> Option<&'static Target> {
    targets()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key(class, name, desc))
        .copied()
}

/// `native_reg::lookup` 的最后一步：绑定表 → 短名扫描。
///
/// 命中时返回跳板函数；目标指针暂存在线程本地，紧接着由跳板取走
/// （`exec` 查完表之后立刻调用，中间没有别的查表动作）。
pub(crate) fn lookup(class: &str, name: &str, desc: &str) -> Option<NativeFn> {
    let t = match bound_target(class, name, desc) {
        Some(t) => t,
        None => resolve_short_name(class, name, desc)?,
    };
    pending_push(t);
    Some(trampoline as NativeFn)
}

/// 短名绑定：`Java_<类名>_<方法名>`（重载时还会试 `__<签名>` 后缀形态）
fn resolve_short_name(class: &str, name: &str, desc: &str) -> Option<&'static Target> {
    let k = key(class, name, desc);
    {
        let g = misses().lock().unwrap_or_else(|e| e.into_inner());
        if g.contains_key(&k) {
            return None;
        }
    }
    let base = format!("Java_{}_{}", mangle(class), mangle(name));
    let with_sig = format!("{base}__{}", mangle_sig(desc));
    let libs = LIBS.lock().unwrap_or_else(|e| e.into_inner());
    let mut found: Option<usize> = None;
    for l in libs.iter() {
        for cand in [with_sig.as_str(), base.as_str()] {
            let Ok(c) = CString::new(cand) else { continue };
            let p = sys::sym(l.handle, &c);
            if !p.is_null() {
                found = Some(p as usize);
                break;
            }
        }
        if found.is_some() {
            break;
        }
    }
    drop(libs);
    match found {
        Some(fp) => {
            let is_static = with_store_flags(class, name, desc);
            Some(insert_target(Target {
                class: class.to_string(),
                name: name.to_string(),
                desc: desc.to_string(),
                is_static,
                fp,
            }))
        }
        None => {
            misses().lock().unwrap_or_else(|e| e.into_inner()).insert(k, ());
            None
        }
    }
}

/// JNI 规范的符号名转义：`/`·`.`→`_`，`_`→`_1`，`;`→`_2`，`[`→`_3`，非 ASCII → `_0xxxx`
fn mangle(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '/' | '.' => out.push('_'),
            '_' => out.push_str("_1"),
            ';' => out.push_str("_2"),
            '[' => out.push_str("_3"),
            c if c.is_ascii_alphanumeric() => out.push(c),
            c => out.push_str(&format!("_0{:04x}", c as u32)),
        }
    }
    out
}

/// 描述符的重载后缀：`(ILjava/lang/String;)V` → `ILjava_lang_String_2V`
fn mangle_sig(desc: &str) -> String {
    let mut out = String::with_capacity(desc.len());
    for c in desc.chars() {
        // HotSpot 的 `pure_jni_name` 把签名里的括号整个丢掉：`(I)V` → `IV`
        if c == '(' || c == ')' {
            continue;
        }
        match c {
            '/' | '.' => out.push('_'),
            '_' => out.push_str("_1"),
            ';' => out.push_str("_2"),
            '[' => out.push_str("_3"),
            c if c.is_ascii_alphanumeric() => out.push(c),
            c => out.push_str(&format!("_0{:04x}", c as u32)),
        }
    }
    out
}

// ============================================================ 线程本地的"待调用目标"

thread_local! {
    static PENDING: std::cell::RefCell<Vec<&'static Target>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn pending_push(t: &'static Target) {
    PENDING.with(|p| p.borrow_mut().push(t));
}

fn pending_pop() -> Option<&'static Target> {
    PENDING.with(|p| p.borrow_mut().pop())
}

/// 跳板：`exec` 把 native 方法交给它，它把控制权交给 C 函数指针
pub(crate) fn trampoline(vm: &mut Vm, frame: &mut Frame) -> Result<Slot, VmError> {
    let Some(t) = pending_pop() else {
        return Err(VmError { msg: "JNI 跳板没有待调用的目标（内部错误）".into(), exc: None });
    };
    unsafe { invoke_native(vm, frame, t) }
}

/// 把一帧的参数拍成 C 的参数、调用、再按描述符把返回值拍回槽位
unsafe fn invoke_native(vm: &mut Vm, frame: &mut Frame, t: &'static Target) -> Result<Slot, VmError> {
    let params = jni::split_params(&t.desc);
    let mut args: Vec<u64> = Vec::with_capacity(params.len() + 1);
    let mut kinds: Vec<u8> = Vec::with_capacity(params.len() + 1);
    // 第一个参数：实例方法是 jobject(this)，静态方法是 jclass
    let vmp: *mut Vm = vm;
    // 静态还是实例，**在调用时**从类表判定：绑定时（require_native 里）还没有 native
    // 上下文，resolve_method 拿不到 flags，只能猜 —— 猜错的话静态 native 收到的
    // jclass 会是 null（回调到静态方法时会静默失效）。
    let is_static = {
        const ACC_STATIC: u16 = 0x0008;
        let vmr: &Vm = &*vmp;
        match vmr.store.method_in_class(&t.class, &t.name, &t.desc) {
            Some((ci, mi)) => vmr.store.classes[ci].methods[mi].access_flags & ACC_STATIC != 0,
            None => t.is_static,
        }
    };
    // JNI 的第一个参数永远是 JNIEnv*（Win64 上就是 rcx），native 方法本体从第二个开始
    let env = jni::env_ptr();
    args.push(env as usize as u64);
    kinds.push(0);
    let first = if is_static {
        (*vmp).class_obj(&t.class) as i64 as u64
    } else {
        frame.this() as i64 as u64
    };
    args.push(first);
    kinds.push(0);
    for (i, ty) in params.iter().enumerate() {
        let v = frame.arg(i);
        let (k, bits) = match ty.as_bytes().first().copied() {
            Some(b'F') => (1u8, v.as_f32().to_bits() as u64),
            Some(b'D') => (2u8, v.as_f64().to_bits()),
            Some(b'J') => (0u8, v.as_i64() as u64),
            Some(b'L') | Some(b'[') => (0u8, v.as_i32() as i64 as u64),
            _ => (0u8, v.as_i32() as i64 as u64),
        };
        args.push(bits);
        kinds.push(k);
    }
    jni::ctx_push(vmp);
    let (rax, xmm) = jni::call_native_ffi(t.fp, &args, &kinds);
    let pending = jni::ctx_pop();
    if pending != 0 {
        return Err(crate::vm::exception::thrown(pending));
    }
    Ok(match jni::ret_type(&t.desc).as_str() {
        "V" => Slot::I(0),
        "J" => Slot::L(rax as i64),
        "F" => Slot::Fl(f32::from_bits(xmm as u32)),
        "D" => Slot::F(f64::from_bits(xmm)),
        "L" | "[" => Slot::I(rax as i32),
        _ => Slot::I(rax as i32),
    })
}

// ============================================================ 装载

/// `JNI_OnLoad` / `JNI_OnUnload` 的函数指针类型
type OnLoad = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;
type OnUnload = unsafe extern "C" fn(*mut c_void, *mut c_void);

/// 把 `foo` 变成平台的文件名（Windows 是 `foo.dll`，Linux `libfoo.so`，macOS `libfoo.dylib`）
pub(crate) fn map_library_name(name: &str) -> String {
    sys::lib_name(name)
}

fn exists(path: &str) -> bool {
    std::path::Path::new(path).is_file()
}

/// 按 `java.library.path` 找库文件（找不到就返回原名，让系统 loader 自己再试一次）
fn search_path(name: &str) -> String {
    if exists(name) {
        return name.to_string();
    }
    let mut dirs: Vec<String> = Vec::new();
    if let Some(p) = property("java.library.path") {
        dirs.extend(p.split(if cfg!(windows) { ';' } else { ':' }).map(|s| s.to_string()));
    }
    // 兜底：当前目录、可执行文件所在目录
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd.to_string_lossy().into_owned());
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            dirs.push(d.to_string_lossy().into_owned());
        }
    }
    for d in dirs {
        if d.is_empty() {
            continue;
        }
        let p = std::path::Path::new(&d).join(name);
        let s = p.to_string_lossy().into_owned();
        if exists(&s) {
            return s;
        }
    }
    name.to_string()
}

/// `System.loadLibrary(name)` / `System.load(path)` 的本体。
///
/// `absolute = true` 表示参数已经是完整路径（`System.load`）。
pub(crate) fn load_library(vm: &mut Vm, name: &str, absolute: bool) -> Result<(), VmError> {
    let mapped = if absolute { name.to_string() } else { map_library_name(name) };
    let path = if absolute { mapped } else { search_path(&mapped) };
    {
        let g = LIBS.lock().unwrap_or_else(|e| e.into_inner());
        if g.iter().any(|l| l.path == path) {
            return Ok(()); // 已经装过了：JVM 的行为也是静默成功
        }
    }
    let handle = sys::load(&path);
    if handle.is_null() {
        return Err(vm.java_exception(
            "java/lang/UnsatisfiedLinkError",
            format!("no {} in java.library.path", map_library_name(name)),
        ));
    }
    let mut version = 0;
    let c = CString::new("JNI_OnLoad").unwrap_or_default();
    let onload = sys::sym(handle, &c);
    if !onload.is_null() {
        // `JNI_OnLoad` 里会用 JNIEnv，所以必须有上下文
        let jvm = jni::java_vm_ptr();
        jni::ctx_push(vm as *mut Vm);
        let f: OnLoad = unsafe { std::mem::transmute(onload) };
        version = unsafe { f(jvm as *mut c_void, std::ptr::null_mut()) };
        let pending = jni::ctx_pop();
        if pending != 0 {
            sys::unload(handle);
            return Err(crate::vm::exception::thrown(pending));
        }
    }
    LIBS.lock().unwrap_or_else(|e| e.into_inner()).push(Lib { path, handle, version });
    Ok(())
}

/// 卸载一个库（`JNI_OnUnload` 会先被调）
pub(crate) fn unload_library(vm: &mut Vm, path: &str) {
    let mut g = LIBS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(i) = g.iter().position(|l| l.path == path) else { return };
    let l = g.remove(i);
    let c = CString::new("JNI_OnUnload").unwrap_or_default();
    let f = sys::sym(l.handle, &c);
    if !f.is_null() {
        let jvm = jni::java_vm_ptr();
        jni::ctx_push(vm as *mut Vm);
        let fun: OnUnload = unsafe { std::mem::transmute(f) };
        unsafe { fun(jvm as *mut c_void, std::ptr::null_mut()) };
        let _ = jni::ctx_pop();
    }
    targets().lock().unwrap_or_else(|e| e.into_inner()).retain(|_, t| {
        // 只保留还在别的库里解析得到的符号（简单起见：全清，库重装时会重新解析）
        let _ = t;
        false
    });
    sys::unload(l.handle);
}

// ============================================================ `java.lang.System` 的 native 实现

/// `static native void loadLibrary(String)` → `(Ljava/lang/String;)V`
pub(crate) fn system_load_library(vm: &mut Vm, frame: &mut Frame) -> Result<Slot, VmError> {
    let name = vm.text_of(frame.arg(0).as_i32());
    load_library(vm, &name, false)?;
    Ok(Slot::I(0))
}

/// `static native void load(String)` → `(Ljava/lang/String;)V`
pub(crate) fn system_load(vm: &mut Vm, frame: &mut Frame) -> Result<Slot, VmError> {
    let name = vm.text_of(frame.arg(0).as_i32());
    load_library(vm, &name, true)?;
    Ok(Slot::I(0))
}

/// JDK 的 `jdk.internal.loader.NativeLibraries.load(...)`（真 java.base 模式下走这条）
///
/// 签名：`(NativeLibraryImpl impl, String name, boolean isBuiltin, boolean throwExceptionIfFail)Z`
/// —— 成功时还要把 `impl.handle` 填上（真 VM 也是这么做的）。
pub(crate) fn native_libraries_load(vm: &mut Vm, frame: &mut Frame) -> Result<Slot, VmError> {
    let impl_obj = frame.arg(0).as_i32();
    let name = vm.text_of(frame.arg(1).as_i32());
    let throw_if_fail = frame.arg(3).as_i32() != 0;
    match load_library(vm, &name, false) {
        Ok(()) => {
            // impl.handle：给一个非 0 的假句柄（真 VM 里是 dlopen 的返回值）
            let idx = loaded_libraries().len() as i64;
            if let Some(slot) = vm.store.field_index("jdk/internal/loader/NativeLibraries$NativeLibraryImpl", "handle") {
                vm.sh.heap.set(impl_obj, slot, Slot::L(idx.max(1)));
            }
            Ok(Slot::I(1))
        }
        Err(e) => {
            if throw_if_fail {
                Err(e)
            } else {
                Ok(Slot::I(0))
            }
        }
    }
}

// ============================================================ 诊断

/// 已绑定的 native 条目（类.名:计数），给报告/自检用
pub(crate) fn bound_count() -> usize {
    targets().lock().map(|g| g.len()).unwrap_or(0)
}

/// 已绑定条目的列表
pub(crate) fn bound_entries() -> Vec<(String, String, String)> {
    let g = targets().lock().unwrap_or_else(|e| e.into_inner());
    let mut v: Vec<(String, String, String)> =
        g.values().map(|t| (t.class.clone(), t.name.clone(), t.desc.clone())).collect();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mangles_jni_symbol_names() {
        // JNI 规范里的例子：包名/类名里的 `/` 变 `_`，方法名里的 `_` 变 `_1`
        assert_eq!(mangle("java/lang/String"), "java_lang_String");
        assert_eq!(mangle("my_class"), "my_1class");
        assert_eq!(mangle_sig("(ILjava/lang/String;)V"), "ILjava_lang_String_2V");
        assert_eq!(mangle_sig("[I"), "_3I");
    }

    #[test]
    fn maps_library_names_per_platform() {
        let n = map_library_name("hello");
        if cfg!(windows) {
            assert_eq!(n, "hello.dll");
        } else if cfg!(target_os = "macos") {
            assert_eq!(n, "libhello.dylib");
        } else {
            assert_eq!(n, "libhello.so");
        }
        // 已经带扩展名的原样保留
        assert_eq!(map_library_name("x.dll"), "x.dll");
    }
}
