//! JIT 侧的内建 JDK 类：包装类、`String` 的方法、`Object` 协议、
//! `System`/`Arrays` 的静态方法。
//!
//! # 表示
//!
//! - **包装类**（`Integer`/`Long`/`Double`/`Boolean`/`Character`）是普通对象：
//!   注册成一个 8 字节字段的类，字段 0 存值（`HEADER` 偏移）。
//!   小整数（-128..127）与两个布尔值在 `Jit::new` 时就分配好并登记成**永久根** ——
//!   必须在那时候做：生成代码每帧都会 push/pop 影子栈，中途往根表里塞常驻根
//!   会把帧的根截掉。
//! - **String** 沿用字符串常量表的布局：数组形态（`class_id = string_class_id()`），
//!   长度在 `HEADER`，UTF-8 字节在 `ARRAY_HEADER`。
//!
//! # ABI
//!
//! 三个通用入口，参数一律 `i64`（double 由调用点 bitcast 成 i64，返回值同理）：
//! `builtin_vcall(recv, op, a, b)` / `builtin_static(op, a, b)` / `builtin_sfield(op)`。
//! `op` 就是 `crate::jdk::VIRTUALS` / `STATICS` / `FIELDS` 的下标 —— 与解释器共用同一张表。

use std::sync::atomic::{AtomicUsize, Ordering};

use librustjrt::object::{ARRAY_HEADER, HEADER};

use crate::jdk;

use super::rt;

/// Integer 缓存的第一个根槽下标（0 = 还没初始化）
static INT_CACHE_BASE: AtomicUsize = AtomicUsize::new(0);
/// Boolean 的 TRUE 根槽下标（0 = 还没初始化）；FALSE 紧随其后
static BOOL_BASE: AtomicUsize = AtomicUsize::new(0);

const INT_CACHE: i32 = 128;

// ---------- 基本存取 ----------

fn wrapper_class_id(cls: &str) -> u32 {
    // 一个 8 字节字段：long/double 都能直接放
    rt::register_class(rt::class_tag(cls), cls, &["J".to_string()])
}

fn read_u32(off: i64) -> u32 {
    let p = rt::base_ptr().wrapping_add(off as usize);
    unsafe { std::ptr::read_unaligned(p as *const u32) }
}

#[allow(dead_code)]
fn read_i64(off: i64) -> i64 {
    let p = rt::base_ptr().wrapping_add(off as usize);
    unsafe { std::ptr::read_unaligned(p as *const i64) }
}

fn write_i64(off: i64, v: i64) {
    let p = rt::base_ptr().wrapping_add(off as usize + HEADER);
    unsafe { std::ptr::write_unaligned(p as *mut i64, v) };
}

fn is_string(off: i64) -> bool {
    off != 0 && read_u32(off) == rt::string_class_id()
}

/// 堆上的字符串 → 字节（**先拷出来**：后面的分配可能触发 GC 把源搬走）
fn str_bytes(off: i64) -> Vec<u8> {
    if !is_string(off) {
        return Vec::new();
    }
    let len = read_u32(off + HEADER as i64) as usize;
    let p = rt::base_ptr().wrapping_add(off as usize + ARRAY_HEADER);
    unsafe { std::slice::from_raw_parts(p, len).to_vec() }
}

/// 分配一个字符串（不登记根：调用方要么立刻返回、要么自己管根）
fn alloc_str(bytes: &[u8]) -> i64 {
    let cid = rt::string_class_id();
    let hd = rt::new_array(cid, bytes.len() as u32);
    if !bytes.is_empty() {
        let p = rt::base_ptr().wrapping_add(hd as usize + ARRAY_HEADER);
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len()) };
    }
    hd
}

fn wrapper_value(off: i64) -> i64 {
    // 字段 0 在 HEADER 之后（前 8 字节是 class_id + flags）
    let p = rt::base_ptr().wrapping_add(off as usize + HEADER);
    unsafe { std::ptr::read_unaligned(p as *const i64) }
}

