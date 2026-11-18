//! JIT 运行时辅助函数（被生成的机器码直接调用）
//!
//! # 对象布局（librustjrt 的分代堆）
//!
//! ```text
//! 对象: [0..4] class_id [4..8] flags [8..] 字段（每 Java 字段占 8 字节 = 两个 4 字节槽）
//! 数组: [0..4] class_id [4..8] flags [8..12] 长度 [12..16] 填充 [16..] 元素
//! ```
//!
//! **引用是堆内 4 字节偏移**（`0` = null），不是裸指针：GC 搬对象时会把根区和
//! 引用槽里的偏移**就地改写**，所以生成代码一律「基址 + 偏移」现算。基址存在
//! `BASE` 里，堆扩容（realloc 搬块）后由运行时更新，生成代码按固定地址每次重读。
//!
//! 一个 Java 字段占 8 字节（两个 4 字节槽：值 + 填充）是刻意保留的旧布局：
//! 字段偏移 `8 + 8*i` 不变，long/double 天然 8 字节对齐，而引用只占低 4 字节，
//! 正好是 librustjrt 的槽语义（GC 按 `class_id` 的位图找到这些槽）。

use std::cell::RefCell;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::Mutex;

use librustjrt::object::{ARRAY_HEADER, FLAG_ARRAY, HEADER};
// 注意：`librustjrt::Heap` 是 0.1 兼容层，新架构的堆叫 `ManagedHeap`
use librustjrt::{Handle, HeapConfig, ManagedHeap as Heap, SlotKind};

// ---------- 堆 ----------

const HEAP_INITIAL: usize = 64 << 20;
const HEAP_MAX: usize = 1 << 30;

/// 初始堆大小（MB）：`VMLESS_HEAP_MB` 可覆盖 —— 基准里用一个小堆，
/// 几次分配就能把 eden 填满，GC 才跑得起来。
fn heap_initial() -> usize {
    std::env::var("VMLESS_HEAP_MB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|mb| *mb > 0)
        .map(|mb| mb << 20)
        .unwrap_or(HEAP_INITIAL)
}

/// 两个常驻根槽（下标固定，永不出栈）：0 = 待抛异常对象，1 = 异常消息
const ROOT_EXC: usize = 0;
const ROOT_MSG: usize = 1;
const RESERVED_ROOTS: usize = 2;

thread_local! {
    /// 单线程 JIT：一个线程一个堆（`Heap` 不是 `Sync`，`RefCell` 就够）
    static HEAP: RefCell<Option<Heap>> = RefCell::new(None);
}

/// 堆基址。生成代码按固定地址读它（`base_addr()` 把地址当立即数嵌进机器码）。
static BASE: AtomicUsize = AtomicUsize::new(0);

pub fn with_heap<R>(f: impl FnOnce(&mut Heap) -> R) -> R {
    HEAP.with(|c| {
        let mut g = c.borrow_mut();
        if g.is_none() {
            let mut cfg = HeapConfig::default();
            cfg.initial = heap_initial();
            cfg.max = HEAP_MAX;
            let h = Heap::new(cfg);
            for _ in 0..RESERVED_ROOTS {
                h.push_root(Handle::NULL);
            }
            BASE.store(h.base() as usize, Ordering::Relaxed);
            *g = Some(h);
        }
        let h = g.as_mut().expect("JIT 堆未初始化");
        let r = f(h);
        // 扩容会 realloc 搬块：每次交互后都把基址刷新一遍
        BASE.store(h.base() as usize, Ordering::Relaxed);
        r
    })
}

/// 堆基址变量的地址（编译期嵌进机器码，生成代码用 `load.i64` 读当前基址）
pub fn base_addr() -> i64 {
    &BASE as *const AtomicUsize as i64
}

/// 当前基址（Rust 侧用）
pub fn base_ptr() -> *mut u8 {
    BASE.load(Ordering::Relaxed) as *mut u8
}

// ---------- 类型标签 ↔ class_id ----------
//
// 生成代码里凡是「读对象头拿类型」的地方都必须走 `tag_of_class`：对象头里存的是
// `class_id`（librustjrt 分配），而 instanceof/checkcast/vcall 的立即数是 tag。

/// class_id → tag（下标是 class_id；注册过才有值）
///
/// **读路径无锁**：生成代码里每个 `instanceof`/`checkcast`/`vcall` 都要读一次这张表，
/// 之前是一把 `Mutex`，实测 16M 次虚调用要 1.7 秒（每次两把锁 + 线性扫全表）。
/// 现在主表是一块**定长原子数组**（首用才分配），读只有「一次原子加载 + 一次下标」，
/// 写仍走下面的 `Mutex<Vec>` 当权威 + 越界兜底。
static TAG_OF_CLASS: Mutex<Vec<u32>> = Mutex::new(Vec::new());
/// 无锁镜像：`TAG_FAST[id] = tag`，初值 0（未注册 = 按 Object 处理，与旧行为一致）
static TAG_FAST: std::sync::atomic::AtomicPtr<u32> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
/// 镜像容量（class_id 超过它就退回 mutex 那条路）
const TAG_FAST_CAP: usize = 1 << 16;
/// tag → class_id（`u32::MAX` = 还没注册）
static CLASS_OF_TAG: Mutex<Vec<u32>> = Mutex::new(Vec::new());

const NO_CLASS: u32 = u32::MAX;

/// 取（必要时分配）无锁 tag 镜像
#[inline]
fn tag_fast() -> Option<&'static [std::sync::atomic::AtomicU32]> {
    use std::sync::atomic::AtomicU32;
    static INIT: Mutex<()> = Mutex::new(());
    let p = TAG_FAST.load(Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: 一旦写入就一直有效（只泄漏、不释放），长度恒为 TAG_FAST_CAP
        return Some(unsafe { std::slice::from_raw_parts(p as *const AtomicU32, TAG_FAST_CAP) });
    }
    let _g = INIT.lock().unwrap();
    let p = TAG_FAST.load(Ordering::Acquire);
    if !p.is_null() {
        return Some(unsafe { std::slice::from_raw_parts(p as *const AtomicU32, TAG_FAST_CAP) });
    }
    let v: Box<[AtomicU32]> = (0..TAG_FAST_CAP).map(|_| AtomicU32::new(0)).collect();
    let raw = Box::into_raw(v) as *mut u32;
    TAG_FAST.store(raw, Ordering::Release);
    Some(unsafe { std::slice::from_raw_parts(raw as *const AtomicU32, TAG_FAST_CAP) })
}

