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
    // 追加 API 的类/原生虚方法：**必须**在这里登记 —— `translate` 遇到
    // `new java/util/ArrayList` 时会用 `field_descs` 拿布局，而 `register_class`
    // 是「先到先得」，晚登记就只能拿到 0 字段的类，字段根本存不进去。
    register_api();
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
    // ---- 追加 API：集合 / Throwable ----
    if let Some(r) = api_vcall(class, name, recv, a, b) {
        return r;
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
    if let Some(v) = api_static(class, name, e.desc, a, b) {
        return v;
    }
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
        ("java/lang/Double", "MIN_VALUE") => f64::from_bits(1).to_bits() as i64, // 最小 subnormal
        ("java/lang/Double", "NaN") => f64::NAN.to_bits() as i64,
        // float 的位模式放低 32 位（调用点按 F32 位风格还原）
        ("java/lang/Float", "MAX_VALUE") => f32::MAX.to_bits() as i64,
        ("java/lang/Float", "MIN_VALUE") => f32::from_bits(1).to_bits() as i64, // 最小 subnormal
        ("java/lang/Float", "NaN") => f32::NAN.to_bits() as i64,
        ("java/lang/Character", "MAX_VALUE") => u16::MAX as i64,
        ("java/lang/Character", "MIN_VALUE") => 0,
        ("java/lang/Boolean", "TRUE") => bool_singleton(1),
        ("java/lang/Boolean", "FALSE") => bool_singleton(0),
        _ => 0,
    }
}

// ============================================================================
// 追加的 JDK API：集合框架 / Throwable / String.format / Math / Integer / Character / Arrays
// ============================================================================
//
// # 集合的表示（**GC 安全**是这里的第一约束）
//
// JIT 的 GC 是搬动式的，Rust 侧存下来的偏移下一次 GC 就失效。所以集合状态
// **全部放在真正的 Java 堆对象里**，让 GC 自己追：
//
// - `ArrayList`/`List`/`Set`/`Collection`：`field0` = 后端 `Object[]`，`field1` = 元素个数
// - `HashMap`/`Map`：同上，后端数组存「键, 值」交替，`field1` = 条目数
// - `Map$Entry`：两个 `Object` 字段（键, 值）
// - 迭代器 `java/util/ArrayList$Itr`：`field0` = 后端数组，`field1` = 游标
//   （它的 `hasNext`/`next` 用 `register_vmethod` 挂成原生虚方法 —— 见 `register_api`）
//
// 需要跨分配持有的引用一律 push 到根区（`Roots`），每次读都重读根槽。

/// 集合对象的字段布局（注册进 LayoutTable，GC 据此追字段）
fn coll_class(name: &str) -> u32 {
    rt::register_class(
        rt::class_tag(name),
        name,
        &["Ljava/lang/Object;".to_string(), "I".to_string()],
    )
}

fn obj_arr_cid() -> u32 {
    rt::array_class_of("[Ljava/lang/Object;")
}

fn rd_ref(obj: i64, f: u32) -> i64 {
    rt::with_heap(|h| h.get_ref(librustjrt::Handle(obj as u32), f).0 as i64)
}

fn wr_ref(obj: i64, f: u32, v: i64) {
    rt::set_ref(obj, f, v);
}

fn rd_i32(obj: i64, f: u32) -> i32 {
    rt::with_heap(|h| h.get_i32(librustjrt::Handle(obj as u32), f))
}

fn wr_i32(obj: i64, f: u32, v: i32) {
    rt::with_heap(|h| h.set_i32(librustjrt::Handle(obj as u32), f, v));
}

fn arr_len(a: i64) -> u32 {
    read_u32(a + HEADER as i64)
}

fn arr_get(a: i64, i: u32) -> i64 {
    rt::with_heap(|h| h.get_ref(librustjrt::Handle(a as u32), i).0 as i64)
}

fn arr_set(a: i64, i: u32, v: i64) {
    rt::set_ref(a, i, v);
}

/// 根区守卫：`new` 时 push N 个根，`Drop` 时弹掉；期间一律用 `get(i)` 重读
/// （分配可能触发 GC 把对象搬走）。
struct Roots {
    base: usize,
    n: usize,
}

impl Roots {
    fn new(vals: &[i64]) -> Self {
        let base = rt::with_heap(|h| {
            for v in vals {
                h.push_root(librustjrt::Handle(*v as u32));
            }
            h.roots_len() - vals.len()
        });
        Roots { base, n: vals.len() }
    }
    fn get(&self, i: usize) -> i64 {
        rt::root_value(self.base + i) as i64
    }
    fn set(&self, i: usize, v: i64) {
        rt::with_heap(|h| {
            let r = h.roots_handle();
            r.borrow_mut()[self.base + i] = v as u32;
        });
    }
}

impl Drop for Roots {
    fn drop(&mut self) {
        rt::with_heap(|h| {
            for _ in 0..self.n {
                h.pop_root();
            }
        });
    }
}