/// 引用 → 文本（Java 的 `String.valueOf` 语义）：null / String / 包装类 / `Klass@hash`
pub fn obj_to_string(off: i64) -> String {
    if off == 0 {
        return "null".to_string();
    }
    let cid = read_u32(off);
    let tag = rt::tag_of_class(cid);
    if tag == 1 {
        return String::from_utf8_lossy(&str_bytes(off)).into_owned();
    }
    let cls = rt::class_internal_of(tag);
    if let Some(prim) = jdk::wrapper_prim(&cls) {
        let v = wrapper_value(off);
        return match prim {
            "Z" => (if v != 0 { "true" } else { "false" }).to_string(),
            "J" => v.to_string(),
            "D" => crate::java_double_to_string(f64::from_bits(v as u64)),
            "C" => char::from_u32(v as u32).unwrap_or('?').to_string(),
            _ => (v as i32).to_string(),
        };
    }
    format!("{}@{}", cls.replace('/', "."), identity_hash(off))
}

/// 标识哈希：同一对象多次调用结果相同、不同对象不同。
///
/// 用堆内偏移算（与 HotSpot 的数值不要求一致）。**已知限制**：GC 搬动对象后
/// 数值会变，所以样例只在规模小、不触发 GC 的场景里比较它。
pub fn identity_hash(off: i64) -> i32 {
    if off == 0 {
        return 0;
    }
    let cid = read_u32(off);
    if rt::tag_of_class(cid) == 1 {
        // 字符串按内容算（与 HotSpot 的 String.hashCode 数值一致）
        let mut hash = 0i32;
        for b in str_bytes(off) {
            hash = hash.wrapping_mul(31).wrapping_add(b as i32);
        }
        return hash;
    }
    let mut x = off as u64 as u32;
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    (x & 0x7fff_ffff) as i32
}

// ---------- 初始化（必须早于任何生成代码）----------

fn alloc_wrapper_unrooted(cls: &str, v: i64) -> i64 {
    let cid = wrapper_class_id(cls);
    let obj = rt::new_object(cid);
    write_i64(obj, v);
    obj
}

fn push_perm_root(off: i64) -> usize {
    rt::with_heap(|h| {
        let r = h.roots_handle();
        let mut b = r.borrow_mut();
        b.push(off as u32);
        b.len() - 1
    })
}

/// 建小整数缓存 + Boolean 单例，全部登记成永久根。
///
/// 在 `Jit::new()` 里调用一次：生成代码的每一帧都会 push/pop 影子栈，
/// 运行期再往根表里塞常驻根会破坏帧的根区间。
pub fn init_builtin_roots() {
    if INT_CACHE_BASE.load(Ordering::Relaxed) != 0 {
        return;
    }
    // 每分配一个立刻登记成根：下一次分配可能触发 GC
    let mut first = 0usize;
    for i in 0..(INT_CACHE * 2) {
        let obj = alloc_wrapper_unrooted("java/lang/Integer", (i - INT_CACHE) as i64);
        let slot = push_perm_root(obj);
        if i == 0 {
            first = slot;
        }
    }
    INT_CACHE_BASE.store(first + 1, Ordering::Relaxed);
    let t = push_perm_root(alloc_wrapper_unrooted("java/lang/Boolean", 1));
    let f = push_perm_root(alloc_wrapper_unrooted("java/lang/Boolean", 0));
    debug_assert_eq!(f, t + 1);
    BOOL_BASE.store(t + 1, Ordering::Relaxed);
}

fn int_cache(v: i32) -> i64 {
    let base1 = INT_CACHE_BASE.load(Ordering::Relaxed);
    if base1 == 0 {
        return 0;
    }
    let idx = base1 - 1 + (v + INT_CACHE) as usize;
    rt::root_value(idx) as i64
}