#[inline]
pub fn tag_of_class(class_id: u32) -> u32 {
    if (class_id as usize) < TAG_FAST_CAP {
        if let Some(t) = tag_fast() {
            return t[class_id as usize].load(Ordering::Relaxed);
        }
    }
    let g = TAG_OF_CLASS.lock().unwrap();
    match g.get(class_id as usize) {
        Some(&t) => t,
        None => 0, // 未注册（<opaque>）：按 Object 处理，不炸
    }
}

/// tag → class_id；没注册过就 panic（编译期就该注册好，运行期拿到 NO_CLASS 是 bug）
pub fn class_id_of(tag: u32) -> u32 {
    let g = CLASS_OF_TAG.lock().unwrap();
    match g.get(tag as usize).copied() {
        Some(id) if id != NO_CLASS => id,
        _ => panic!("类 tag {tag} 没有注册进 LayoutTable（JIT 编译期漏了 register_class）"),
    }
}

fn record_mapping(tag: u32, class_id: u32) {
    // 无锁镜像先写（读端只看它）
    if (class_id as usize) < TAG_FAST_CAP {
        if let Some(t) = tag_fast() {
            t[class_id as usize].store(tag, Ordering::Relaxed);
        }
    }
    let mut tm = TAG_OF_CLASS.lock().unwrap();
    if tm.len() <= class_id as usize {
        tm.resize(class_id as usize + 1, 0);
    }
    tm[class_id as usize] = tag;
    drop(tm);
    let mut cm = CLASS_OF_TAG.lock().unwrap();
    if cm.len() <= tag as usize {
        cm.resize(tag as usize + 1, NO_CLASS);
    }
    cm[tag as usize] = class_id;
}

fn is_ref_desc(d: &str) -> bool {
    matches!(d.as_bytes().first().copied(), Some(b'L') | Some(b'['))
}

/// 注册一个对象类（幂等）。`descs` 是实例字段描述符，顺序与 JIT 的字段下标一致。
///
/// 每个 Java 字段注册成**两个** librustjrt 槽：`[值/引用][填充]`。这样字段偏移
/// 仍是 `8 + 8*i`，而 GC 只追踪每个字段的第一个槽。
pub fn register_class(tag: u32, name: &str, descs: &[String]) -> u32 {
    {
        let g = CLASS_OF_TAG.lock().unwrap();
        if let Some(&id) = g.get(tag as usize) {
            if id != NO_CLASS {
                return id;
            }
        }
    }
    let mut slots: Vec<SlotKind> = Vec::with_capacity(descs.len() * 2);
    for d in descs {
        slots.push(if is_ref_desc(d) { SlotKind::Ref } else { SlotKind::Prim });
        slots.push(SlotKind::Prim);
    }
    let id = with_heap(|h| h.register_object(name, &slots));
    record_mapping(tag, id);
    id
}

/// 注册一个数组类（幂等）：元素宽度 / 是否引用都进布局表，GC 按它追元素
pub fn register_array_class(tag: u32, name: &str, elem_size: u32, ref_elem: bool) -> u32 {
    {
        let g = CLASS_OF_TAG.lock().unwrap();
        if let Some(&id) = g.get(tag as usize) {
            if id != NO_CLASS {
                return id;
            }
        }
    }
    let id = with_heap(|h| h.register_array(name, elem_size, ref_elem));
    record_mapping(tag, id);
    id
}

/// 数组描述符（`[I` / `[[Ljava/lang/String;`）→ (元素字节数, 元素是不是引用)
pub fn array_desc_shape(elem_desc: &str) -> (u32, bool) {
    match elem_desc.as_bytes().first().copied() {
        Some(b'J') | Some(b'D') => (8, false),
        Some(b'L') | Some(b'[') => (4, true),
        _ => (4, false),
    }
}

/// 数组描述符 → class_id（编译期/运行期都可调，幂等）
pub fn array_class_of(desc: &str) -> u32 {
    let tag = tag_of(desc);
    let (esize, is_ref) = array_desc_shape(&desc[1..]);
    register_array_class(tag, desc, esize, is_ref)
}

// ---------- 分配 ----------

fn init_array_header(h: &Heap, hd: Handle, class_id: u32, len: u32) {
    unsafe {
        let base = h.base();
        let p = base.add(hd.0 as usize) as *mut librustjrt::object::Object;
        (*p).class_id = class_id;
        (*p).flags = FLAG_ARRAY;
        *(base.add(hd.0 as usize + HEADER) as *mut u32) = len;
    }
}

/// 分配一个对象：TLAB bump + 写头；TLAB 用完就先回收再切一块
pub extern "C" fn new_object(class_id: u32) -> i64 {
    with_heap(|h| {
        let size = h.layout().class(class_id).total_size(0);
        if size >= h.cfg().los_threshold {
            return h.alloc(class_id).raw() as i64;
        }
        if let Some(hd) = h.tlab_alloc_object(class_id) {
            return hd.raw() as i64;
        }
        // 这块 TLAB 用完了：先切一块新的（不回收）
        if h.refill_tlab() {
            if let Some(hd) = h.tlab_alloc_object(class_id) {
                return hd.raw() as i64;
            }
        }
        // eden 也满了：回收一轮（根集 = 影子栈），再切新 TLAB
        h.collect();
        h.refill_tlab();
        if let Some(hd) = h.tlab_alloc_object(class_id) {
            return hd.raw() as i64;
        }
        // 实在装不下（对象比 eden 还大）：走 LOS / 扩容慢路径
        h.alloc(class_id).raw() as i64
    })
}