/// 引用相等（Java 的 `equals`）：String 比内容、包装类比值、其余比偏移
fn obj_eq(a: i64, b: i64) -> bool {
    if a == b {
        return true;
    }
    if a == 0 || b == 0 {
        return false;
    }
    let ca = rt::class_internal_of(rt::tag_of_class(read_u32(a)));
    let cb = rt::class_internal_of(rt::tag_of_class(read_u32(b)));
    if ca != cb {
        return false;
    }
    if ca == "java/lang/String" {
        return str_bytes(a) == str_bytes(b);
    }
    if jdk::wrapper_prim(&ca).is_some() {
        return wrapper_value(a) == wrapper_value(b);
    }
    false
}

// ---------- ArrayList / List / Set / Collection ----------

fn list_add(recv: i64, x: i64) -> i64 {
    let r = Roots::new(&[recv, x, 0]);
    let mut arr = rd_ref(r.get(0), 0);
    let n = rd_i32(r.get(0), 1).max(0) as u32;
    if arr == 0 || n >= arr_len(arr) {
        let cap = if arr == 0 { 0 } else { arr_len(arr) };
        let newcap = if cap == 0 { 8 } else { cap * 2 };
        let na = rt::new_array(obj_arr_cid(), newcap);
        r.set(2, na);
        let na = r.get(2);
        let old = rd_ref(r.get(0), 0);
        for i in 0..n {
            let v = arr_get(old, i);
            arr_set(na, i, v);
        }
        wr_ref(r.get(0), 0, na);
        arr = na;
    }
    let n = rd_i32(r.get(0), 1).max(0) as u32;
    arr_set(arr, n, r.get(1));
    wr_i32(r.get(0), 1, (n + 1) as i32);
    1
}

fn list_get(recv: i64, i: i64) -> i64 {
    let n = rd_i32(recv, 1);
    if i < 0 || i >= n as i64 {
        return 0;
    }
    let arr = rd_ref(recv, 0);
    if arr == 0 {
        return 0;
    }
    arr_get(arr, i as u32)
}

fn list_items(recv: i64) -> Vec<i64> {
    let n = rd_i32(recv, 1).max(0) as u32;
    let arr = rd_ref(recv, 0);
    if arr == 0 || n == 0 {
        return Vec::new();
    }
    (0..n.min(arr_len(arr))).map(|i| arr_get(arr, i)).collect()
}

fn list_find(recv: i64, x: i64) -> i64 {
    list_items(recv).iter().position(|&e| obj_eq(e, x)).map(|p| p as i64).unwrap_or(-1)
}

fn list_remove(recv: i64, i: i64) -> i64 {
    let n = rd_i32(recv, 1).max(0) as u32;
    let arr = rd_ref(recv, 0);
    if arr == 0 || i < 0 || i >= n as i64 {
        return 0;
    }
    let i = i as u32;
    let old = arr_get(arr, i);
    for j in i..n - 1 {
        let v = arr_get(arr, j + 1);
        arr_set(arr, j, v);
    }
    arr_set(arr, n - 1, 0);
    wr_i32(recv, 1, (n - 1) as i32);
    old
}

fn list_clear(recv: i64) {
    let n = rd_i32(recv, 1).max(0) as u32;
    let arr = rd_ref(recv, 0);
    if arr != 0 {
        for i in 0..n.min(arr_len(arr)) {
            arr_set(arr, i, 0);
        }
    }
    wr_i32(recv, 1, 0);
}

fn list_to_array(recv: i64) -> i64 {
    let items = list_items(recv);
    let r = Roots::new(&items);
    let a = rt::new_array(obj_arr_cid(), items.len() as u32);
    let r2 = Roots::new(&[a]);
    for i in 0..items.len() {
        arr_set(r2.get(0), i as u32, r.get(i));
    }
    r2.get(0)
}

fn list_text(recv: i64) -> i64 {
    let items = list_items(recv);
    let parts: Vec<String> = items.iter().map(|&h| obj_to_string(h)).collect();
    alloc_str(format!("[{}]", parts.join(", ")).as_bytes())
}

/// 建一个迭代器对象：`(后端数组, 游标 0)`
/// 建一个迭代器对象：`(后端数组, 元素个数, 游标 0)`
///
/// **必须带元素个数**：后端数组是**容量**（会按 2 倍扩容），元素个数可能小于它，
/// 只按数组长度判 `hasNext` 会把容量里的空槽也迭代进来
/// （实测：40 个元素的表，容量 64 → for-each 数出 64）。
fn make_iter(arr: i64, size: i32) -> i64 {
    let cid = itr_class();
    let r = Roots::new(&[arr]);
    let obj = rt::new_object(cid);
    let r2 = Roots::new(&[obj]);
    wr_ref(r2.get(0), I_ARR, r.get(0));
    wr_i32(r2.get(0), I_SIZE, size);
    wr_i32(r2.get(0), I_CUR, 0);
    r2.get(0)
}