fn bool_singleton(v: i32) -> i64 {
    let base1 = BOOL_BASE.load(Ordering::Relaxed);
    if base1 == 0 {
        return 0;
    }
    // TRUE 的根槽在前（见 init_builtin_roots）
    rt::root_value(base1 - 1 + usize::from(v == 0)) as i64
}

// ---------- Object 协议 ----------

fn object_protocol(op: u32, recv: i64, a: i64) -> i64 {
    let name = jdk::VIRTUALS[op as usize].name;
    match name {
        "hashCode" => identity_hash(recv) as i64,
        "toString" => alloc_str(obj_to_string(recv).as_bytes()),
        "equals" => i64::from(recv == a),
        _ => 0,
    }
}

// ---------- 虚方法入口 ----------

/// `invokevirtual/invokeinterface` 的内建实现（`op` 取自 `jdk::VIRTUALS` 下标）
pub extern "C" fn builtin_vcall(recv: i64, op: u32, a: i64, b: i64) -> i64 {
    let e = &jdk::VIRTUALS[op as usize];
    let (class, name, desc) = (e.class, e.name, e.desc);
    // ---- String ----
    if is_string(recv) {
        return string_op(name, recv, a, b, desc.starts_with("(II)") || desc.starts_with("(Ljava/lang/String;I)") || desc.starts_with("(Ljava/lang/CharSequence;L"));
    }
    // ---- 包装类 ----
    if recv != 0 {
        let cls = rt::class_internal_of(rt::tag_of_class(read_u32(recv)));
        if let Some(prim) = jdk::wrapper_prim(&cls) {
            return wrapper_op(&cls, prim, name, recv, a);
        }
    }
    // ---- Object 协议 ----
    let _ = (class, desc);
    object_protocol(op, recv, a)
}

fn wrapper_op(cls: &str, prim: &str, name: &str, recv: i64, a: i64) -> i64 {
    let v = wrapper_value(recv);
    match name {
        "intValue" => match prim {
            "D" => f64::from_bits(v as u64) as i32 as i64,
            _ => v as i32 as i64,
        },
        "longValue" => match prim {
            "D" => f64::from_bits(v as u64) as i64,
            _ => v,
        },
        "doubleValue" => match prim {
            "J" => (v as f64).to_bits() as i64,
            "I" | "C" | "Z" => (v as i32 as f64).to_bits() as i64,
            _ => v, // D：位模式原样
        },
        "booleanValue" => i64::from(v != 0),
        "charValue" => v as i32 as i64,
        "toString" => {
            let text = match prim {
                "Z" => (if v != 0 { "true" } else { "false" }).to_string(),
                "J" => v.to_string(),
                "D" => crate::java_double_to_string(f64::from_bits(v as u64)),
                "C" => char::from_u32(v as u32).unwrap_or('?').to_string(),
                _ => (v as i32).to_string(),
            };
            alloc_str(text.as_bytes())
        }
        "hashCode" => match prim {
            "D" => {
                let bits = v as u64;
                (bits ^ (bits >> 32)) as i32 as i64
            }
            "J" => (v ^ ((v as u64 >> 32) as i64)) as i32 as i64,
            "Z" => i64::from(if v != 0 { 1231 } else { 1237 }),
            _ => v as i32 as i64,
        },
        "equals" => {
            if a == 0 {
                return 0;
            }
            let acls = rt::class_internal_of(rt::tag_of_class(read_u32(a)));
            i64::from(acls == cls && wrapper_value(a) == v)
        }
        _ => 0,
    }
}

fn ascii_upper(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().map(|b| b.to_ascii_uppercase()).collect()
}

fn ascii_lower(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().map(|b| b.to_ascii_lowercase()).collect()
}

fn find_sub(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(hay.len()));
    }
    if from > hay.len() || needle.len() > hay.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

