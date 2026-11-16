//! JIT 运行时辅助函数（被生成的机器码直接调用）
//!
//! 堆对象统一布局：**偏移 4 处是 u32 类型标签**，字段/元素从偏移 8 开始。
//! - 对象：`[0..4] 未用`、`[4..8] class tag`、`[8..] 字段（每字段 8 字节）`
//! - 数组：`[0..4] 长度`、`[4..8] class tag`、`[8..] 元素（int 4 字节 / long·double·引用 8 字节）`
//! - 字符串：同数组，`[8..]` 是 UTF-8 字节

use std::sync::Mutex;

const CAP: usize = 256 << 20;
struct State {
    base: *mut u8,
    used: usize,
}
unsafe impl Send for State {}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// 类名 → 类型标签（0 = java/lang/Object，1 = java/lang/String，其余从 2 起）
static CLASS_TAGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 正在传播的异常：非 0 表示 pending，值是异常对象指针。
/// JIT 出来的代码在每个可能抛异常的调用点后检查它，命中就查本方法的异常表。
static EXC_PENDING: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static EXC_OBJ: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
/// 异常对象指针 → message 字符串指针（java.* 的构造器我们没有字节码，只能这样记）
static EXC_MSGS: Mutex<Vec<(i64, i64)>> = Mutex::new(Vec::new());

pub extern "C" fn exc_throw(obj: i64) {
    EXC_OBJ.store(obj, std::sync::atomic::Ordering::Relaxed);
    EXC_PENDING.store(1, std::sync::atomic::Ordering::Relaxed);
}

pub extern "C" fn exc_pending() -> i32 {
    EXC_PENDING.load(std::sync::atomic::Ordering::Relaxed)
}

pub extern "C" fn exc_get() -> i64 {
    EXC_OBJ.load(std::sync::atomic::Ordering::Relaxed)
}

pub extern "C" fn exc_clear() {
    EXC_PENDING.store(0, std::sync::atomic::Ordering::Relaxed);
}

/// 记下异常对象的消息（`new RuntimeException("boom")` 那一步）
pub extern "C" fn exc_set_msg(obj: i64, msg: i64) {
    let mut t = EXC_MSGS.lock().unwrap();
    t.push((obj, msg));
}

fn exc_msg(obj: i64) -> Option<i64> {
    EXC_MSGS.lock().unwrap().iter().rev().find(|(o, _)| *o == obj).map(|(_, m)| *m)
}

/// 类名的**内部形式**（class_name_of 返回的是带点的那种，继承链比较要用斜杠形式）
pub fn class_internal_of(tag: u32) -> String {
    match tag {
        0 => "java/lang/Object".to_string(),
        1 => "java/lang/String".to_string(),
        t => {
            let names = CLASS_TAGS.lock().unwrap();
            match names.get((t - 2) as usize) {
                Some(n) => n.clone(),
                None => format!("<type#{t}>"),
            }
        }
    }
}

/// catch 子句的类型匹配：同类型或子类（java.* 的继承链写死在 vm 那边，共用一份）
pub extern "C" fn catch_match(obj: i64, tag: u32) -> i32 {
    if obj == 0 {
        return 0;
    }
    let actual = unsafe { std::ptr::read_unaligned((obj as *const u8).add(4) as *const u32) };
    if actual == tag || tag == 0 {
        return 1;
    }
    let a = class_internal_of(actual);
    let t = class_internal_of(tag);
    let mut cur = a;
    for _ in 0..32 {
        match crate::vm::builtin_super(&cur) {
            Some(s) => {
                if s == t {
                    return 1;
                }
                cur = s.to_string();
            }
            None => return 0,
        }
    }
    0
}

/// 未捕获异常：按 JVM 的格式打到 stderr 并结束进程（退出码 1）
pub fn print_uncaught(obj: i64) -> ! {
    let tag = unsafe { std::ptr::read_unaligned((obj as *const u8).add(4) as *const u32) };
    let class = class_name_of(tag);
    match exc_msg(obj) {
        Some(m) if m != 0 => {
            // message 就是字符串堆里那块的指针：`[0..4] 长度`、`[4..8] 标签`、`[8..]` UTF-8
            let len = unsafe { std::ptr::read_unaligned(m as *const u32) } as usize;
            let bytes = unsafe { std::slice::from_raw_parts((m as *const u8).add(8), len) };
            eprintln!("Exception in thread \"main\" {class}: {}", String::from_utf8_lossy(bytes));
        }
        _ => eprintln!("Exception in thread \"main\" {class}"),
    }
    std::process::exit(1);
}

/// 登记/查询一个类的类型标签。编译期调用，把结果当常量嵌进机器码。
pub fn class_tag(name: &str) -> u32 {
    match name {
        "java/lang/Object" => return 0,
        "java/lang/String" => return 1,
        _ => {}
    }
    let mut t = CLASS_TAGS.lock().unwrap();
    if let Some(i) = t.iter().position(|s| s == name) {
        return i as u32 + 2;
    }
    t.push(name.to_string());
    t.len() as u32 + 1
}

