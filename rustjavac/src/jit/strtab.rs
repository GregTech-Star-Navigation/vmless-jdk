//! 字符串常量表：JIT 编译期登记，运行期按序号取指针。

use std::sync::Mutex;

// 每份缓冲的布局与堆数组一致：`[0..4] 长度`、`[4..8] 类型标签`、`[8..]` UTF-8 字节
static TABLE: Mutex<Vec<Box<[u8]>>> = Mutex::new(Vec::new());

/// 登记一个字符串，返回序号
pub fn register(s: &str) -> u32 {
    let mut buf = Vec::with_capacity(8 + s.len());
    buf.extend_from_slice(&(s.len() as u32).to_ne_bytes());
    buf.extend_from_slice(&super::rt::string_tag().to_ne_bytes());
    buf.extend_from_slice(s.as_bytes());
    let mut t = TABLE.lock().unwrap();
    t.push(buf.into_boxed_slice());
    (t.len() - 1) as u32
}

/// 运行时辅助：返回指向「长度 + 标签 + 内容」缓冲的指针（缓冲常驻，指针稳定）
pub extern "C" fn str_const(idx: u32) -> *const u8 {
    let t = TABLE.lock().unwrap();
    match t.get(idx as usize) {
        Some(b) => b.as_ptr(),
        None => std::ptr::null(),
    }
}

/// 运行时辅助：打印一个字符串堆里的「长度 + 内容」；
/// 不是字符串（标签 ≠ 1）就打类型名，避免把对象字段当成字符串读
pub extern "C" fn println_str(s: *const u8) {
    if s.is_null() {
        println!("null"); // Java 打 println((String) null) 也是 "null"
        return;
    }
    unsafe {
        let mut lb = [0u8; 4];
        std::ptr::copy_nonoverlapping(s, lb.as_mut_ptr(), 4);
        let len = u32::from_ne_bytes(lb) as usize;
        let mut tb = [0u8; 4];
        std::ptr::copy_nonoverlapping(s.add(4), tb.as_mut_ptr(), 4);
        if u32::from_ne_bytes(tb) != super::rt::string_tag() {
            println!("{}", super::rt::class_name_of(std::ptr::read_unaligned(s.add(4) as *const u32)));
            return;
        }
        let bytes = std::slice::from_raw_parts(s.add(8), len);
        println!("{}", String::from_utf8_lossy(bytes));
    }
}