/// 迭代器的类：`field0` = 后端数组，`field1` = 元素个数，`field2` = 游标
fn itr_class() -> u32 {
    rt::register_class(
        rt::class_tag("java/util/ArrayList$Itr"),
        "java/util/ArrayList$Itr",
        &["Ljava/lang/Object;".to_string(), "I".to_string(), "I".to_string()],
    )
}

// ---------- HashMap / Map ----------

fn map_find(recv: i64, k: i64) -> Option<u32> {
    let n = rd_i32(recv, 1).max(0) as u32;
    let arr = rd_ref(recv, 0);
    if arr == 0 {
        return None;
    }
    for i in 0..n {
        if obj_eq(arr_get(arr, i * 2), k) {
            return Some(i);
        }
    }
    None
}

fn map_put(recv: i64, k: i64, v: i64) -> i64 {
    let r = Roots::new(&[recv, k, v, 0]);
    if let Some(i) = map_find(r.get(0), r.get(1)) {
        let arr = rd_ref(r.get(0), 0);
        let old = arr_get(arr, i * 2 + 1);
        arr_set(arr, i * 2 + 1, r.get(2));
        return old;
    }
    let n = rd_i32(r.get(0), 1).max(0) as u32;
    let arr0 = rd_ref(r.get(0), 0);
    let cap = if arr0 == 0 { 0 } else { arr_len(arr0) / 2 };
    let mut cur = arr0;
    if n >= cap {
        let newcap = if cap == 0 { 8 } else { cap * 2 };
        let na = rt::new_array(obj_arr_cid(), newcap * 2);
        r.set(3, na);
        let na = r.get(3);
        let old = rd_ref(r.get(0), 0);
        for i in 0..n * 2 {
            let e = arr_get(old, i);
            arr_set(na, i, e);
        }
        wr_ref(r.get(0), 0, na);
        cur = na;
    }
    arr_set(cur, n * 2, r.get(1));
    arr_set(cur, n * 2 + 1, r.get(2));
    wr_i32(r.get(0), 1, (n + 1) as i32);
    0
}

fn map_get(recv: i64, k: i64) -> i64 {
    match map_find(recv, k) {
        Some(i) => {
            let arr = rd_ref(recv, 0);
            arr_get(arr, i * 2 + 1)
        }
        None => 0,
    }
}

fn map_remove(recv: i64, k: i64) -> i64 {
    let Some(i) = map_find(recv, k) else { return 0 };
    let n = rd_i32(recv, 1).max(0) as u32;
    let arr = rd_ref(recv, 0);
    let old = arr_get(arr, i * 2 + 1);
    for j in i..n - 1 {
        let ka = arr_get(arr, (j + 1) * 2);
        let va = arr_get(arr, (j + 1) * 2 + 1);
        arr_set(arr, j * 2, ka);
        arr_set(arr, j * 2 + 1, va);
    }
    arr_set(arr, (n - 1) * 2, 0);
    arr_set(arr, (n - 1) * 2 + 1, 0);
    wr_i32(recv, 1, (n - 1) as i32);
    old
}

fn map_clear(recv: i64) {
    let n = rd_i32(recv, 1).max(0) as u32;
    let arr = rd_ref(recv, 0);
    if arr != 0 {
        for i in 0..(n * 2).min(arr_len(arr)) {
            arr_set(arr, i, 0);
        }
    }
    wr_i32(recv, 1, 0);
}

fn map_text(recv: i64) -> i64 {
    let n = rd_i32(recv, 1).max(0) as u32;
    let arr = rd_ref(recv, 0);
    let mut parts = Vec::with_capacity(n as usize);
    for i in 0..n {
        let k = if arr != 0 { arr_get(arr, i * 2) } else { 0 };
        let v = if arr != 0 { arr_get(arr, i * 2 + 1) } else { 0 };
        parts.push(format!("{}={}", obj_to_string(k), obj_to_string(v)));
    }
    alloc_str(format!("{{{}}}", parts.join(", ")).as_bytes())
}

/// `keySet()` / `values()` / `entrySet()`：物化成 List 视图。
///
/// 后端数组**一次分配到位**（长度 = 条目数），之后只往里面写引用 —— 数组挂在
/// 已根住的视图对象 `field0` 上，GC 全程可达，中间不留任何「裸偏移」的窗口。
fn map_view(recv: i64, kind: u8) -> i64 {
    let n = rd_i32(recv, 1).max(0) as u32;
    let vcls = if kind == 1 { "java/util/Collection" } else { "java/util/Set" };
    let vobj = rt::new_object(coll_class(vcls));
    let rv = Roots::new(&[vobj]);
    let back = rt::new_array(obj_arr_cid(), n.max(1));
    wr_ref(rv.get(0), F_ARR, back);
    wr_i32(rv.get(0), F_LEN, n as i32);
    for i in 0..n {
        let src = rd_ref(recv, F_ARR);
        if src == 0 {
            break;
        }
        let item = match kind {
            0 => arr_get(src, i * 2),
            1 => arr_get(src, i * 2 + 1),
            _ => {
                let e = rt::new_object(coll_class("java/util/Map$Entry"));
                let r2 = Roots::new(&[e]);
                wr_ref(r2.get(0), 0, arr_get(src, i * 2));
                wr_ref(r2.get(0), 1, arr_get(src, i * 2 + 1));
                r2.get(0)
            }
        };
        arr_set(rd_ref(rv.get(0), F_ARR), i, item);
    }
    rv.get(0)
}

