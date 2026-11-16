//! 对象头与对象体布局。
//!
//! 整个堆是**一块连续内存**，对象用 `u32` 偏移（相对堆基址，`0` = null）表示。
//! 选偏移而不是裸指针，是因为堆增长时靠 `realloc` 搬整块：基址变了，偏移不变，
//! JIT 存在寄存器里的 handle 因此不会失效（见 `heap::Heap::grow_capacity`）。
//!
//! # 布局
//!
//! ```text
//! 普通对象:  [class_id u32][flags u32][field0][field1]...
//! 数组:      [class_id u32][flags u32][length u32][pad u32][elem0]...
//! ```
//!
//! 头只有 8 字节（对标 HotSpot 的压缩类指针）。字段数放在 `flags` 的高 16 位，
//! 所以「这个对象多大」是 O(1) 自描述的，不需要查类表；数组长度单独占 4 字节，
//! 元素因此从 16 字节处开始（8 字节元素也能对齐）。

/// 对象头：class_id + flags，共 8 字节
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Object {
    pub class_id: u32,
    pub flags: u32,
}

pub const HEADER: usize = std::mem::size_of::<Object>(); // 8
/// 槽宽度：引用存 4 字节偏移，基本类型也占一个槽
pub const SLOT_BYTES: usize = 4;
/// 所有对象大小按 8 字节对齐 —— 卡表按 8 字节粒度定位「对象起点」
pub const ALIGN: usize = 8;
/// 数组元素区起点（头 8 + 长度槽 4 + 对齐填充 4）
pub const ARRAY_HEADER: usize = 16;
/// null 引用 / 空 handle
pub const NULL: u32 = 0;

/// 复制式回收写入的转发地址（放在 `class_id` 字段里，省一个哈希表）
pub const FLAG_FORWARDED: u32 = 1 << 0;
/// 标记位（major 与 minor 的去重都用它，用完即清）
pub const FLAG_MARK: u32 = 1 << 1;
/// 大对象（LOS）：不参与复制，直接生在老年代
pub const FLAG_LOS: u32 = 1 << 2;
/// 数组标记
pub const FLAG_ARRAY: u32 = 1 << 3;
/// 有 finalizer：没标到时先进队列、复活一轮（见 `weak::FinalizerQueue`）
pub const FLAG_FINALIZE: u32 = 1 << 4;
/// 已经进过 finalization 队列（保证只入队一次）
pub const FLAG_FINALIZED: u32 = 1 << 5;
/// 存活年龄，占 8 位；达阈值即晋升
pub const AGE_SHIFT: u32 = 8;
pub const AGE_MASK: u32 = 0xFF << AGE_SHIFT;
/// 字段数（普通对象），占高 16 位；数组为 0
pub const FIELD_SHIFT: u32 = 16;
pub const FIELD_MASK: u32 = 0xFFFF << FIELD_SHIFT;

impl Object {
    #[inline]
    pub const fn align(n: usize) -> usize {
        (n + ALIGN - 1) & !(ALIGN - 1)
    }

    /// 带 `field_count` 个槽的普通对象总字节数
    #[inline]
    pub const fn size_for_fields(field_count: usize) -> usize {
        Self::align(HEADER + field_count * SLOT_BYTES)
    }

    /// 带 `len` 个 `elem_size` 字节元素的数组总字节数
    #[inline]
    pub const fn size_for_array(len: usize, elem_size: usize) -> usize {
        Self::align(ARRAY_HEADER + len * elem_size)
    }
}

#[inline]
pub unsafe fn hdr(base: *mut u8, off: u32) -> *mut Object {
    base.add(off as usize) as *mut Object
}

#[inline]
pub unsafe fn class_of(base: *mut u8, off: u32) -> u32 {
    (*hdr(base, off)).class_id
}

#[inline]
pub unsafe fn flags_of(base: *mut u8, off: u32) -> u32 {
    (*hdr(base, off)).flags
}

#[inline]
pub unsafe fn set_flags(base: *mut u8, off: u32, flags: u32) {
    (*hdr(base, off)).flags = flags;
}

/// 数组长度（放在头后面的独立槽里）
#[inline]
pub unsafe fn length_of(base: *mut u8, off: u32) -> u32 {
    *(base.add(off as usize + HEADER) as *const u32)
}

#[inline]
pub unsafe fn set_length(base: *mut u8, off: u32, len: u32) {
    *(base.add(off as usize + HEADER) as *mut u32) = len;
}