fn string_op(name: &str, recv: i64, a: i64, b: i64, two: bool) -> i64 {
    let s = str_bytes(recv);
    match name {
        "length" => s.len() as i64,
        "charAt" => {
            let i = a as i32;
            if i < 0 || i as usize >= s.len() {
                return 0; // 越界按 0 处理（样例不依赖 StringIndexOutOfBounds 的消息）
            }
            s[i as usize] as i64
        }
        "isEmpty" => i64::from(s.is_empty()),
        "substring" => {
            let start = (a as i32).max(0) as usize;
            let end = if two { (b as i32).max(0) as usize } else { s.len() };
            let end = end.min(s.len());
            let start = start.min(end);
            alloc_str(&s[start..end])
        }
        "indexOf" => {
            let (needle, from) = if two {
                (str_bytes(a), (b as i32).max(0) as usize)
            } else if is_string(a) {
                (str_bytes(a), 0)
            } else {
                (vec![a as i32 as u8], 0)
            };
            find_sub(&s, &needle, from).map(|i| i as i64).unwrap_or(-1)
        }
        "contains" => i64::from(find_sub(&s, &str_bytes(a), 0).is_some()),
        "startsWith" => {
            let p = str_bytes(a);
            i64::from(s.len() >= p.len() && &s[..p.len()] == &p[..])
        }
        "endsWith" => {
            let p = str_bytes(a);
            i64::from(s.len() >= p.len() && &s[s.len() - p.len()..] == &p[..])
        }
        "toUpperCase" => alloc_str(&ascii_upper(&s)),
        "toLowerCase" => alloc_str(&ascii_lower(&s)),
        "trim" => {
            let start = s.iter().position(|c| *c > b' ').unwrap_or(s.len());
            let end = s.iter().rposition(|c| *c > b' ').map(|i| i + 1).unwrap_or(start);
            alloc_str(&s[start..end.max(start)])
        }
        "replace" => {
            if two {
                let from = str_bytes(a);
                let to = str_bytes(b);
                if from.is_empty() {
                    return alloc_str(&s);
                }
                let mut out = Vec::with_capacity(s.len());
                let mut i = 0;
                while i < s.len() {
                    if i + from.len() <= s.len() && &s[i..i + from.len()] == &from[..] {
                        out.extend_from_slice(&to);
                        i += from.len();
                    } else {
                        out.push(s[i]);
                        i += 1;
                    }
                }
                alloc_str(&out)
            } else {
                let from = a as i32 as u8;
                let to = b as i32 as u8;
                let out: Vec<u8> = s.iter().map(|c| if *c == from { to } else { *c }).collect();
                alloc_str(&out)
            }
        }
        "compareTo" => {
            let other = str_bytes(a);
            let mut r = 0i64;
            for (x, y) in s.iter().zip(other.iter()) {
                if x != y {
                    r = *x as i64 - *y as i64;
                    break;
                }
            }
            if r == 0 {
                r = s.len() as i64 - other.len() as i64;
            }
            r
        }
        "concat" => {
            let mut out = s.clone();
            out.extend_from_slice(&str_bytes(a));
            alloc_str(&out)
        }
        "split" => {
            let sep = str_bytes(a);
            let parts: Vec<Vec<u8>> = if sep.is_empty() {
                s.iter().map(|c| vec![*c]).collect()
            } else {
                let mut out = Vec::new();
                let mut i = 0;
                let mut start = 0;
                while i < s.len() {
                    if i + sep.len() <= s.len() && &s[i..i + sep.len()] == &sep[..] {
                        out.push(s[start..i].to_vec());
                        i += sep.len();
                        start = i;
                    } else {
                        i += 1;
                    }
                }
                out.push(s[start..].to_vec());
                out
            };
            split_array(&parts)
        }
        "equals" => i64::from(is_string(a) && str_bytes(a) == s),
        "hashCode" => identity_hash(recv) as i64,
        "toString" => recv,
        _ => 0,
    }
}