/// 分配一个数组：元素宽度取自数组类布局
pub extern "C" fn new_array(class_id: u32, len: u32) -> i64 {
    with_heap(|h| {
        let size = h.layout().class(class_id).total_size(len);
        if size >= h.cfg().los_threshold {
            return h.alloc_array(class_id, len).raw() as i64;
        }
        if let Some(hd) = h.tlab_alloc(size) {
            init_array_header(h, hd, class_id, len);
            return hd.raw() as i64;
        }
        if h.refill_tlab() {
            if let Some(hd) = h.tlab_alloc(size) {
                init_array_header(h, hd, class_id, len);
                return hd.raw() as i64;
            }
        }
        h.collect();
        h.refill_tlab();
        if let Some(hd) = h.tlab_alloc(size) {
            init_array_header(h, hd, class_id, len);
            return hd.raw() as i64;
        }
        h.alloc_array(class_id, len).raw() as i64
    })
}

/// `multianewarray`：`counts` 是 ndims 个 i32（从最外层开始），`desc` 指向
/// 「长度 + 内容」缓冲（描述符串由 strtab 常驻）。
pub extern "C" fn new_multi_array(counts: *const i32, ndims: u32, desc: *const u8) -> i64 {
    let len = unsafe { std::ptr::read_unaligned(desc as *const u32) } as usize;
    let bytes = unsafe { std::slice::from_raw_parts(desc.add(8), len) };
    let d = String::from_utf8_lossy(bytes).into_owned();
    let mut dims = Vec::with_capacity(ndims as usize);
    for i in 0..ndims as usize {
        dims.push(unsafe { std::ptr::read_unaligned(counts.add(i)) });
    }
    build_multi(&d, &dims)
}

fn build_multi(desc: &str, counts: &[i32]) -> i64 {
    let n = counts.first().copied().unwrap_or(0).max(0) as u32;
    let class_id = array_class_of(desc);
    let hd = new_array(class_id, n);
    if counts.len() > 1 {
        let elem_desc = &desc[1..];
        let (esize, _) = array_desc_shape(elem_desc);
        // 外层数组要先登记成根：内层的分配可能触发 GC，把还没填完的外层搬走
        let slot = with_heap(|h| {
            h.push_root(Handle(hd as u32));
            h.roots_len() - 1
        });
        for i in 0..n {
            let sub = build_multi(elem_desc, &counts[1..]);
            // 每轮都从根槽重读：GC 可能已经把外层搬到新位置
            let outer = with_heap(|h| {
                let r = h.roots_handle();
                let b = r.borrow();
                b[slot]
            });
            // 内层数组的引用：元素是引用，必须过写屏障
            store_elem_ref(outer, i, sub as u32, esize);
        }
        with_heap(|h| {
            h.pop_root();
        });
    }
    hd
}

fn store_elem_ref(arr: u32, idx: u32, val: u32, _esize: u32) {
    with_heap(|h| h.set_ref(Handle(arr), idx, Handle(val)));
}

/// 写引用槽 / 引用元素：过卡表写屏障（老对象存年轻引用必须把卡标脏）
pub extern "C" fn set_ref(obj: i64, idx: u32, val: i64) {
    with_heap(|h| h.set_ref(Handle(obj as u32), idx, Handle(val as u32)));
}

// ---------- 影子栈（根区）----------
//
// JIT 每帧在**自己的栈帧**里留一块 `max_locals + max_stack` 个 u32 的根区：
// 引用型局部变量只住在这里，操作数栈上的引用在 GC 点前也写进来。帧进入时整块
// 登记进 `Heap` 的根表（拷贝），退出时收回并**把 GC 改写过的新偏移拷回根区**。

/// 把根区登记进堆的根表（帧入口 / 每次 GC 点）
pub extern "C" fn roots_push(p: *mut u32, n: u32) {
    with_heap(|h| {
        let r = h.roots_handle();
        let mut b = r.borrow_mut();
        for i in 0..n as usize {
            b.push(unsafe { *p.add(i) });
        }
    })
}

/// 收回根表并把（可能被 GC 搬过的）偏移写回根区
pub extern "C" fn roots_pop(p: *mut u32, n: u32) {
    with_heap(|h| {
        let r = h.roots_handle();
        let mut b = r.borrow_mut();
        let n = n as usize;
        // 弹得比登记的多 = 生成的代码里 push/pop 不平衡：宁可直接报错，
        // 也不能悄悄把常驻根（异常 / 字符串常量 / 静态引用字段）一起截掉
        assert!(
            b.len() >= n,
            "roots_pop: 根表里只有 {} 项，却要弹出 {n} 项（影子栈 push/pop 不平衡）",
            b.len()
        );
        let start = b.len() - n;
        for i in 0..n {
            let v = b[start + i];
            unsafe { *p.add(i) = v };
        }
        b.truncate(start);
    })
}

// ---------- 类型判断 ----------

/// 对象头里的 class_id → tag
#[inline]
fn class_tag_at(obj: i64) -> u32 {
    if obj == 0 {
        return 0;
    }
    let p = base_ptr().wrapping_add(obj as usize);
    let id = unsafe { std::ptr::read_unaligned(p as *const u32) };
    tag_of_class(id)
}

/// 类名 → 类型标签（0 = java/lang/Object，1 = java/lang/String，其余从 2 起）
static CLASS_TAGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

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
    if tag == 0 {
        return 1; // catch-all（finally）
    }
    i32::from(is_subclass_tag(class_tag_at(obj), tag))
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