fn entries_push(v: &mut Vec<i64>, x: i64) {
    v.push(x);
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

// ---------- 集合 / Throwable 的虚方法分派 ----------

/// 集合对象的字段下标：`field0` = 后端 `Object[]`，`field1` = 元素/条目数
const F_ARR: u32 = 0;
const F_LEN: u32 = 1;

/// 迭代器的三个字段（比集合多一个游标，见 `make_iter`）
const I_ARR: u32 = 0;
const I_SIZE: u32 = 1;
const I_CUR: u32 = 2;

fn is_throwable_class(c: &str) -> bool {
    matches!(
        c,
        "java/lang/Throwable"
            | "java/lang/Exception"
            | "java/lang/RuntimeException"
            | "java/lang/Error"
    )
}

/// 追加 API 的虚方法分派（`class` 是 `jdk::VIRTUALS` 里登记的类）
fn api_vcall(class: &str, name: &str, recv: i64, a: i64, b: i64) -> Option<i64> {
    if is_throwable_class(class) {
        return Some(throwable_call(name, recv));
    }
    if class.starts_with("java/util/") && class != "java/util/Arrays" {
        return Some(coll_call(class, name, recv, a, b));
    }
    None
}

fn coll_call(class: &str, name: &str, recv: i64, a: i64, b: i64) -> i64 {
    if class == "java/util/Map$Entry" {
        return match name {
            "getKey" => rd_ref(recv, 0),
            "getValue" => rd_ref(recv, 1),
            "toString" => {
                let k = obj_to_string(rd_ref(recv, 0));
                let v = obj_to_string(rd_ref(recv, 1));
                alloc_str(format!("{k}={v}").as_bytes())
            }
            _ => 0,
        };
    }
    if class.ends_with("HashMap") || class == "java/util/Map" {
        return map_call(name, recv, a, b);
    }
    list_call(name, recv, a, b)
}

fn list_call(name: &str, recv: i64, a: i64, b: i64) -> i64 {
    let _ = b;
    match name {
        "add" => list_add(recv, a),
        "get" => list_get(recv, a as i32 as i64),
        "size" => rd_i32(recv, F_LEN).max(0) as i64,
        "isEmpty" => i64::from(rd_i32(recv, F_LEN) <= 0),
        "remove" => list_remove(recv, a as i32 as i64),
        "contains" => i64::from(list_find(recv, a) >= 0),
        "indexOf" => list_find(recv, a),
        "clear" => {
            list_clear(recv);
            0
        }
        "toArray" => list_to_array(recv),
        "iterator" => {
            let arr = rd_ref(recv, F_ARR);
            let size = rd_i32(recv, F_LEN).max(0);
            make_iter(arr, size)
        }
        "toString" => list_text(recv),
        _ => 0,
    }
}

fn map_call(name: &str, recv: i64, a: i64, b: i64) -> i64 {
    match name {
        "put" => map_put(recv, a, b),
        "get" => map_get(recv, a),
        "containsKey" => i64::from(map_find(recv, a).is_some()),
        "remove" => map_remove(recv, a),
        "size" => rd_i32(recv, F_LEN).max(0) as i64,
        "isEmpty" => i64::from(rd_i32(recv, F_LEN) <= 0),
        "clear" => {
            map_clear(recv);
            0
        }
        "keySet" => map_view(recv, 0),
        "values" => map_view(recv, 1),
        "entrySet" => map_view(recv, 2),
        "toString" => map_text(recv),
        _ => 0,
    }
}

fn throwable_class_name(recv: i64) -> String {
    if recv == 0 {
        return "java.lang.Throwable".to_string();
    }
    rt::class_internal_of(rt::tag_of_class(read_u32(recv))).replace('/', ".")
}
fn throwable_call(name: &str, recv: i64) -> i64 {
    let msg = rt::exc_msg_text(recv);
    match name {
        // 每次重新分配一个字符串：文本是真值，不依赖可能被 GC 搬走的偏移
        "getMessage" => match msg {
            Some(t) => alloc_str(t.as_bytes()),
            None => 0,
        },
        "toString" => {
            let cls = throwable_class_name(recv);
            // Java 的规则：detailMessage == null 才只打类名
            let t = match msg {
                Some(m) => format!("{cls}: {m}"),
                None => cls,
            };
            alloc_str(t.as_bytes())
        }
        "printStackTrace" => {
            // 走 stderr：HotSpot 的行号无法逐字复现，不参与 stdout 的三方比对
            let cls = throwable_class_name(recv);
            match msg {
                Some(m) => eprintln!("{cls}: {m}"),
                None => eprintln!("{cls}"),
            }
            0
        }
        _ => 0,
    }
}
/// 迭代器的 `hasNext` / `next`（挂成原生虚方法，见 `register_api`）
unsafe extern "C" fn itr_has_next(locals: *mut i64) -> i64 {
    let this = unsafe { *locals };
    if this == 0 {
        return 0;
    }
    let cur = rd_i32(this, I_CUR).max(0);
    let size = rd_i32(this, I_SIZE).max(0);
    i64::from(cur < size)
}

unsafe extern "C" fn itr_next(locals: *mut i64) -> i64 {
    let this = unsafe { *locals };
    if this == 0 {
        return 0;
    }
    let arr = rd_ref(this, I_ARR);
    let cur = rd_i32(this, I_CUR).max(0) as u32;
    let size = rd_i32(this, I_SIZE).max(0) as u32;
    if arr == 0 || cur >= size {
        return 0;
    }
    wr_i32(this, I_CUR, (cur + 1) as i32);
    arr_get(arr, cur)
}
/// 把追加的类与原生虚方法登记进运行时（`jit::mod` 在初始化时调一次）
pub fn register_api() {
    for c in [
        "java/util/ArrayList",
        "java/util/List",
        "java/util/Set",
        "java/util/Collection",
        "java/util/HashMap",
        "java/util/Map",
    ] {
        coll_class(c);
    }
    rt::register_class(
        rt::class_tag("java/util/Map$Entry"),
        "java/util/Map$Entry",
        &["Ljava/lang/Object;".to_string(), "Ljava/lang/Object;".to_string()],
    );
    itr_class();
    let tag = rt::class_tag("java/util/ArrayList$Itr");
    rt::register_vmethod(
        tag,
        rt::intern_sig("hasNext", "()Z"),
        itr_has_next as usize,
        rt::RET_I32,
    );
    rt::register_vmethod(
        tag,
        rt::intern_sig("next", "()Ljava/lang/Object;"),
        itr_next as usize,
        rt::RET_I64,
    );
}

// ---------- 静态方法 ----------

/// 从 `Object[]` 里取出格式化实参
fn arg_i64(h: i64) -> i64 {
    if h != 0 {
        let cls = rt::class_internal_of(rt::tag_of_class(read_u32(h)));
        if jdk::wrapper_prim(&cls).is_some() {
            return wrapper_value(h);
        }
    }
    h
}

fn arg_f64(h: i64) -> f64 {
    if h != 0 {
        let cls = rt::class_internal_of(rt::tag_of_class(read_u32(h)));
        if let Some(p) = jdk::wrapper_prim(&cls) {
            let v = wrapper_value(h);
            return match p {
                "D" => f64::from_bits(v as u64),
                "J" => v as f64,
                _ => v as i32 as f64,
            };
        }
    }
    0.0
}

fn arg_char(h: i64) -> char {
    let v = if h != 0 {
        let cls = rt::class_internal_of(rt::tag_of_class(read_u32(h)));
        if jdk::wrapper_prim(&cls).is_some() {
            wrapper_value(h) as i32
        } else {
            h as i32
        }
    } else {
        0
    };
    char::from_u32(v as u32).unwrap_or('\u{fffd}')
}

fn format_pad(text: String, width: usize, left: bool, zero: bool) -> String {
    let n = text.chars().count();
    if n >= width {
        return text;
    }
    let fill = width - n;
    if left {
        let mut s = text;
        s.push_str(&" ".repeat(fill));
        s
    } else if zero {
        let (sign, body) = if let Some(rest) = text.strip_prefix('-') {
            ("-", rest.to_string())
        } else if let Some(rest) = text.strip_prefix('+') {
            ("+", rest.to_string())
        } else {
            ("", text)
        };
        format!("{sign}{}{body}", "0".repeat(fill))
    } else {
        format!("{}{text}", " ".repeat(fill))
    }
}

/// `String.format`：`%d %s %f %c %x %o %b %% %n` + 宽度/精度/`-`/`0`/`+`
fn java_format(fmt: &str, argv: &[i64]) -> String {
    let cs: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut ai = 0usize;
    let mut i = 0usize;
    while i < cs.len() {
        let c = cs[i];
        i += 1;
        if c != '%' {
            out.push(c);
            continue;
        }
        if i >= cs.len() {
            out.push('%');
            break;
        }
        let (mut left, mut zero, mut plus) = (false, false, false);
        while i < cs.len() {
            match cs[i] {
                '-' => left = true,
                '0' => zero = true,
                '+' => plus = true,
                ' ' | ',' | '(' | '#' => {}
                _ => break,
            }
            i += 1;
        }
        let mut width = 0usize;
        while i < cs.len() && cs[i].is_ascii_digit() {
            width = width * 10 + (cs[i] as u8 - b'0') as usize;
            i += 1;
        }
        let mut prec: Option<usize> = None;
        if i < cs.len() && cs[i] == '.' {
            i += 1;
            let mut p = 0usize;
            while i < cs.len() && cs[i].is_ascii_digit() {
                p = p * 10 + (cs[i] as u8 - b'0') as usize;
                i += 1;
            }
            prec = Some(p);
        }
        let conv = if i < cs.len() { cs[i] } else { ' ' };
        if i < cs.len() {
            i += 1;
        }
        if conv == '%' {
            out.push('%');
            continue;
        }
        if conv == 'n' {
            out.push('\n');
            continue;
        }
        let arg = argv.get(ai).copied().unwrap_or(0);
        ai += 1;
        let raw = match conv {
            'd' => {
                let v = arg_i64(arg);
                if plus && v >= 0 {
                    format!("+{v}")
                } else {
                    v.to_string()
                }
            }
            'x' => format!("{:x}", arg_i64(arg)),
            'X' => format!("{:X}", arg_i64(arg)),
            'o' => format!("{:o}", arg_i64(arg)),
            'f' => format!("{:.*}", prec.unwrap_or(6), arg_f64(arg)),
            's' | 'S' => {
                let t = obj_to_string(arg);
                if let Some(p) = prec {
                    t.chars().take(p).collect()
                } else {
                    t
                }
            }
            'c' => arg_char(arg).to_string(),
            'b' => {
                if arg == 0 {
                    "false".to_string()
                } else {
                    let cls = rt::class_internal_of(rt::tag_of_class(read_u32(arg)));
                    match jdk::wrapper_prim(&cls) {
                        Some("Z") => {
                            (if wrapper_value(arg) != 0 { "true" } else { "false" }).to_string()
                        }
                        _ => "true".to_string(),
                    }
                }
            }
            other => other.to_string(),
        };
        out.push_str(&format_pad(raw, width, left, zero));
    }
    out
}

// ---------- 数组工具 ----------

fn elem_read(arr: i64, i: u32, esize: u32) -> i64 {
    let p = rt::base_ptr()
        .wrapping_add(arr as usize + ARRAY_HEADER + (i as usize) * esize as usize);
    unsafe {
        if esize == 8 {
            std::ptr::read_unaligned(p as *const i64)
        } else {
            std::ptr::read_unaligned(p as *const i32) as i64
        }
    }
}

fn elem_write(arr: i64, i: u32, esize: u32, v: i64) {
    if esize == 4 {
        let desc = arr_desc_of(arr);
        if rt::array_desc_shape(&desc[1..]).1 {
            rt::set_ref(arr, i, v);
            return;
        }
    }
    let p = rt::base_ptr()
        .wrapping_add(arr as usize + ARRAY_HEADER + (i as usize) * esize as usize);
    unsafe {
        if esize == 8 {
            std::ptr::write_unaligned(p as *mut i64, v);
        } else {
            std::ptr::write_unaligned(p as *mut i32, v as i32);
        }
    }
}

fn arr_desc_of(arr: i64) -> String {
    rt::class_internal_of(rt::tag_of_class(read_u32(arr)))
}

fn arr_shown(arr: i64) -> i64 {
    if arr == 0 {
        return alloc_str(b"null");
    }
    let desc = arr_desc_of(arr);
    let (esize, is_ref) = rt::array_desc_shape(&desc[1..]);
    let len = arr_len(arr);
    let mut parts: Vec<String> = Vec::with_capacity(len as usize);
    for i in 0..len {
        let raw = elem_read(arr, i, esize);
        let t = if is_ref || desc.starts_with("[L") || desc.starts_with("[[") {
            obj_to_string(raw)
        } else if esize == 8 {
            if desc.starts_with("[D") {
                crate::java_double_to_string(f64::from_bits(raw as u64))
            } else {
                raw.to_string()
            }
        } else if desc.starts_with("[Z") {
            (if raw != 0 { "true" } else { "false" }).to_string()
        } else if desc.starts_with("[C") {
            char::from_u32(raw as u32).unwrap_or('\0').to_string()
        } else if desc.starts_with("[F") {
            crate::java_float_to_string(f32::from_bits(raw as u32))
        } else {
            raw.to_string()
        };
        parts.push(t);
    }
    alloc_str(format!("[{}]", parts.join(", ")).as_bytes())
}

fn arr_compare(a: i64, b: i64) -> std::cmp::Ordering {
    if a == b {
        return std::cmp::Ordering::Equal;
    }
    if a == 0 {
        return std::cmp::Ordering::Less;
    }
    if b == 0 {
        return std::cmp::Ordering::Greater;
    }
    let ca = rt::class_internal_of(rt::tag_of_class(read_u32(a)));
    if ca == "java/lang/String" {
        return str_bytes(a).cmp(&str_bytes(b));
    }
    let cb = rt::class_internal_of(rt::tag_of_class(read_u32(b)));
    match (jdk::wrapper_prim(&ca), jdk::wrapper_prim(&cb)) {
        (Some(_), Some(_)) => {
            let x = wrapper_value(a);
            let y = wrapper_value(b);
            if ca.contains("Double") || ca.contains("Float") {
                let fx = if ca.contains("Double") {
                    f64::from_bits(x as u64)
                } else {
                    f32::from_bits(x as u32) as f64
                };
                let fy = if cb.contains("Double") {
                    f64::from_bits(y as u64)
                } else {
                    f32::from_bits(y as u32) as f64
                };
                fx.partial_cmp(&fy).unwrap_or(std::cmp::Ordering::Equal)
            } else {
                x.cmp(&y)
            }
        }
        _ => std::cmp::Ordering::Equal,
    }
}

/// 追加的静态 API 分派
fn api_static(class: &str, name: &str, desc: &str, a: i64, b: i64) -> Option<i64> {
    let out = match (class, name) {
        ("java/lang/String", "valueOf") => {
            let t = if desc.starts_with("(Ljava/lang/Object;)") {
                obj_to_string(a)
            } else if desc.starts_with("(Z)") {
                (if a as i32 != 0 { "true" } else { "false" }).to_string()
            } else if desc.starts_with("(C)") {
                char::from_u32(a as i32 as u32).unwrap_or('\0').to_string()
            } else if desc.starts_with("(I)") {
                (a as i32).to_string()
            } else if desc.starts_with("(J)") {
                a.to_string()
            } else if desc.starts_with("(F)") {
                crate::java_float_to_string(f32::from_bits(a as u32))
            } else if desc.starts_with("(D)") {
                crate::java_double_to_string(f64::from_bits(a as u64))
            } else if desc.starts_with("([C)") {
                let len = if a != 0 { arr_len(a) } else { 0 };
                (0..len)
                    .map(|i| char::from_u32(elem_read(a, i, 4) as u32).unwrap_or('\0'))
                    .collect()
            } else {
                String::new()
            };
            alloc_str(t.as_bytes())
        }
        ("java/lang/String", "format") => {
            let fmt = String::from_utf8_lossy(&str_bytes(a)).into_owned();
            let argv: Vec<i64> = if b != 0 {
                let len = arr_len(b);
                (0..len).map(|i| arr_get(b, i)).collect()
            } else {
                Vec::new()
            };
            alloc_str(java_format(&fmt, &argv).as_bytes())
        }
        ("java/lang/Math", "abs") => match desc {
            "(I)I" => (a as i32).wrapping_abs() as i64,
            "(J)J" => a.wrapping_abs(),
            "(F)F" => f32::from_bits(a as u32).abs().to_bits() as i64,
            _ => f64::from_bits(a as u64).abs().to_bits() as i64,
        },
        ("java/lang/Math", "max") => match desc {
            "(II)I" => (a as i32).max(b as i32) as i64,
            "(JJ)J" => a.max(b),
            _ => f64::from_bits(a as u64).max(f64::from_bits(b as u64)).to_bits() as i64,
        },
        ("java/lang/Math", "min") => match desc {
            "(II)I" => (a as i32).min(b as i32) as i64,
            "(JJ)J" => a.min(b),
            _ => f64::from_bits(a as u64).min(f64::from_bits(b as u64)).to_bits() as i64,
        },
        ("java/lang/Math", "sqrt") => f64::from_bits(a as u64).sqrt().to_bits() as i64,
        ("java/lang/Math", "pow") => f64::from_bits(a as u64)
            .powf(f64::from_bits(b as u64))
            .to_bits() as i64,
        ("java/lang/Math", "floor") => f64::from_bits(a as u64).floor().to_bits() as i64,
        ("java/lang/Math", "ceil") => f64::from_bits(a as u64).ceil().to_bits() as i64,
        ("java/lang/Math", "signum") => {
            let d = f64::from_bits(a as u64);
            let r = if d > 0.0 {
                1.0
            } else if d < 0.0 {
                -1.0
            } else {
                d
            };
            r.to_bits() as i64
        }
        ("java/lang/Math", "round") => match desc {
            "(F)I" => (f32::from_bits(a as u32) + 0.5).floor() as i32 as i64,
            _ => (f64::from_bits(a as u64) + 0.5).floor() as i64,
        },
        ("java/lang/Math", "random") => rng_next().to_bits() as i64,
        ("java/lang/Integer", "toHexString") => alloc_str(format!("{:x}", a as i32 as u32).as_bytes()),
        ("java/lang/Integer", "toBinaryString") => {
            alloc_str(format!("{:b}", a as i32 as u32).as_bytes())
        }
        ("java/lang/Integer", "toOctalString") => {
            alloc_str(format!("{:o}", a as i32 as u32).as_bytes())
        }
        ("java/lang/Integer", "compare") => (a as i32).cmp(&(b as i32)) as i64,
        ("java/lang/Long", "toHexString") => alloc_str(format!("{:x}", a as u64).as_bytes()),
        ("java/lang/Long", "compare") => a.cmp(&b) as i64,
        ("java/lang/Character", ch) => {
            // 这个分支是**通配**的，必须把不属于这里的方法踢回去 ——
            // 否则 `Character.valueOf('h')` 会被吞成 0（曾实测 `%c` 打出空格）
            if !matches!(
                ch,
                "isDigit"
                    | "isLetter"
                    | "isLetterOrDigit"
                    | "isUpperCase"
                    | "isLowerCase"
                    | "isWhitespace"
                    | "toUpperCase"
                    | "toLowerCase"
                    | "getNumericValue"
            ) {
                return None;
            }
            let c = char::from_u32(a as i32 as u32);
            match ch {
                "isDigit" => i64::from(c.map(|c| c.is_ascii_digit()).unwrap_or(false)),
                "isLetter" => i64::from(c.map(|c| c.is_alphabetic()).unwrap_or(false)),
                "isLetterOrDigit" => i64::from(c.map(|c| c.is_alphanumeric()).unwrap_or(false)),
                "isUpperCase" => i64::from(c.map(|c| c.is_uppercase()).unwrap_or(false)),
                "isLowerCase" => i64::from(c.map(|c| c.is_lowercase()).unwrap_or(false)),
                "isWhitespace" => i64::from(c.map(|c| c.is_whitespace()).unwrap_or(false)),
                "toUpperCase" => c
                    .and_then(|c| c.to_uppercase().next())
                    .map(|c| c as i64)
                    .unwrap_or(a as i32 as i64),
                "toLowerCase" => c
                    .and_then(|c| c.to_lowercase().next())
                    .map(|c| c as i64)
                    .unwrap_or(a as i32 as i64),
                "getNumericValue" => match c {
                    Some(c) if c.is_ascii_digit() => c as i64 - '0' as i64,
                    Some(c) if c.is_ascii_alphabetic() => {
                        c.to_ascii_lowercase() as i64 - 'a' as i64 + 10
                    }
                    _ => -1,
                },
                _ => 0,
            }
        }
        ("java/util/Arrays", "toString") => arr_shown(a),
        ("java/util/Arrays", "fill") => {
            if a != 0 {
                let desc = arr_desc_of(a);
                let (esize, _) = rt::array_desc_shape(&desc[1..]);
                let len = arr_len(a);
                for i in 0..len {
                    elem_write(a, i, esize, b);
                }
            }
            0
        }
        ("java/util/Arrays", "equals") => {
            if a == b {
                1
            } else if a == 0 || b == 0 {
                0
            } else {
                let la = arr_len(a);
                let lb = arr_len(b);
                if la != lb {
                    0
                } else {
                    let desc = arr_desc_of(a);
                    let (esize, is_ref) = rt::array_desc_shape(&desc[1..]);
                    let same = (0..la).all(|i| {
                        let x = elem_read(a, i, esize);
                        let y = elem_read(b, i, esize);
                        if is_ref {
                            obj_eq(x, y)
                        } else {
                            x == y
                        }
                    });
                    i64::from(same)
                }
            }
        }
        ("java/util/Arrays", "sort") => {
            if a != 0 {
                let desc = arr_desc_of(a);
                let (esize, _) = rt::array_desc_shape(&desc[1..]);
                let len = arr_len(a);
                let mut vals: Vec<i64> = (0..len).map(|i| elem_read(a, i, esize)).collect();
                if esize == 8 && desc.starts_with("[D") {
                    vals.sort_by(|x, y| {
                        f64::from_bits(*x as u64)
                            .partial_cmp(&f64::from_bits(*y as u64))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                } else if desc.starts_with("[J") {
                    vals.sort();
                } else if desc.starts_with("[I") || desc.starts_with("[F") {
                    vals.sort_by_key(|v| *v as i32);
                } else {
                    vals.sort_by(|x, y| arr_compare(*x, *y));
                }
                for (i, v) in vals.into_iter().enumerate() {
                    elem_write(a, i as u32, esize, v);
                }
            }
            0
        }
        _ => return None,
    };
    Some(out)
}

thread_local! {
    static RNG: std::cell::Cell<u64> = const { std::cell::Cell::new(0x2545_F491_4F6C_DD1D) };
}

fn rng_next() -> f64 {
    RNG.with(|s| {
        let v = s
            .get()
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        s.set(v);
        ((v >> 11) as f64) / ((1u64 << 53) as f64)
    })
}

/// 堆对象偏移 → 类 tag（给 `rt::exc_msg_text` 用）
pub fn tag_of_obj(off: i64) -> u32 {
    if off == 0 {
        return u32::MAX;
    }
    rt::tag_of_class(read_u32(off))
}
