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
    if s == 0 {
        println!("null"); // Java 打 println((String) null) 也是 "null"
        return;
    }
    unsafe {
        let p = rt::base_ptr().wrapping_add(s as usize);
        let class_id = std::ptr::read_unaligned(p as *const u32);
        if class_id != rt::string_class_id() {
            println!("{}", rt::class_name_of(rt::tag_of_class(class_id)));
            return;
        }
        let len = std::ptr::read_unaligned(p.add(8) as *const u32) as usize;
        let bytes = std::slice::from_raw_parts(p.add(16), len);
        println!("{}", String::from_utf8_lossy(bytes));
    }
}