fn tag_of(name: &str) -> u32 {
    match name {
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

/// String 类的 class_id（数组形态：元素 1 字节，不追踪引用）
pub fn string_class_id() -> u32 {
    register_array_class(1, "java/lang/String", 1, false)
}

/// 把一段 UTF-8 分配成堆上的 String 对象，并登记成**永久根**（常量活到进程结束）。
/// 返回根槽下标 —— 常量表存的是它，取的时候现读（GC 搬过之后那里是新偏移）。
pub fn alloc_string_const(bytes: &[u8]) -> usize {
    let cid = string_class_id();
    let hd = new_array(cid, bytes.len() as u32);
    unsafe {
        // 元素区从 ARRAY_HEADER(16) 开始：头 8 + 长度槽 4 + 填充 4
        let p = base_ptr().add(hd as u32 as usize + ARRAY_HEADER);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
    }
    with_heap(|h| {
        let r = h.roots_handle();
        let mut b = r.borrow_mut();
        b.push(hd as u32);
        b.len() - 1
    })
}

/// 读一个永久根槽的当前值（GC 可能已经把它改写成新偏移）
pub fn root_value(idx: usize) -> u32 {
    with_heap(|h| {
        let r = h.roots_handle();
        let b = r.borrow();
        b.get(idx).copied().unwrap_or(0)
    })
}

// ---------- 异常 ----------

/// 正在传播的异常：非 0 表示 pending
static EXC_PENDING: AtomicI32 = AtomicI32::new(0);

/// 抛异常：对象存进常驻根槽（GC 搬了也会就地改写），再置 pending
pub extern "C" fn exc_throw(obj: i64) {
    set_reserved_root(ROOT_EXC, obj as u32);
    EXC_PENDING.store(1, Ordering::Relaxed);
}

pub extern "C" fn exc_pending() -> i32 {
    EXC_PENDING.load(Ordering::Relaxed)
}

/// 取当前待抛的异常对象（偏移从常驻根槽读，永远是 GC 后的新位置）
pub extern "C" fn exc_get() -> i64 {
    reserved_root(ROOT_EXC) as i64
}

pub extern "C" fn exc_clear() {
    EXC_PENDING.store(0, Ordering::Relaxed);
}

/// 记下异常对象的消息（`new RuntimeException("boom")` 那一步）。
/// 消息也进常驻根槽：GC 搬过之后 print_uncaught 还能读对。
pub extern "C" fn exc_set_msg(obj: i64, msg: i64) {
    set_reserved_root(ROOT_MSG, msg as u32);
    exc_msg_remember(obj, msg);
}

static EXC_MSGS: Mutex<Vec<(i64, u32, String)>> = Mutex::new(Vec::new());

/// 记下「这个异常对象的消息文本」。
///
/// **不能往根栈里插永久根**：`roots_pop(p, n)` 是按**个数**从栈顶弹的，
/// 中途插进去的根会让帧的 pop 把别的槽弹掉（实测直接把 E01/E02/E04 三个
/// 异常样例打成不一致）。所以这里只存**偏移快照 + 文本**：
/// 快照命中就直接重建字符串，快照过期（GC 搬过）就退回「同类最近一条」。
fn exc_msg_remember(obj: i64, msg: i64) {
    // msg == 0（`RuntimeException((String) null)`）不记：Java 的 toString 此时只打类名
    if obj == 0 || msg == 0 {
        return;
    }
    let tag = super::natives::tag_of_obj(obj);
    let text = super::natives::obj_to_string(msg);
    EXC_MSGS.lock().unwrap().push((obj, tag, text));
}

/// `Throwable.getMessage()` 的文本：按对象偏移快照匹配。
///
/// **不做「同类最近一条」兜底**：`throw new RuntimeException()` 没有消息，
/// 兜底会把上一条同类的 "boom" 借过来（实测 `getMessage()==null` 变成 false）。
/// 代价是构造与读取之间若发生过 GC（对象被搬动）消息会丢，退化成 null ——
/// 这是已知限制，比给错消息好。
pub fn exc_msg_text(recv: i64) -> Option<String> {
    if recv == 0 {
        return None;
    }
    let g = EXC_MSGS.lock().unwrap();
    g.iter().rev().find(|(o, _, _)| *o == recv).map(|(_, _, t)| t.clone())
}

fn set_reserved_root(idx: usize, val: u32) {
    with_heap(|h| {
        let r = h.roots_handle();
        let mut b = r.borrow_mut();
        if b.len() <= idx {
            b.resize(idx + 1, 0);
        }
        b[idx] = val;
    });
}

fn reserved_root(idx: usize) -> u32 {
    with_heap(|h| {
        let r = h.roots_handle();
        let b = r.borrow();
        b.get(idx).copied().unwrap_or(0)
    })
}

/// 未捕获异常：按 JVM 的格式打到 stderr 并结束进程（退出码 1）
pub fn print_uncaught(obj: i64) -> ! {
    let tag = class_tag_at(obj);
    let class = class_name_of(tag);
    let msg = reserved_root(ROOT_MSG);
    if msg != 0 {
        let p = base_ptr().wrapping_add(msg as usize);
        let len = unsafe { std::ptr::read_unaligned(p.add(HEADER) as *const u32) } as usize;
        let bytes = unsafe { std::slice::from_raw_parts(p.add(ARRAY_HEADER), len) };
        eprintln!("Exception in thread \"main\" {class}: {}", String::from_utf8_lossy(bytes));
    } else {
        eprintln!("Exception in thread \"main\" {class}");
    }
    std::process::exit(1);
}

// ---------- 打印 ----------

/// 打印一个 int（对应 System.out.println(int)）
pub extern "C" fn println_int(v: i32) {
    println!("{v}");
}

/// 打印一个 boolean（对应 System.out.println(boolean)）
pub extern "C" fn println_bool(v: i32) {
    println!("{}", if v != 0 { "true" } else { "false" });
}

/// 打印一个 char（对应 `System.out.println(char)`）：按字符而不是码点
pub extern "C" fn println_char(v: i32) {
    println!("{}", char::from_u32(v as u32).unwrap_or('?')); }

/// 打印一个 long（对应 System.out.println(long)）
pub extern "C" fn println_long(v: i64) {
    println!("{v}");
}

/// 打印一个 double（对应 System.out.println(double)，格式与 Java 一致）
pub extern "C" fn println_double(v: f64) {
    println!("{}", crate::java_double_to_string(v));
}

/// 打印一个 float（对应 System.out.println(float)）：**f32 的十进制形态与 double 不同**
pub extern "C" fn println_float(v: f32) {
    println!("{}", crate::java_float_to_string(v));
}

/// double 取余（drem 没有对应的 cranelift 指令，走 CRT 的 fmod）
pub extern "C" fn drem(a: f64, b: f64) -> f64 {
    unsafe extern "C" {
        fn fmod(a: f64, b: f64) -> f64;
    }
    unsafe { fmod(a, b) }
}

/// float 取余（frem 同理，走 CRT 的 fmodf）
pub extern "C" fn frem(a: f32, b: f32) -> f32 {
    unsafe extern "C" {
        fn fmodf(a: f32, b: f32) -> f32;
    }
    unsafe { fmodf(a, b) }
}

/// 打印一段 UTF-8 文本（指针 + 长度，缓冲常驻、不归 GC 管）
pub extern "C" fn println_str(ptr: *const u8, len: u32) {
    let s = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
    println!("{}", String::from_utf8_lossy(s));
}

// ---------- 静态字段与静态初始化 ----------
//
// 引用型静态字段是**永久根**：它的存储就是堆根表里的一个槽，GC 会就地改写。
// 否则 GC 搬走对象之后静态字段里留的就是个野偏移。

/// (类, 字段) → 静态槽下标（编译期登记，运行期按下标读写）
static STATIC_SLOTS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
static STATIC_VALS: Mutex<Vec<i64>> = Mutex::new(Vec::new());
/// 静态槽 → 堆根表下标（只对引用型字段有值）
static STATIC_ROOTS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// 登记一个静态字段，返回它的槽下标（同一个字段永远同号）。
/// `is_ref` 的字段会额外占一个永久根槽。
pub fn static_slot(class: &str, name: &str, is_ref: bool) -> u32 {
    let mut g = STATIC_SLOTS.lock().unwrap();
    if let Some(i) = g.iter().position(|(c, n)| c == class && n == name) {
        return i as u32;
    }
    g.push((class.to_string(), name.to_string()));
    let i = g.len() - 1;
    drop(g);
    let mut v = STATIC_VALS.lock().unwrap();
    while v.len() <= i {
        v.push(0);
    }
    drop(v);
    let root_idx = if is_ref {
        with_heap(|h| {
            let r = h.roots_handle();
            let mut b = r.borrow_mut();
            b.push(0);
            b.len() - 1
        })
    } else {
        usize::MAX
    };
    let mut sr = STATIC_ROOTS.lock().unwrap();
    while sr.len() <= i {
        sr.push(usize::MAX);
    }
    sr[i] = root_idx;
    i as u32
}

fn static_root_of(idx: u32) -> Option<usize> {
    let sr = STATIC_ROOTS.lock().unwrap();
    match sr.get(idx as usize).copied() {
        Some(r) if r != usize::MAX => Some(r),
        _ => None,
    }
}

/// 读静态字段（值统一按 i64 位模式存，调用点再按字段类型还原）
pub extern "C" fn get_static(idx: u32) -> i64 {
    if let Some(r) = static_root_of(idx) {
        return with_heap(|h| {
            let roots = h.roots_handle();
            let b = roots.borrow();
            b.get(r).copied().unwrap_or(0) as i64
        });
    }
    STATIC_VALS.lock().unwrap().get(idx as usize).copied().unwrap_or(0)
}

pub extern "C" fn put_static(idx: u32, v: i64) {
    if let Some(r) = static_root_of(idx) {
        with_heap(|h| {
            let roots = h.roots_handle();
            let mut b = roots.borrow_mut();
            if b.len() <= r {
                b.resize(r + 1, 0);
            }
            b[r] = v as u32;
        });
        return;
    }
    let mut g = STATIC_VALS.lock().unwrap();
    let i = idx as usize;
    if g.len() <= i {
        g.resize(i + 1, 0);
    }
    g[i] = v;
}

/// 类标签 → `<clinit>` 的函数指针（finalize 之后才登记得进来）
static CLINITS: Mutex<Vec<(u32, usize)>> = Mutex::new(Vec::new());
static CLINIT_DONE: Mutex<Vec<u32>> = Mutex::new(Vec::new());

pub fn register_clinit(tag: u32, ptr: usize) {
    let mut g = CLINITS.lock().unwrap();
    if let Some(e) = g.iter_mut().find(|(t, _)| *t == tag) {
        e.1 = ptr;
    } else {
        g.push((tag, ptr));
    }
}

/// 静态初始化：第一次访问这个类时跑一次（先标记再调用，自引用不会无限递归）
pub extern "C" fn clinit(tag: u32) {
    {
        let mut done = CLINIT_DONE.lock().unwrap();
        if done.contains(&tag) {
            return;
        }
        done.push(tag);
    }
    let ptr = CLINITS.lock().unwrap().iter().find(|(t, _)| *t == tag).map(|(_, p)| *p);
    if let Some(p) = ptr {
        let f: unsafe extern "C" fn(*mut i64) -> i32 = unsafe { std::mem::transmute(p) };
        unsafe {
            f(std::ptr::null_mut());
        }
    }
}

// ---------- 继承：父类表、虚方法表、虚分派 ----------

/// 我们自己 JIT 出来的类的父类（instanceof / checkcast / 虚分派都要沿链走）
static SUPERS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
/// 类 → 它**直接**实现的接口（一个类可以有多个，所以是表不是映射）
static IFACES: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
/// (方法名, 描述符) → 签名号（调用点只传一个 u32）
static SIG_IDS: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// (类型标签, 签名号) → (函数指针, 返回种类)
static VMETHODS: Mutex<Vec<((u32, u32), (usize, u8))>> = Mutex::new(Vec::new());

/// 虚方法分派的**无锁**哈希表：`(tag, sig) → (函数地址, 返回种类)`。
///
/// 之前每次虚调用都是「锁 `VMETHODS` + 线性扫整个 Vec」—— 16M 次虚调用要 1.7 秒。
/// 现在读路径是「一次原子加载 + 一次开放寻址探测」，写仍然走 mutex（只发生在编译期）。
/// 表建好后不再变；`VMETHOD_COUNT` 与表里的 `n` 对齐就说明没过期。
static VFAST: std::sync::atomic::AtomicPtr<VFast> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
static VMETHOD_COUNT: AtomicUsize = AtomicUsize::new(0);

struct VFast {
    /// 建表时的条目数（与 `VMETHOD_COUNT` 比对判过期）
    n: usize,
    mask: usize,
    /// `(tag << 32) | sig`；`u64::MAX` = 空槽
    keys: Vec<u64>,
    vals: Vec<(usize, u8)>,
}

impl VFast {
    #[inline]
    fn hash(k: u64) -> usize {
        let mut h = k ^ (k >> 33);
        h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
        h ^= h >> 29;
        h as usize
    }

    fn build(list: &[((u32, u32), (usize, u8))]) -> VFast {
        let mut cap = 16usize;
        while cap < list.len() * 2 {
            cap *= 2;
        }
        let mut keys = vec![u64::MAX; cap];
        let mut vals = vec![(0usize, 0u8); cap];
        let mask = cap - 1;
        for &((tag, sig), v) in list {
            let k = ((tag as u64) << 32) | sig as u64;
            let mut i = Self::hash(k) & mask;
            while keys[i] != u64::MAX && keys[i] != k {
                i = (i + 1) & mask;
            }
            keys[i] = k;
            vals[i] = v;
        }
        VFast { n: list.len(), mask, keys, vals }
    }

    #[inline]
    fn get(&self, tag: u32, sig: u32) -> Option<(usize, u8)> {
        let k = ((tag as u64) << 32) | sig as u64;
        let mut i = Self::hash(k) & self.mask;
        loop {
            let kk = self.keys[i];
            if kk == k {
                return Some(self.vals[i]);
            }
            if kk == u64::MAX {
                return None;
            }
            i = (i + 1) & self.mask;
        }
    }
}

/// 取（必要时重建）无锁虚方法表。登记只发生在编译期，运行期最多建一次。
fn vfast() -> &'static VFast {
    let want = VMETHOD_COUNT.load(Ordering::Acquire);
    let p = VFAST.load(Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: 只要 `n` 对得上就是这份，泄漏的表永远有效
        let f = unsafe { &*p };
        if f.n == want {
            return f;
        }
    }
    let list = VMETHODS.lock().unwrap();
    let f = Box::into_raw(Box::new(VFast::build(&list)));
    VMETHOD_COUNT.store(list.len(), Ordering::Release);
    VFAST.store(f, Ordering::Release);
    // SAFETY: 刚建的表，长度足够；旧表泄漏（编译期最多几张）
    unsafe { &*f }
}

/// 返回值种类
pub const RET_I32: u8 = 0;
pub const RET_I64: u8 = 1;
pub const RET_F64: u8 = 2;
/// float 的返回走独立的 ABI 种类：Cranelift 的 F32 返回值在 xmm0 低 4 字节，
/// 按 RET_F64 还原会把高 4 字节的垃圾一起读进来
pub const RET_F32: u8 = 3;

/// 登记一个用户类的父类名
pub fn register_super(class: &str, super_name: &str) {
    let mut g = SUPERS.lock().unwrap();
    if let Some(e) = g.iter_mut().find(|(c, _)| c == class) {
        e.1 = super_name.to_string();
    } else {
        g.push((class.to_string(), super_name.to_string()));
    }
}

/// 直接父类：先看登记过的用户类，再看写死的 java.* 链
fn super_of_name(class: &str) -> Option<String> {
    if let Some((_, s)) = SUPERS.lock().unwrap().iter().find(|(c, _)| c == class) {
        return Some(s.clone());
    }
    crate::vm::builtin_super(class).map(|s| s.to_string())
}

/// 登记「class 实现了 iface」
pub fn register_iface(class: &str, iface: &str) {
    let mut g = IFACES.lock().unwrap();
    if !g.iter().any(|(c, i)| c == class && i == iface) {
        g.push((class.to_string(), iface.to_string()));
    }
}

fn ifaces_of(class: &str) -> Vec<String> {
    IFACES.lock().unwrap().iter().filter(|(c, _)| c == class).map(|(_, i)| i.clone()).collect()
}

/// `actual`（含继承链上每一层）有没有实现 `target`（接口 extends 接口也算）
fn implements_iface(actual: &str, target: &str) -> bool {
    let mut queue: Vec<String> = vec![actual.to_string()];
    let mut cur = actual.to_string();
    for _ in 0..64 {
        match super_of_name(&cur) {
            Some(s) if s != cur => {
                queue.push(s.clone());
                cur = s;
            }
            _ => break,
        }
    }
    let mut seen = std::collections::HashSet::new();
    while let Some(c) = queue.pop() {
        for i in ifaces_of(&c) {
            if i == target {
                return true;
            }
            if seen.insert(i.clone()) {
                queue.push(i);
            }
        }
    }
    false
}

/// actual 是不是 target 本身、它的子类，或它实现的接口
fn is_subclass_tag(actual: u32, target: u32) -> bool {
    if actual == target {
        return true;
    }
    if target == tag_of("java/lang/Object") {
        return true;
    }
    let mut cur = class_internal_of(actual);
    let t = class_internal_of(target);
    for _ in 0..64 {
        match super_of_name(&cur) {
            Some(s) => {
                if s == t {
                    return true;
                }
                cur = s;
            }
            None => return implements_iface(&class_internal_of(actual), &t),
        }
    }
    implements_iface(&class_internal_of(actual), &t)
}

/// `instanceof`：null 对任何类型都是 false；标签 0（java/lang/Object）对任何非 null 都是 true
pub extern "C" fn instanceof(obj: i64, tag: u32) -> i32 {
    if obj == 0 {
        return 0;
    }
    if tag == 0 {
        return 1;
    }
    i32::from(is_subclass_tag(class_tag_at(obj), tag))
}

/// `checkcast`：类型不符时按 ClassCastException 退出（JIT 还没有异常机制）
pub extern "C" fn checkcast(obj: i64, tag: u32) -> i64 {
    if obj != 0 && tag != 0 {
        let actual = class_tag_at(obj);
        if !is_subclass_tag(actual, tag) {
            eprintln!(
                "ClassCastException: 类型标签 {actual} 的对象不能转换成标签 {tag}（checkcast 失败）"
            );
            std::process::exit(1);
        }
    }
    obj
}

/// (方法名, 描述符) → 签名号（同一个签名永远同号）
pub fn intern_sig(name: &str, desc: &str) -> u32 {
    let key = format!("{name}{desc}");
    let mut g = SIG_IDS.lock().unwrap();
    if let Some(i) = g.iter().position(|s| *s == key) {
        return i as u32;
    }
    g.push(key);
    g.len() as u32 - 1
}

/// 登记一个虚方法实现：`class_tag` 的实例调用 `sig` 时进 `ptr`
pub fn register_vmethod(class_tag: u32, sig: u32, ptr: usize, ret: u8) {
    let mut g = VMETHODS.lock().unwrap();
    if let Some(e) = g.iter_mut().find(|(k, _)| *k == (class_tag, sig)) {
        e.1 = (ptr, ret);
    } else {
        g.push(((class_tag, sig), (ptr, ret)));
    }
    // 条目数变了 → 无锁表过期（`vfast` 靠这个比对重建）
    let n = g.len();
    VMETHOD_COUNT.store(n, Ordering::Release);
}

/// 虚方法分派：`locals[0]` 是接收者，按它的**运行时类型**查表再间接调用。
///
/// 返回值统一按 i64 传回（f64 走位模式、i32 零扩展），调用点再转成真类型。
pub extern "C" fn vcall(locals: *mut i64, sig: u32) -> i64 {
    if locals.is_null() {
        eprintln!("vcall: locals 指针为空");
        std::process::exit(1);
    }
    let obj = unsafe { *locals };
    if obj == 0 {
        eprintln!("NullPointerException: 在 null 上调用虚方法（sig {sig}）");
        std::process::exit(1);
    }
    let tag = class_tag_at(obj);
    let Some((ptr, ret)) = vfast().get(tag, sig) else {
        eprintln!(
            "NoSuchMethodError: 类 {} 没有虚方法（sig {sig}）",
            class_internal_of(tag)
        );
        std::process::exit(1);
    };
    unsafe {
        match ret {
            RET_I64 => {
                let f: unsafe extern "C" fn(*mut i64) -> i64 = std::mem::transmute(ptr);
                f(locals)
            }
            RET_F64 => {
                let f: unsafe extern "C" fn(*mut i64) -> f64 = std::mem::transmute(ptr);
                f(locals).to_bits() as i64
            }
            RET_F32 => {
                // float 的位模式放低 32 位（调用点按 F32 位风格还原，高 32 位无意义）
                let f: unsafe extern "C" fn(*mut i64) -> f32 = std::mem::transmute(ptr);
                f(locals).to_bits() as i64
            }
            _ => {
                let f: unsafe extern "C" fn(*mut i64) -> i32 = std::mem::transmute(ptr);
                f(locals) as i64
            }
        }
    }
}

// ---------- 统计（VMLESS_GC_STATS=1）----------

/// 打印一轮 GC 统计到 stderr（GC 真的在回收的可见证据）
pub fn print_gc_stats(tag: &str) {
    with_heap(|h| {
        let s = h.stats();
        let st = h.spaces();
        eprintln!(
            "[gc] {tag}: minor={} major={} 暂停={:.2}ms 最大={:.2}ms 回收对象={} 晋升={} 老年代活={}B 容量={}B",
            s.minor_count,
            s.major_count,
            s.total_pause_ns as f64 / 1e6,
            s.max_pause_ns as f64 / 1e6,
            s.collected_objects,
            s.promoted_objects,
            h.old_live_bytes(),
            st.cap
        );
    });
}

/// 堆当前占用的字节数（年轻代 + 老年代活对象 + LOS），基准用
pub fn heap_live_bytes() -> usize {
    with_heap(|h| h.young_used() as usize + h.old_live_bytes())
}

/// 已发生的 GC 次数
pub fn gc_count() -> u64 {
    with_heap(|h| {
        let s = h.stats();
        s.minor_count + s.major_count
    })
}


// ---------- 字符串拼接：内建 `java/lang/StringBuilder` ----------
//
// 编译器把 `"a" + b` 编成标准调用链 `new java/lang/StringBuilder` + `append(…)` +
// `toString()`（见 `codegen::expr::emit_concat`）—— 这样 HotSpot 那一路上跑的就是
// 真 StringBuilder。我们没有它的字节码，于是这里内建：
//
// - 对象注册成「一个 int 字段」的类，字段 0 存**缓冲编号**（GC 搬对象时字段跟着走，
//   所以不能用指针做键）；
// - 文本本身留在 Rust 侧的 `SB_BUFS[编号]` 里；
// - `toString()` 把缓冲一次性分配成堆上的 String 对象。

static SB_BUFS: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

/// 字段 0 的偏移（字段按 8 字节槽排：偏移 = HEADER + 8*下标）
const SB_ID_OFF: usize = HEADER;

fn sb_buf_id(obj: i64) -> usize {
    let p = base_ptr().wrapping_add(obj as usize + SB_ID_OFF);
    unsafe { std::ptr::read_unaligned(p as *const u32) as usize }
}

/// 分配一个 StringBuilder 对象（字段 0 = 新的缓冲编号）
pub extern "C" fn sb_new() -> i64 {
    let tag = class_tag("java/lang/StringBuilder");
    let cid = register_class(tag, "java/lang/StringBuilder", &["I".to_string()]);
    let id = {
        let mut g = SB_BUFS.lock().unwrap();
        g.push(Vec::new());
        (g.len() - 1) as u32
    };
    let obj = new_object(cid);
    let p = base_ptr().wrapping_add(obj as usize + SB_ID_OFF);
    unsafe { std::ptr::write_unaligned(p as *mut u32, id) };
    obj
}

fn sb_push(obj: i64, text: &[u8]) {
    let i = sb_buf_id(obj);
    let mut g = SB_BUFS.lock().unwrap();
    if g.len() <= i {
        g.resize(i + 1, Vec::new());
    }
    g[i].extend_from_slice(text);
}

pub extern "C" fn sb_append_int(obj: i64, v: i32) {
    sb_push(obj, v.to_string().as_bytes());
}

pub extern "C" fn sb_append_bool(obj: i64, v: i32) {
    sb_push(obj, if v != 0 { b"true" } else { b"false" });
}

pub extern "C" fn sb_append_long(obj: i64, v: i64) {
    sb_push(obj, v.to_string().as_bytes());
}

pub extern "C" fn sb_append_double(obj: i64, v: f64) {
    sb_push(obj, crate::java_double_to_string(v).as_bytes());
}

/// `append(float)`：十进制形态按 f32 规则（与 `println(float)` 同一套）
pub extern "C" fn sb_append_float(obj: i64, v: f32) {
    sb_push(obj, crate::java_float_to_string(v).as_bytes());
}

/// `append(char)`：按 UTF-8 字节接上（char 码点不是十进制数字）
pub extern "C" fn sb_append_char(obj: i64, v: i32) {
    let mut buf = [0u8; 4];
    let c = char::from_u32(v as u32).unwrap_or('?');
    sb_push(obj, c.encode_utf8(&mut buf).as_bytes());
}

/// 引用实参：null → "null"，String → 内容，包装类 → 值的文本，
/// 其它对象 → `Klass@hash`（与 `String.valueOf(Object)` 一致）
pub extern "C" fn sb_append_obj(obj: i64, v: i64) {
    let text = super::natives::obj_to_string(v);
    sb_push(obj, text.as_bytes());
}

/// `toString()`：缓冲 → 堆上的 String 对象（1 字节元素、不追踪引用）
pub extern "C" fn sb_to_string(obj: i64) -> i64 {
    let i = sb_buf_id(obj);
    // 取走缓冲（不清空就会随每次拼接一直涨）
    let bytes = {
        let mut g = SB_BUFS.lock().unwrap();
        g.get_mut(i).map(std::mem::take).unwrap_or_default()
    };
    let cid = string_class_id();
    let hd = new_array(cid, bytes.len() as u32);
    if !bytes.is_empty() {
        let p = base_ptr().wrapping_add(hd as u32 as usize + ARRAY_HEADER);
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len()) };
    }
    hd
}

