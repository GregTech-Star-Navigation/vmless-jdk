//! 字符串常量表。
//!
//! 字符串常量不再是静态缓冲：它被分配成 librustjrt 堆上的 String 对象（布局与数组
//! 一致：`class_id / flags / 长度 / 填充 / UTF-8 字节`），并登记成**永久根**。
//! 这样引用处处都是同一种东西（堆内偏移），`putfield`/`aastore`/`println` 不必
//! 再区分「堆对象」和「静态缓冲」。

use std::sync::Mutex;

use super::rt;

/// 序号 → 永久根槽下标（根槽里存的是当前偏移，GC 搬过之后是新值）
static TABLE: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// 描述符串（multianewarray 用）→ 常驻缓冲，**只给 Rust 侧读**，不当 Java 对象用
static DESCS: Mutex<Vec<Box<[u8]>>> = Mutex::new(Vec::new());

/// 登记一个字符串常量，返回序号
pub fn register(s: &str) -> u32 {
    let root_idx = rt::alloc_string_const(s.as_bytes());
    let mut t = TABLE.lock().unwrap();
    t.push(root_idx);
    (t.len() - 1) as u32
}

/// 登记一个描述符串（`[[I` / `[[Ljava/lang/String;`），返回序号
pub fn register_desc(s: &str) -> u32 {
    let mut buf = Vec::with_capacity(8 + s.len());
    buf.extend_from_slice(&(s.len() as u32).to_ne_bytes());
    buf.extend_from_slice(&0u32.to_ne_bytes());
    buf.extend_from_slice(s.as_bytes());
    let mut t = DESCS.lock().unwrap();
    t.push(buf.into_boxed_slice());
    (t.len() - 1) as u32
}

/// 字符串常量编号 → 文本。
///
/// 读的是堆上的 String 对象（GC 搬过之后按根槽里**当前**的偏移读），
/// 供 `rt::indy_concat` 拼字面量用。
pub fn text(idx: u32) -> String {
    let root = { TABLE.lock().unwrap().get(idx as usize).copied() };
    let Some(r) = root else { return String::new() };
    let off = rt::root_value(r) as i64;
    if off == 0 {
        return String::new();
    }
    unsafe {
        let p = rt::base_ptr().wrapping_add(off as usize);
        let len = std::ptr::read_unaligned(p.add(8) as *const u32) as usize;
        String::from_utf8_lossy(std::slice::from_raw_parts(p.add(16), len)).into_owned()
    }
}

/// 运行时辅助：字符串常量当前在堆里的偏移（0 = 没有这个序号）
pub extern "C" fn str_const(idx: u32) -> i64 {
    let root = { TABLE.lock().unwrap().get(idx as usize).copied() };
    match root {
        Some(r) => rt::root_value(r) as i64,
        None => 0,
    }
}

/// 运行时辅助：描述符缓冲的稳定指针（缓冲常驻，指针不随 GC 变）
pub extern "C" fn desc_ptr(idx: u32) -> i64 {
    let t = DESCS.lock().unwrap();
    match t.get(idx as usize) {
        Some(b) => b.as_ptr() as i64,
        None => 0,
    }
}

/// 运行时辅助：打印一个字符串引用；
/// 不是字符串就打类型名，避免把对象的字段当成字符串读
pub extern "C" fn println_str(s: i64) {
    // 引用 → 文本：null → "null"，String → 内容，包装类 → 值的文本，
    // 其余 → `Klass@hash`（`println(Object)` 的语义）
    println!("{}", super::natives::obj_to_string(s));
}

/// 运行时辅助：两个字符串引用按**内容**比较（`String.equals(Object)`）。
///
/// 布局与 `println_str` 一致：class_id(4) / flags(4) / 长度(4) / 填充(4) / UTF-8 字节。
/// 任一侧不是 String（null / 对象 / 数组）→ 0，和 `String.equals` 的语义一致。
pub extern "C" fn str_equals(a: i64, b: i64) -> i32 {
    if a == 0 || b == 0 {
        return 0;
    }
    unsafe {
        let pa = rt::base_ptr().wrapping_add(a as usize);
        let pb = rt::base_ptr().wrapping_add(b as usize);
        let sid = rt::string_class_id();
        if std::ptr::read_unaligned(pa as *const u32) != sid {
            return 0;
        }
        if std::ptr::read_unaligned(pb as *const u32) != sid {
            return 0;
        }
        let la = std::ptr::read_unaligned(pa.add(8) as *const u32) as usize;
        let lb = std::ptr::read_unaligned(pb.add(8) as *const u32) as usize;
        if la != lb {
            return 0;
        }
        let ba = std::slice::from_raw_parts(pa.add(16), la);
        let bb = std::slice::from_raw_parts(pb.add(16), lb);
        if ba == bb {
            1
        } else {
            0
        }
    }
}