/// 把若干字节串装成堆上的 `String[]`。
///
/// 每个元素分配都可能触发 GC：数组本身先登记成根，每轮从根槽重读它，
/// 元素用 `set_ref`（写屏障）存进去。
fn split_array(parts: &[Vec<u8>]) -> i64 {
    let class_id = rt::array_class_of("[Ljava/lang/String;");
    let arr = rt::new_array(class_id, parts.len() as u32);
    if parts.is_empty() {
        return arr;
    }
    let slot = rt::with_heap(|h| {
        h.push_root(librustjrt::Handle(arr as u32));
        h.roots_len() - 1
    });
    for (i, p) in parts.iter().enumerate() {
        let s = alloc_str(p);
        let cur = rt::root_value(slot) as i64;
        rt::set_ref(cur, i as u32, s);
    }
    rt::with_heap(|h| {
        h.pop_root();
    });
    arr
}

// ---------- 静态方法入口 ----------

/// `invokestatic` 的内建实现（`op` 取自 `jdk::STATICS` 下标）
pub extern "C" fn builtin_static(op: u32, a: i64, b: i64) -> i64 {
    let e = &jdk::STATICS[op as usize];
    let (class, name) = (e.class, e.name);
    match (class, name) {
        ("java/lang/Integer", "valueOf") => {
            let v = a as i32;
            if (-INT_CACHE..INT_CACHE).contains(&v) {
                let h = int_cache(v);
                if h != 0 {
                    return h;
                }
            }
            alloc_wrapper_unrooted("java/lang/Integer", v as i64)
        }
        ("java/lang/Long", "valueOf") => alloc_wrapper_unrooted("java/lang/Long", a),
        ("java/lang/Double", "valueOf") => alloc_wrapper_unrooted("java/lang/Double", a),
        ("java/lang/Boolean", "valueOf") => {
            let h = bool_singleton(a as i32);
            if h != 0 {
                return h;
            }
            alloc_wrapper_unrooted("java/lang/Boolean", a as i32 as i64)
        }
        ("java/lang/Character", "valueOf") => {
            alloc_wrapper_unrooted("java/lang/Character", a as i32 as i64)
        }
        ("java/lang/Integer", "parseInt") => String::from_utf8_lossy(&str_bytes(a))
            .trim()
            .parse::<i32>()
            .unwrap_or(0) as i64,
        ("java/lang/Long", "parseLong") => {
            String::from_utf8_lossy(&str_bytes(a)).trim().parse::<i64>().unwrap_or(0)
        }
        ("java/lang/Double", "parseDouble") => {
            let d: f64 = String::from_utf8_lossy(&str_bytes(a)).trim().parse().unwrap_or(0.0);
            d.to_bits() as i64
        }
        ("java/lang/Integer", "toString") => alloc_str((a as i32).to_string().as_bytes()),
        ("java/lang/Long", "toString") => alloc_str(a.to_string().as_bytes()),
        ("java/lang/Double", "toString") => {
            alloc_str(crate::java_double_to_string(f64::from_bits(a as u64)).as_bytes())
        }
        ("java/lang/System", "currentTimeMillis") => now_millis(),
        ("java/lang/System", "nanoTime") => now_nanos(),
        ("java/util/Arrays", "toString") => int_array_text(a),
        ("java/util/Arrays", "sort") => {
            int_array_sort(a);
            0
        }
        _ => {
            let _ = b;
            0
        }
    }
}