// ---------- invokedynamic：字符串拼接（`StringConcatFactory`）----------
//
// JIT 不跑引导方法：`"a" + b` 编译出来的 `invokedynamic` 在**编译期**就被解码成一条
// 「拼接计划」（字面量 + 参数类型序列），运行时只走一次 `indy_concat`。
// 这样调用点缓存、`CallSite`、`MethodHandle` 都不必进机器码，而结果与
// `StringConcatFactory` 的语义一致（`\u0001` = 栈上的参数，`\u0002` = 静态常量）。

/// 拼接计划里的一段
#[derive(Clone, Copy)]
pub enum CPiece {
    /// 字面量：`strtab` 里的字符串编号
    Lit(u32),
    /// 栈上的动态参数（按 `K_*` 解码）
    Arg(u8),
}

/// 参数类型：与描述符字符一一对应（`I`/`Z`/`C`/`J`/`F`/`D`/引用）
pub const K_INT: u8 = 0;
pub const K_BOOL: u8 = 1;
pub const K_CHAR: u8 = 2;
pub const K_LONG: u8 = 3;
pub const K_FLOAT: u8 = 4;
pub const K_DOUBLE: u8 = 5;
pub const K_REF: u8 = 6;

static CONCAT_PLANS: Mutex<Vec<Vec<CPiece>>> = Mutex::new(Vec::new());