/// 普通对象第 `i` 个字段槽的地址
#[inline]
pub unsafe fn field_slot(base: *mut u8, off: u32, i: u32) -> *mut u32 {
    (base.add(off as usize + HEADER) as *mut u32).add(i as usize)
}

/// 数组第 `i` 个元素的地址
#[inline]
pub unsafe fn elem_ptr(base: *mut u8, off: u32, i: u32, elem_size: u32) -> *mut u8 {
    base.add(off as usize + ARRAY_HEADER + i as usize * elem_size as usize)
}

/// 4 字节元素数组第 `i` 个元素的槽地址（引用元素就用它）
#[inline]
pub unsafe fn elem_slot(base: *mut u8, off: u32, i: u32) -> *mut u32 {
    elem_ptr(base, off, i, 4) as *mut u32
}

#[inline]
pub fn age_of(flags: u32) -> u32 {
    (flags & AGE_MASK) >> AGE_SHIFT
}

#[inline]
pub fn with_age(flags: u32, age: u32) -> u32 {
    (flags & !AGE_MASK) | ((age & 0xFF) << AGE_SHIFT)
}

#[inline]
pub fn field_count_of(flags: u32) -> u32 {
    (flags & FIELD_MASK) >> FIELD_SHIFT
}

#[inline]
pub fn with_field_count(flags: u32, n: u32) -> u32 {
    (flags & !FIELD_MASK) | ((n & 0xFFFF) << FIELD_SHIFT)
}

/// 对象总字节数（自描述：普通对象只看头，数组还要查元素大小）
///
/// # Safety
/// `layout` 必须包含该对象的类。
#[inline]
pub unsafe fn size_at(base: *mut u8, off: u32, layout: &crate::layout::LayoutTable) -> u32 {
    let fl = flags_of(base, off);
    if fl & FLAG_ARRAY != 0 {
        let len = length_of(base, off);
        layout.class(class_of(base, off)).array_size(len)
    } else {
        Object::size_for_fields(field_count_of(fl) as usize) as u32
    }
}

/// 分配后初始化：写头、写数组长度、把字段区清零（引用槽必须显式是 NULL）。
///
/// 字段区按 8 字节字写而不是 `write_bytes`：小对象上 `memset` 的调用开销
/// 比清零本身还大。
///
/// # Safety
/// `off` 必须在堆内、且 `size` 字节可写。
pub unsafe fn init(
    base: *mut u8,
    off: u32,
    class_id: u32,
    flags: u32,
    length: u32,
    size: u32,
) {
    let p = hdr(base, off);
    (*p).class_id = class_id;
    (*p).flags = flags;
    let body = base.add(off as usize + HEADER);
    let words = ((size as usize).saturating_sub(HEADER)) >> 3;
    let d = body as *mut u64;
    let mut i = 0;
    while i < words {
        d.add(i).write_unaligned(0);
        i += 1;
    }
    if flags & FLAG_ARRAY != 0 {
        set_length(base, off, length);
    }
}

/// 复制一个对象（大小是 8 的倍数）。
///
/// 常见尺寸（16/24/32/40/48 字节）直接展开成定长 load/store：
/// 通用循环因为「字数运行期才知道」会退化成小循环，直接
/// `copy_nonoverlapping` 更会变成一次 `memcpy` 调用 —— 在拷贝 1M 对象的
/// minor 里这两者都是可测量的开销。
#[inline]
pub unsafe fn copy_object(src: *mut u8, dst: *mut u8, size: usize) {
    let s = src as *const u64;
    let d = dst as *mut u64;
    match size {
        8 => d.write_unaligned(s.read_unaligned()),
        16 => {
            d.write_unaligned(s.read_unaligned());
            d.add(1).write_unaligned(s.add(1).read_unaligned());
        }
        24 => {
            d.write_unaligned(s.read_unaligned());
            d.add(1).write_unaligned(s.add(1).read_unaligned());
            d.add(2).write_unaligned(s.add(2).read_unaligned());
        }
        32 => {
            d.write_unaligned(s.read_unaligned());
            d.add(1).write_unaligned(s.add(1).read_unaligned());
            d.add(2).write_unaligned(s.add(2).read_unaligned());
            d.add(3).write_unaligned(s.add(3).read_unaligned());
        }
        40 | 48 => {
            let words = size >> 3;
            let mut i = 0;
            while i < words {
                d.add(i).write_unaligned(s.add(i).read_unaligned());
                i += 1;
            }
        }
        _ => std::ptr::copy_nonoverlapping(src, dst, size),
    }
}