/// 类型标签 → 类名（打印非字符串对象时用）
pub fn class_name_of(tag: u32) -> String {
    match tag {
        0 => "java.lang.Object".to_string(),
        1 => "java.lang.String".to_string(),
        t => {
            let names = CLASS_TAGS.lock().unwrap();
            match names.get((t - 2) as usize) {
                Some(n) => n.replace('/', "."),
                None => format!("<type#{t}>"),
            }
        }
    }
}

fn tag_of(name: &str) -> u32 {    match name {
        "java/lang/Object" => 0,
        "java/lang/String" => 1,
        other => {
            let mut t = CLASS_TAGS.lock().unwrap();
            match t.iter().position(|s| s == other) {
                Some(i) => i as u32 + 2,
                None => {
                    t.push(other.to_string());
                    t.len() as u32 + 1
                }
            }
        }
    }
}

/// 分配一个对象：头部 8 字节（[4..8] 是类型标签），后面 fields 个 8 字节字段槽
pub extern "C" fn new_object(fields: u32, tag: u32) -> *mut u8 {
    let p = unsafe { alloc_zeroed(8 + fields as usize * 8) };
    unsafe { std::ptr::write_unaligned(p.add(4) as *mut u32, tag) };
    p
}

/// 分配一个数组：`[0..4]` 长度，`[4..8]` 类型标签，`[8..]` len 个 elem_size 字节元素
pub extern "C" fn new_array(len: u32, elem_size: u32, tag: u32) -> *mut u8 {
    let p = unsafe { alloc_zeroed(8 + len as usize * elem_size as usize) };
    unsafe {
        std::ptr::write_unaligned(p as *mut u32, len);
        std::ptr::write_unaligned(p.add(4) as *mut u32, tag);
    }
    p
}

/// `instanceof`：null 对任何类型都是 false；标签 0（java/lang/Object）对任何非 null 都是 true
pub extern "C" fn instanceof(obj: i64, tag: u32) -> i32 {
    if obj == 0 {
        return 0;
    }
    if tag == 0 {
        return 1;
    }
    let actual = unsafe { std::ptr::read_unaligned((obj as *const u8).add(4) as *const u32) };
    i32::from(actual == tag)
}

/// `checkcast`：类型不符时按 ClassCastException 退出（JIT 还没有异常机制）
pub extern "C" fn checkcast(obj: i64, tag: u32) -> i64 {
    if obj != 0 && tag != 0 {
        let actual = unsafe { std::ptr::read_unaligned((obj as *const u8).add(4) as *const u32) };
        if actual != tag {
            eprintln!(
                "ClassCastException: 类型标签 {actual} 的对象不能转换成标签 {tag}（checkcast 失败）"
            );
            std::process::exit(1);
        }
    }
    obj
}

/// 打印一个 int（对应 System.out.println(int)）
pub extern "C" fn println_int(v: i32) {
    println!("{v}");
}

/// 打印一个 boolean（对应 System.out.println(boolean)）
pub extern "C" fn println_bool(v: i32) {
    println!("{}", if v != 0 { "true" } else { "false" });
}

/// 打印一个 long（对应 System.out.println(long)）
pub extern "C" fn println_long(v: i64) {
    println!("{v}");
}

/// 打印一个 double（对应 System.out.println(double)，格式与 Java 一致）
pub extern "C" fn println_double(v: f64) {
    println!("{}", crate::java_double_to_string(v));
}

/// double 取余（drem 没有对应的 cranelift 指令，走 CRT 的 fmod）
pub extern "C" fn drem(a: f64, b: f64) -> f64 {
    unsafe extern "C" {
        fn fmod(a: f64, b: f64) -> f64;
    }
    unsafe { fmod(a, b) }
}

/// 打印一段 UTF-8 文本（指针 + 长度）
pub extern "C" fn println_str(ptr: *const u8, len: u32) {
    let s = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
    println!("{}", String::from_utf8_lossy(s));
}

unsafe fn alloc_zeroed(size: usize) -> *mut u8 {
    let mut g = STATE.lock().unwrap();
    if g.is_none() {
        let layout = std::alloc::Layout::from_size_align(CAP, 8).unwrap();
        let p = std::alloc::alloc_zeroed(layout);
        *g = Some(State { base: p, used: 0 });
    }
    let s = g.as_mut().unwrap();
    let off = s.used;
    s.used += (size + 7) & !7;
    assert!(s.used <= CAP, "JIT heap full");
    let p = s.base.add(off);
    std::ptr::write_bytes(p, 0, size);
    p
}

/// 让 `strtab` 也能用同一个分配器/标签表
pub(super) fn string_tag() -> u32 {
    tag_of("java/lang/String")
}