/// 编译期登记一条拼接计划，返回编号
pub fn register_concat(plan: Vec<CPiece>) -> u32 {
    let mut g = CONCAT_PLANS.lock().unwrap();
    g.push(plan);
    (g.len() - 1) as u32
}

/// 分配一个 String 对象（内容 = `bytes`）。**不登记永久根**：保活是调用方的事。
pub fn new_string(bytes: &[u8]) -> i64 {
    let cid = string_class_id();
    let hd = new_array(cid, bytes.len() as u32);
    if !bytes.is_empty() {
        let p = base_ptr().wrapping_add(hd as u32 as usize + ARRAY_HEADER);
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len()) };
    }
    hd
}

/// 运行时桩：按计划拼出字符串。
///
/// `vals` 是「每个动态参数一格」的 i64 数组（float/double 走位模式，和生成代码一致）。
/// 先把文本攒进 Rust 侧缓冲、最后一次性分配 —— 中途没有 GC 能移动源对象，
/// 所以参数里的引用不必额外保活。
pub extern "C" fn indy_concat(plan: u32, vals: *const i64) -> i64 {
    let pieces = match CONCAT_PLANS.lock().unwrap().get(plan as usize) {
        Some(p) => p.clone(),
        None => return 0,
    };
    let mut buf: Vec<u8> = Vec::new();
    let mut i = 0usize;
    for p in pieces {
        match p {
            CPiece::Lit(id) => buf.extend_from_slice(super::strtab::text(id).as_bytes()),
            CPiece::Arg(k) => {
                let raw = unsafe { std::ptr::read_unaligned(vals.add(i)) };
                i += 1;
                match k {
                    K_INT => buf.extend_from_slice((raw as i32).to_string().as_bytes()),
                    K_BOOL => buf.extend_from_slice(if raw as i32 != 0 { b"true" } else { b"false" }),
                    K_CHAR => {
                        let mut tmp = [0u8; 4];
                        let c = char::from_u32(raw as u32).unwrap_or('?');
                        buf.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                    }
                    K_LONG => buf.extend_from_slice(raw.to_string().as_bytes()),
                    K_FLOAT => buf.extend_from_slice(
                        crate::java_float_to_string(f32::from_bits(raw as u32)).as_bytes(),
                    ),
                    K_DOUBLE => buf.extend_from_slice(
                        crate::java_double_to_string(f64::from_bits(raw as u64)).as_bytes(),
                    ),
                    _ => buf.extend_from_slice(super::natives::obj_to_string(raw).as_bytes()),
                }
            }
        }
    }
    new_string(&buf)
}

/// 运行时桩：JIT 调用一个**只能解释执行**的方法（方法级回退的入口）。
///
/// `vals` 是「每个实参一格」的 i64 数组，返回值也是 i64 位模式（与解释器桥同一套 ABI）。
/// 实参个数由登记表里的描述符决定，桥自己按描述符读，所以这里不设上限。
pub extern "C" fn interp_call(id: u32, vals: *const i64) -> i64 {
    unsafe { super::super::vm::call_interpreted_ptr(id, vals) }
}