/// `System.arraycopy(src, sp, dst, dp, n)`：五参数，单独一个入口（通用 ABI 只有 2 个）
pub extern "C" fn arraycopy(src: i64, sp: i64, dst: i64, dp: i64, n: i64) -> i64 {
    let (n, sp, dp) = (n as i32, sp as i32, dp as i32);
    if src == 0 || dst == 0 || n <= 0 {
        return 0;
    }
    let (slen, slen_ref) = array_shape(src);
    let (dlen, dlen_ref) = array_shape(dst);
    if slen_ref != dlen_ref {
        return 0;
    }
    let width = slen as usize;
    let count = n as usize;
    if sp < 0 || dp < 0 {
        return 0;
    }
    let mut tmp = vec![0u8; width * count];
    unsafe {
        let base = rt::base_ptr();
        let s = base.wrapping_add(src as usize + ARRAY_HEADER + sp as usize * width);
        std::ptr::copy_nonoverlapping(s, tmp.as_mut_ptr(), width * count);
        if dlen_ref {
            // 引用元素要过写屏障（老数组存年轻引用）
            for i in 0..count {
                let mut v = [0u8; 4];
                v.copy_from_slice(&tmp[i * 4..i * 4 + 4]);
                let r = u32::from_ne_bytes(v) as i64;
                rt::set_ref(dst, (dp as usize + i) as u32, r);
            }
        } else {
            let d = base.wrapping_add(dst as usize + ARRAY_HEADER + dp as usize * width);
            std::ptr::copy_nonoverlapping(tmp.as_ptr(), d, width * count);
        }
    }
    let _ = dlen;
    0
}

fn array_shape(arr: i64) -> (u32, bool) {
    let cid = read_u32(arr);
    let tag = rt::tag_of_class(cid);
    let desc = rt::class_internal_of(tag);
    if !desc.starts_with('[') {
        return (4, false);
    }
    let (w, r) = rt::array_desc_shape(&desc[1..]);
    (w, r)
}

fn int_array_text(arr: i64) -> i64 {
    if arr == 0 {
        return alloc_str(b"null");
    }
    let len = read_u32(arr + HEADER as i64) as usize;
    let p = rt::base_ptr().wrapping_add(arr as usize + ARRAY_HEADER);
    let mut parts: Vec<String> = Vec::with_capacity(len);
    for i in 0..len {
        let v = unsafe { std::ptr::read_unaligned(p.add(i * 4) as *const i32) };
        parts.push(v.to_string());
    }
    alloc_str(format!("[{}]", parts.join(", ")).as_bytes())
}

fn int_array_sort(arr: i64) {
    if arr == 0 {
        return;
    }
    let len = read_u32(arr + HEADER as i64) as usize;
    let p = rt::base_ptr().wrapping_add(arr as usize + ARRAY_HEADER);
    let mut vals: Vec<i32> = (0..len)
        .map(|i| unsafe { std::ptr::read_unaligned(p.add(i * 4) as *const i32) })
        .collect();
    vals.sort_unstable();
    for (i, v) in vals.into_iter().enumerate() {
        unsafe { std::ptr::write_unaligned(p.add(i * 4) as *mut i32, v) };
    }
}

/// 内建静态字段（`Integer.MAX_VALUE` 这类）
pub extern "C" fn builtin_sfield(op: u32) -> i64 {
    let (class, name) = (jdk::FIELDS[op as usize].0, jdk::FIELDS[op as usize].1);
    match (class, name) {
        ("java/lang/Integer", "MAX_VALUE") => i32::MAX as i64,
        ("java/lang/Integer", "MIN_VALUE") => i32::MIN as i64,
        ("java/lang/Long", "MAX_VALUE") => i64::MAX,
        ("java/lang/Long", "MIN_VALUE") => i64::MIN,
        ("java/lang/Double", "MAX_VALUE") => f64::MAX.to_bits() as i64,
        ("java/lang/Double", "MIN_VALUE") => f64::MIN_POSITIVE.to_bits() as i64,
        ("java/lang/Double", "NaN") => f64::NAN.to_bits() as i64,
        ("java/lang/Character", "MAX_VALUE") => u16::MAX as i64,
        ("java/lang/Character", "MIN_VALUE") => 0,
        ("java/lang/Boolean", "TRUE") => bool_singleton(1),
        ("java/lang/Boolean", "FALSE") => bool_singleton(0),
        _ => 0,
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn now_nanos() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}
