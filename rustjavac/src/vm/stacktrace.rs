//! 每线程**帧链**与栈回溯：`Throwable` / `Thread.getStackTrace` / `StackWalker` /
//! `jstack` 风格死锁报告的唯一帧来源。
//!
//! ## 为什么不是「把 `Frame` 串成链表」
//!
//! 解释器的 `Frame` 活在 Rust 栈上（`exec::execute_body` 的局部变量）。把它的地址串成
//! `prev` 指针当然能走通，但**别的线程**读这条链时代码已经返回过——指针指向的是已被
//! 复用的栈内存，读到什么全凭运气。而这里有两个硬需求：park 住的线程要能被别人看、
//! 死锁报告要从外部读整条链。
//!
//! 所以帧的可读副本放在**每线程一条、与执行栈生命周期无关**的定长表里：
//!
//! ```text
//! FrameStack (Arc，存在 sh.threads[tid].frames 里)
//!   seq    : 写者序列（seqlock，奇数 = 正在改）
//!   depth  : 当前帧数
//!   slots  : [ { meta: *const FrameMeta, pc: usize }; CAP ]
//! ```
//!
//! 写者是**本线程**（每方法进出各一次 push/pop，全程只有原子量，没有锁）；读者可以是
//! 任何线程，靠 seqlock 拿一致快照（写窗口只有几条指令，重试一两次就过）。
//!
//! `FrameMeta`（类名 / 方法名 / 描述符 / 源文件名 / LineNumberTable）按
//! `(ClassFile 指针, 方法下标)` 全局 intern 一次，**泄漏、永不释放**——于是帧槽里只存
//! 一个指针，读侧解引用永远安全（intern 表里同时留着 `Arc<ClassFile>`，类文件不会被
//! 释放、指针不会被复用，和 `sh.refmaps` 是同一套理由）。
//!
//! ## 行号
//!
//! `pc` 只在**调用点 / athrow / safepoint** 同步进帧槽（不是每条字节码）。这正好覆盖
//! 所有需要行号的场景：异常栈的每一帧都停在「它调用下一层的那个 invoke 指令」上，行号
//! 就是源码里那个调用所在行——与 HotSpot 一致。

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use super::frame::Frame;
use super::native_reg::NativeRegistry;
use super::slot::Slot;
use super::{Vm, VmError};
use crate::ClassFile;

/// 每线程帧表容量。
///
/// 递归能有多深由**线程栈**决定（见 `thread::stack_exhausted`，默认 24 MB 栈 →
/// 约 8000 层），所以这张表要装得下「一次真实的深递归」，否则栈轨迹会在深处被截断。
/// 1024 是 HotSpot 的默认口径（`MaxJavaStackTraceDepth`），也够覆盖任何一次异常栈；
/// 更深的递归由 [`FrameStack::push`] 的溢出计数兜住（见那里的注释）。
pub(crate) const CAP: usize = 1024;

// ================================================================ 帧元数据（intern）

/// 一帧里**不变**的部分（按 (类, 方法) intern 一份，所有线程共享）。
pub(crate) struct FrameMeta {
    /// 内部类名（`java/lang/Thread`）
    pub class: Arc<str>,
    pub method: Arc<str>,
    pub desc: Arc<str>,
    /// `SourceFile` 属性（没有就 None）
    pub source: Option<Arc<str>>,
    /// `LineNumberTable`：`(start_pc, line)`，按 start_pc 升序
    pub lines: Vec<(u32, u32)>,
    /// 类所在的模块（只有 JDK 类有；写进 `StackTraceElement` 的 moduleName）
    pub module: Option<&'static str>,
    /// 带 `jdk.internal.vm.annotation.Hidden`：不进栈轨迹（HotSpot 的规矩）
    pub hidden: bool,
}

impl FrameMeta {
    /// pc 对应的源码行号；找不到 = -1（HotSpot 的 Unknown line）
    pub fn line_at(&self, pc: usize) -> i32 {
        let pc = pc as u32;
        let mut line = -1i32;
        for (start, l) in &self.lines {
            if *start <= pc {
                line = *l as i32;
            } else {
                break;
            }
        }
        line
    }
}

type MetaKey = (usize, u32);
type MetaVal = (Arc<ClassFile>, &'static FrameMeta);

fn meta_table() -> &'static RwLock<HashMap<MetaKey, MetaVal>> {
    static T: OnceLock<RwLock<HashMap<MetaKey, MetaVal>>> = OnceLock::new();
    T.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 每线程 8 条直接映射缓存：递归/重复调用链在热路径上**完全不碰锁**。
/// 命中安全性：intern 表长期持有 `Arc<ClassFile>`，指针不会被复用（见模块注释）。
#[derive(Clone, Copy)]
struct MetaCacheEntry {
    cf: usize,
    method: u32,
    meta: *const FrameMeta,
}

thread_local! {
    static META_CACHE: Cell<[MetaCacheEntry; 8]> = const {
        Cell::new([MetaCacheEntry { cf: 0, method: 0, meta: std::ptr::null() }; 8])
    };
}

fn cache_slot(cf: usize, method: u32) -> usize {
    ((cf >> 4) ^ (method as usize).wrapping_mul(0x9E37_79B9)) & 7
}

fn meta_of(cf: &Arc<ClassFile>, method: usize) -> &'static FrameMeta {
    let key = (Arc::as_ptr(cf) as usize, method as u32);
    let slot = cache_slot(key.0, key.1);
    let hit = META_CACHE.with(|c| {
        let e = c.get()[slot];
        if e.cf == key.0 && e.method == key.1 && !e.meta.is_null() {
            Some(e.meta)
        } else {
            None
        }
    });
    if let Some(p) = hit {
        // SAFETY: intern 表里的 FrameMeta 是泄漏的、永不释放（见 meta_table 的生命周期）。
        return unsafe { &*p };
    }
    let mut g = meta_table().write().unwrap_or_else(|e| e.into_inner());
    let meta = match g.get(&key) {
        Some((_, m)) => *m,
        None => {
            let m: &'static FrameMeta = Box::leak(Box::new(build_meta(cf, method)));
            // 同时钉住 ClassFile：指针不复用 → 缓存/槽里的指针永远指向同一份元数据
            g.insert(key, (Arc::clone(cf), m));
            m
        }
    };
    drop(g);
    META_CACHE.with(|c| {
        let mut arr = c.get();
        arr[slot] = MetaCacheEntry { cf: key.0, method: key.1, meta };
        c.set(arr);
    });
    meta
}

/// 从 ClassFile 里取出帧元数据（类名 / 方法名 / 描述符 / SourceFile / LineNumberTable）
fn build_meta(cf: &ClassFile, method: usize) -> FrameMeta {
    let m = &cf.methods[method];
    let name = |i: u16| -> String {
        cf.utf8(i).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default()
    };
    let class: Arc<str> = Arc::from(cf.this_class_name().unwrap_or_default().as_str());
    let source = attr_body(cf, &cf.attributes, "SourceFile").and_then(|b| {
        if b.len() >= 2 {
            Some(Arc::from(name(u16::from_be_bytes([b[0], b[1]])).as_str()))
        } else {
            None
        }
    });
    let mut lines = Vec::new();
    if let Some(code) = attr_body(cf, &m.attributes, "Code") {
        if let Some(lnt) = code_attr(code, cf, "LineNumberTable") {
            let n = if lnt.len() >= 2 { u16::from_be_bytes([lnt[0], lnt[1]]) as usize } else { 0 };
            let mut p = 2usize;
            for _ in 0..n {
                if p + 4 > lnt.len() {
                    break;
                }
                let start = u16::from_be_bytes([lnt[p], lnt[p + 1]]) as u32;
                let line = u16::from_be_bytes([lnt[p + 2], lnt[p + 3]]) as u32;
                lines.push((start, line));
                p += 4;
            }
            lines.sort_unstable_by_key(|e| e.0);
        }
    }
    let module = module_of(&class);
    let hidden = has_hidden_annotation(cf, &m.attributes);
    FrameMeta {
        class,
        method: Arc::from(name(m.name_index).as_str()),
        desc: Arc::from(name(m.descriptor_index).as_str()),
        source,
        lines,
        module,
        hidden,
    }
}

fn attr_body<'a>(cf: &ClassFile, attrs: &'a [crate::Attribute], want: &str) -> Option<&'a [u8]> {
    for a in attrs {
        if cf.utf8(a.name_index).map(|b| b == want.as_bytes()).unwrap_or(false) {
            return Some(&a.info);
        }
    }
    None
}

/// `Code` 属性体里再找子属性（`LineNumberTable` 等）：`u2 max_stack, u2 max_locals, u4 code_len, code, ...`
/// 方法上有没有 `@jdk.internal.vm.annotation.Hidden` —— HotSpot 用它把「内联壳」藏出
/// 栈轨迹（`Thread.runWith` 就是：`run()` 里包一层 scoped-value 绑定）。
/// 不过滤的话，每个子线程的栈轨迹都会多出一帧，与真 java 对不上。
///
/// 只读 `RuntimeVisibleAnnotations` 的**顶层**注解类型；参数值按 element_value 的
/// 结构跳（不能按字节扫，常量池下标随机命中）。
fn has_hidden_annotation(cf: &ClassFile, attrs: &[crate::Attribute]) -> bool {
    let Some(body) = attr_body(cf, attrs, "RuntimeVisibleAnnotations") else { return false };
    let want = b"Ljdk/internal/vm/annotation/Hidden;";
    let Some(n) = read_u2(body, 0) else { return false };
    let mut p = 2usize;
    for _ in 0..n {
        let (Some(ty), Some(np)) = (read_u2(body, p), read_u2(body, p + 2)) else { return false };
        if cf.utf8(ty).map(|b| b == want).unwrap_or(false) {
            return true;
        }
        p += 4;
        for _ in 0..np {
            p += 2; // name_index
            p = match skip_element_value(body, p) {
                Some(q) => q,
                None => return false,
            };
        }
    }
    false
}

fn read_u2(b: &[u8], p: usize) -> Option<u16> {
    if p + 2 <= b.len() {
        Some(u16::from_be_bytes([b[p], b[p + 1]]))
    } else {
        None
    }
}

/// 跳过一段 `element_value`（JVMS 4.7.16.1）
fn skip_element_value(b: &[u8], p: usize) -> Option<usize> {
    let tag = *b.get(p)? as char;
    let p = p + 1;
    match tag {
        'B' | 'C' | 'D' | 'F' | 'I' | 'J' | 'S' | 'Z' | 's' | 'c' => {
            (p + 2 <= b.len()).then(|| p + 2)
        }
        'e' => (p + 4 <= b.len()).then(|| p + 4),
        '@' => {
            let np = read_u2(b, p)? as usize;
            let mut q = p + 4;
            for _ in 0..np {
                q += 2;
                q = skip_element_value(b, q)?;
            }
            Some(q)
        }
        '[' => {
            let n = read_u2(b, p)? as usize;
            let mut q = p + 2;
            for _ in 0..n {
                q = skip_element_value(b, q)?;
            }
            Some(q)
        }
        _ => None,
    }
}

fn code_attr<'a>(code: &'a [u8], cf: &ClassFile, want: &str) -> Option<&'a [u8]> {
    if code.len() < 8 {
        return None;
    }
    let code_len = u32::from_be_bytes([code[4], code[5], code[6], code[7]]) as usize;
    let mut p = 8 + code_len;
    if p + 2 > code.len() {
        return None;
    }
    let exc_n = u16::from_be_bytes([code[p], code[p + 1]]) as usize;
    p += 2 + exc_n * 8;
    if p + 2 > code.len() {
        return None;
    }
    let attr_n = u16::from_be_bytes([code[p], code[p + 1]]) as usize;
    p += 2;
    for _ in 0..attr_n {
        if p + 6 > code.len() {
            return None;
        }
        let ni = u16::from_be_bytes([code[p], code[p + 1]]);
        let len = u32::from_be_bytes([code[p + 2], code[p + 3], code[p + 4], code[p + 5]]) as usize;
        p += 6;
        if p + len > code.len() {
            return None;
        }
        if cf.utf8(ni).map(|b| b == want.as_bytes()).unwrap_or(false) {
            return Some(&code[p..p + len]);
        }
        p += len;
    }
    None
}

/// 类属于哪个模块（写进 `StackTraceElement` 的 moduleName）。
///
/// 真 JVM 从类的 module 拿；我们的 `ClassStore` 没有记录「这个类是从镜像的哪个模块
/// 读来的」，所以按**引导层命名约定**给：`java.*` / `jdk.internal.*` / `sun.*` 在
/// `java.base`，其余一律 None（= 无名模块，`StackTraceElement.toString` 不打印前缀，
/// 与用户类在真 JVM 下的表现一致）。
///
/// 这是**展示层**的近似：只影响 `toString` 的前缀，不影响 `getClassName()` /
/// `getFileName()` / `getLineNumber()` 这些契约方法。
fn module_of(class: &str) -> Option<&'static str> {
    if class.starts_with("java/") || class.starts_with("jdk/internal/") || class.starts_with("sun/") {
        Some("java.base")
    } else if class.starts_with("javax/") {
        Some("java.base") // javac 常用的 javax.crypto / javax.net 都在 java.base
    } else {
        None
    }
}

// ================================================================ 每线程帧链

struct FrameSlot {
    meta: AtomicUsize,
    pc: AtomicUsize,
}

/// 一条线程的帧表（见模块注释）。`Arc` 由 `sh.threads[tid].frames` 持有。
pub(crate) struct FrameStack {
    pub tid: u64,
    /// seqlock：奇数 = 写者正在改
    seq: AtomicU64,
    depth: AtomicU32,
    /// 因为表满而被丢掉的帧数。
    ///
    /// 必须有它：`push` 在表满时会丢弃最里面的帧，但对应的 `pop` 照样会来 ——
    /// 只减 `depth` 而不记这笔账，表的深度就会和真实调用深度**永久错位**，
    /// 之后所有栈轨迹整体偏一截（这在 64 层上限的年代看不出来，深度放开到
    /// 8000 之后任何深递归都会踩到）。
    skipped: AtomicU32,
    slots: Box<[FrameSlot]>,
}

impl FrameStack {
    /// 新建一条线程的帧表（`thread_register` 调一次）
    pub(crate) fn new_for(tid: u64) -> FrameStack {
        FrameStack {
            tid,
            seq: AtomicU64::new(0),
            depth: AtomicU32::new(0),
            skipped: AtomicU32::new(0),
            slots: (0..CAP)
                .map(|_| FrameSlot { meta: AtomicUsize::new(0), pc: AtomicUsize::new(0) })
                .collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.depth.load(Ordering::Acquire) as usize
    }

    fn push(&self, meta: &'static FrameMeta, pc: usize) {
        let d = self.depth.load(Ordering::Relaxed) as usize;
        if d >= self.slots.len() {
            // 表满：丢最里面的帧（栈轨迹少几帧，好过越界），但要记下这笔账 ——
            // 对应的 `pop` 还会来，不清账就会把 `depth` 减穿，之后所有栈轨迹错位。
            self.skipped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.seq.fetch_add(1, Ordering::Acquire);
        self.slots[d].meta.store(meta as *const FrameMeta as usize, Ordering::Relaxed);
        self.slots[d].pc.store(pc, Ordering::Relaxed);
        self.depth.store(d as u32 + 1, Ordering::Release);
        self.seq.fetch_add(1, Ordering::Release);
    }

    fn pop(&self) {
        // 先还「被丢掉的那些帧」的账：它们没进 `slots`，只能抵掉计数
        if self.skipped.load(Ordering::Relaxed) > 0 {
            self.skipped.fetch_sub(1, Ordering::Relaxed);
            return;
        }
        let d = self.depth.load(Ordering::Relaxed) as usize;
        if d == 0 {
            return;
        }
        self.seq.fetch_add(1, Ordering::Acquire);
        self.depth.store(d as u32 - 1, Ordering::Release);
        self.seq.fetch_add(1, Ordering::Release);
    }

    /// 顶帧的 pc（**不**进 seqlock：读到旧值只等于行号略旧，不会读到坏指针）
    fn set_pc(&self, pc: usize) {
        let d = self.depth.load(Ordering::Relaxed) as usize;
        if d > 0 {
            self.slots[d - 1].pc.store(pc, Ordering::Relaxed);
        }
    }

    /// 一致的快照（seqlock）。返回 `(元数据, pc)`，从栈顶到栈底。
    fn snapshot(&self) -> Vec<(&'static FrameMeta, usize)> {
        let mut spins = 0u32;
        loop {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 == 0 {
                let d = self.depth.load(Ordering::Acquire) as usize;
                let mut v = Vec::with_capacity(d);
                // 槽 0 是**栈底**（最先压进来的）：对外要栈顶在前，所以倒着走
                for i in (0..d).rev() {
                    let m = self.slots[i].meta.load(Ordering::Relaxed) as *const FrameMeta;
                    if m.is_null() {
                        break;
                    }
                    // SAFETY: 槽里的指针来自 intern 表，永不释放（见模块注释）
                    v.push((unsafe { &*m }, self.slots[i].pc.load(Ordering::Relaxed)));
                }
                if self.seq.load(Ordering::Acquire) == s1 {
                    return v;
                }
            }
            spins += 1;
            if spins < 1000 {
                std::hint::spin_loop();
            } else {
                // 写者一直在进出方法（递归 + 别的线程在读）：让一下再试，不做有损读
                std::thread::yield_now();
            }
        }
    }
}

thread_local! {
    /// 本 OS 线程当前对应的帧表（`thread_register` / `Thread.start` 时绑定）。
    /// 裸指针的安全性：`Arc` 由 `sh.threads` 长期持有（线程记录永不删除），见 `bind_current`。
    static CUR_FS: Cell<*const FrameStack> = const { Cell::new(std::ptr::null()) };
}

/// 把**当前 OS 线程**绑到某个 Java 线程的帧表上（线程登记时调一次）。
///
/// 必须在 `sh.threads` 里已经有这个 tid 的记录之后调：帧表的所有权在那里。
pub(crate) fn bind_current(sh: &Arc<super::shared::Shared>, tid: u64) {
    let fs = super::shared::lk(&sh.threads).get(&tid).map(|t| Arc::clone(&t.frames));
    if let Some(fs) = fs {
        CUR_FS.with(|c| c.set(Arc::as_ptr(&fs) as *const FrameStack));
        // 故意泄漏一份 Arc 计数：帧表要和「这条 OS 线程」同寿命，而线程结束时没有
        // 一个可靠的析构点（`thread_finish` 之后还可能被诊断代码读）。记录本身也在
        // sh.threads 里，这里多一份只是双保险。
        std::mem::forget(fs);
    }
}

#[inline]
fn current_fs() -> Option<&'static FrameStack> {
    let p = CUR_FS.with(|c| c.get());
    if p.is_null() {
        None
    } else {
        // SAFETY: 由 bind_current 绑定的 FrameStack 由 sh.threads 与泄漏的 Arc 双重持有，
        // 进程生命周期内不会释放。
        Some(unsafe { &*p })
    }
}

/// 某个 tid 的帧表（park 住的线程也能读 —— 这就是它不在 TLS 里独占的原因）
pub(crate) fn stack_of(vm: &Vm, tid: u64) -> Option<Arc<FrameStack>> {
    // 统一走线程表：当前线程也在表里，拿到的就是同一份 Arc（自读不特殊化，少一条分支）
    super::shared::lk(&vm.sh.threads).get(&tid).map(|t| Arc::clone(&t.frames))
}

// ================================================================ 执行侧钩子

/// 进方法时压帧；Drop 时弹。**每个被解释的方法一进一出各一次**，只有原子量。
pub(crate) struct FrameMark {
    on: bool,
}

impl Drop for FrameMark {
    fn drop(&mut self) {
        if self.on {
            if let Some(fs) = current_fs() {
                fs.pop();
            }
        }
    }
}

/// `execute_body` 进方法时调：登记一帧（`pc` 先给 0，调用点会同步）
///
/// 热路径：一次 TLS 读 + 一次（多数时候命中的）元数据查找 + 4 条原子操作，
/// **没有锁、没有分配**。
pub(crate) fn enter(cf: &Arc<ClassFile>, method: usize) -> FrameMark {
    match current_fs() {
        Some(fs) => {
            fs.push(meta_of(cf, method), 0);
            FrameMark { on: true }
        }
        None => FrameMark { on: false },
    }
}

/// 同步**当前帧**的 pc（调用点 / athrow / safepoint 调）
#[inline]
pub(crate) fn set_pc(pc: usize) {
    if let Some(fs) = current_fs() {
        fs.set_pc(pc);
    }
}

// ================================================================ 对外读帧

/// 一帧的只读视图（诊断 / 造对象共用）
#[derive(Clone, Debug)]
pub(crate) struct FrameInfo {
    pub class: String,
    pub method: String,
    pub desc: String,
    pub source: Option<String>,
    pub line: i32,
    pub pc: usize,
    pub module: Option<&'static str>,
}

/// 读 tid 的整条帧链（栈顶在前）。任何线程、任何时刻都能读。
pub(crate) fn frames_of(vm: &Vm, tid: u64) -> Vec<FrameInfo> {
    let Some(fs) = stack_of(vm, tid) else { return Vec::new() };
    infos_of(&fs)
}

/// 当前线程的帧链（native 里用；不查线程表）
/// 帧表快照 → `FrameInfo` 列表（栈顶在前）。
///
/// `` 标注的方法（`jdk.internal.vm.annotation.Hidden`）按 HotSpot 的规矩
/// **不进栈轨迹** —— 典型是 `Thread.runWith`：它只是 `run()` 的内联壳，真 JVM 的
/// `getStackTrace` 里看不到它，不滤掉就会多出一帧。
fn infos_of(fs: &FrameStack) -> Vec<FrameInfo> {
    fs.snapshot()
        .into_iter()
        .filter(|(m, _)| !m.hidden)
        .map(|(m, pc)| FrameInfo {
            class: m.class.to_string(),
            method: m.method.to_string(),
            desc: m.desc.to_string(),
            source: m.source.as_ref().map(|s| s.to_string()),
            line: m.line_at(pc),
            pc,
            module: m.module,
        })
        .collect()
}

/// 当前线程的帧链（native 里用；不查线程表）
pub(crate) fn frames_current() -> Vec<FrameInfo> {
    match current_fs() {
        Some(fs) => infos_of(fs),
        None => Vec::new(),
    }
}

// ================================================================ 造 Java 对象

/// `java.lang.StackTraceElement` 的 7 参构造器。
///
/// `(classLoaderName, moduleName, moduleVersion, declaringClass, methodName, fileName, lineNumber)`
const STE_CTOR: &str = "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;I)V";

/// 把一条 `FrameInfo` 造成 `StackTraceElement` 对象（失败 → 0）
fn make_element(vm: &mut Vm, f: &FrameInfo) -> i32 {
    let class = vm.str_new(f.class.replace('/', "."));
    let method = vm.str_new(f.method.clone());
    let file = match &f.source {
        Some(s) => vm.str_new(s.clone()),
        None => 0,
    };
    let module = match f.module {
        Some(m) => vm.str_new(m.to_string()),
        None => 0,
    };
    let args = [
        Slot::I(0),      // classLoaderName
        Slot::I(module), // moduleName
        Slot::I(0),      // moduleVersion
        Slot::I(class),
        Slot::I(method),
        Slot::I(file),
        Slot::I(f.line),
    ];
    vm.new_instance_args("java/lang/StackTraceElement", STE_CTOR, &args).unwrap_or(0)
}

/// 把帧列表造成 `StackTraceElement[]`（空列表也返回一个长度 0 的数组，不是 null）
pub(crate) fn elements_for(vm: &mut Vm, frames: &[FrameInfo]) -> i32 {
    let mut slots = Vec::with_capacity(frames.len());
    for f in frames {
        let h = make_element(vm, f);
        slots.push(Slot::I(h));
    }
    vm.sh.heap.new_array(slots, "[Ljava/lang/StackTraceElement;").unwrap_or(0)
}

/// 某个 tid 的 `StackTraceElement[]`（`skip` = 从栈顶跳过多少帧）
pub(crate) fn to_stack_trace_elements(vm: &mut Vm, tid: u64, skip: usize) -> i32 {
    let frames = frames_of(vm, tid);
    let frames = if skip < frames.len() { &frames[skip..] } else { &[][..] };
    elements_for(vm, frames)
}

// ================================================================ native：Throwable

/// `Throwable.getOurStackTrace()Ljava/lang/StackTraceElement[];`
///
/// JDK 自己的实现会把 `backtrace` 再过一遍 `StackTraceElement.of(...)`，而 `of()`
/// 会对**每个元素**调 `computeFormat()` —— 那里面第一行是
/// `declaringClassObject.getClassLoader0()`。我们造的元素来自公开的 7 参构造器
/// （按 JDK 语义 `declaringClassObject` 就是 null），于是**每打一次异常栈就 NPE**，
/// 而且这个 NPE 会把真正的异常整个盖掉（`D2` 就是这样：`new JFrame` 的真错看不到）。
///
/// 我们的 `backtrace` 本来就是造好的 `StackTraceElement[]`（`throwable_fill_in`
/// 按精确帧算出来的），直接返回即可 —— 不需要那趟再加工，也绕开了 `Module` 那套。
fn throwable_get_our_stack_trace(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let bt = vm.get_named(this, "java/lang/Throwable", "backtrace").as_i32();
    if bt != 0 {
        return Ok(Slot::I(bt));
    }
    // 没有 backtrace（VM 直接造的异常，没走过 `fillInStackTrace`）→ 空数组，
    // 绝不返回 null：JDK 的 `getStackTrace()` 直接用这个返回值，null 会炸在别人那
    let empty = vm
        .sh
        .heap
        .new_array(Vec::new(), "[Ljava/lang/StackTraceElement;")
        .unwrap_or(0);
    Ok(Slot::I(empty))
}

/// Throwable 自己的这几帧不能出现在栈轨迹里（HotSpot 的 `fill_in_stack_trace` 同样跳过）。
///
/// `fillInStackTrace` 这一条**不看类名**：JDK 里每个异常类都覆写了它
/// （`NullPointerException.fillInStackTrace(NullPointerException.java:90)` 那种），
/// 它们全在「这个异常自己的构造链」上 —— 只看 `java/lang/Throwable` 会漏掉一整串，
/// 栈顶就变成了 `NullPointerException.fillInStackTrace` 而不是真正的抛出点。
fn is_throwable_internal(f: &FrameInfo) -> bool {
    if f.method == "fillInStackTrace" {
        return true;
    }
    f.class == "java/lang/Throwable" && f.method == "<init>"
}

/// `Throwable.fillInStackTrace(I)Ljava/lang/Throwable;`
///
/// JDK 25 的 `Throwable.fillInStackTrace()` 是 Java 方法，它调这个 native，然后
/// Java 侧把 `stackTrace` 复位成 `UNASSIGNED_STACK`。`getOurStackTrace()` 再用
/// `StackTraceElement.of(backtrace, depth)` 把 backtrace 变成数组。
///
/// 我们直接把**造好的 `StackTraceElement[]`** 存进 `backtrace` 字段（它是 `Object`），
/// `depth` 存长度 —— `initStackTraceElements` 只做一次拷贝。这样 backtrace 是 GC 管
/// 的普通对象，不存在「VM 侧表漏条目」的问题。
fn throwable_fill_in(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    let mut frames = frames_current();
    // 栈顶要跳过的是「**这个异常自己的**构造链」：`Throwable.fillInStackTrace` 本身，
    // 以及 `this` 的运行时类到 `java.lang.Throwable` 之间那些 `<init>`。
    //
    // 判据不能写成「所有 `<init>` 都跳」—— 在构造器里 new 一个异常时，那个构造器帧
    // 是**合法栈顶**（HotSpot 实测：`class A { A(){ new RuntimeException(); } }` 的栈顶
    // 就是 `A.<init>`；而 `class D extends RuntimeException { D(){ new RuntimeException(); } }`
    // 的栈顶也是 `D.<init>`，因为 D 不是 RuntimeException 的祖先）。所以判据是
    // 「throwable_cls 是这一帧的类的子类」。
    let throwable_cls = vm.class_of(this).unwrap_or_else(|| "java/lang/Throwable".to_string());
    let mut skip = 0usize;
    while skip < frames.len() {
        let fr = &frames[skip];
        let own_ctor = fr.method == "<init>" && vm.is_subclass(&throwable_cls, &fr.class);
        if is_throwable_internal(fr) || own_ctor {
            skip += 1;
        } else {
            break;
        }
    }
    frames.drain(..skip);
    // 未捕获异常要打的栈就在这里定下来：此刻正是 `fillInStackTrace` 的时机
    // （= HotSpot 取栈轨迹的同一时刻），再晚帧就弹空了。
    super::exception::note_frames(this, frames.clone());
    let arr = elements_for(vm, &frames);
    vm.put_named(this, "java/lang/Throwable", "backtrace", Slot::I(arr));
    vm.put_named(this, "java/lang/Throwable", "depth", Slot::I(frames.len() as i32));
    Ok(Slot::I(this))
}

/// `StackTraceElement.initStackTraceElements([Ljava/lang/StackTraceElement;Ljava/lang/Object;I)V`
///
/// `backtrace` 是我们自己造的 `StackTraceElement[]`：把元素逐个搬进 JDK 分配好的数组。
fn ste_init_elements(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let arr = f.arg_i(0);
    let bt = f.arg_i(1);
    let depth = f.arg_i(2).max(0) as usize;
    if arr == 0 || bt == 0 {
        return Ok(Slot::I(0));
    }
    let n = depth.min(vm.sh.heap.array_len(arr).unwrap_or(0));
    for i in 0..n {
        if let Some(v) = vm.sh.heap.get(bt, i) {
            vm.sh.heap.set(arr, i, v);
        }
    }
    Ok(Slot::I(0))
}

// ================================================================ native：Thread

/// `Thread.getStackTrace0()Ljava/lang/Object;`
///
/// `Thread.getStackTrace()`（JDK 真字节码）对**别的线程**走这条路，返回
/// `StackTraceElement[]` 或 null（null → `EMPTY_STACK_TRACE`）。
fn thread_get_stack_trace0(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let target = f.this();
    let Some(tid) = thread_tid_of(vm, target) else { return Ok(Slot::I(0)) };
    let frames = frames_of(vm, tid);
    if frames.is_empty() {
        return Ok(Slot::I(0));
    }
    Ok(Slot::I(elements_for(vm, &frames)))
}

/// Thread 对象句柄 → tid
pub(crate) fn thread_tid_of(vm: &Vm, thread_obj: i32) -> Option<u64> {
    if thread_obj == 0 {
        return None;
    }
    super::shared::lk(&vm.sh.threads)
        .values()
        .find(|t| t.java == thread_obj)
        .map(|t| t.tid)
}

/// `Thread.dumpThreads([Ljava/lang/Thread;)[[Ljava/lang/StackTraceElement;`
fn thread_dump_threads(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let arr = f.arg_i(0);
    let n = if arr == 0 { 0 } else { vm.sh.heap.array_len(arr).unwrap_or(0) };
    let mut rows = Vec::with_capacity(n);
    for i in 0..n {
        let t = vm.sh.heap.get(arr, i).map(|s| s.as_i32()).unwrap_or(0);
        let elems = match thread_tid_of(vm, t) {
            Some(tid) => {
                let frames = frames_of(vm, tid);
                if frames.is_empty() {
                    0
                } else {
                    elements_for(vm, &frames)
                }
            }
            None => 0,
        };
        rows.push(Slot::I(elems));
    }
    let h = vm.sh.heap.new_array(rows, "[[Ljava/lang/StackTraceElement;").unwrap_or(0);
    Ok(Slot::I(h))
}

/// `Thread.getThreads()[Ljava/lang/Thread;`：线程表里所有有 Java 对象的线程
fn thread_get_threads(vm: &mut Vm, _f: &mut Frame) -> Result<Slot, VmError> {
    let mut objs: Vec<i32> = super::shared::lk(&vm.sh.threads)
        .values()
        .filter(|t| t.alive && t.java != 0)
        .map(|t| t.java)
        .collect();
    objs.sort_unstable();
    objs.dedup();
    let slots: Vec<Slot> = objs.into_iter().map(Slot::I).collect();
    Ok(Slot::I(vm.sh.heap.new_array(slots, "[Ljava/lang/Thread;").unwrap_or(0)))
}

// ================================================================ native：StackWalker
//
// 契约（JDK 25，`javap -p -c` 核对过）：
//   * `callStackWalk(mode, maxDepth, contScope, cont, startIndex, batchSize, frames)`
//     填第一批判 `frames[startIndex..]`，把续块游标写进 `anchor`，然后**回调**
//     `consumeFrames()`（Java 侧创建 Stream 并把用户函数跑完），返回它的结果。
//   * `fetchStackFrames(mode, anchor, startIndex, startFrame, batchSize, frames)`
//     续填下一批，返回填了几帧。
//   * `StackFrameInfo` 由 VM 填 `name` / `classOrMemberName` / `bci` / `ste`；
//     `expandStackFrameInfo()` 只在这些字段为 null 时才会被 Java 调（我们也实现，兜底）。
//
// 帧的筛选：真 JVM 的 walk 从**调用 `walk` 的那一帧**开始（不含 StackWalker 自己的
// 内部帧），所以跳过 `java/lang/StackWalker*` / `java/lang/StackStreamFactory*`。

fn is_walker_internal(f: &FrameInfo) -> bool {
    f.class.starts_with("java/lang/StackWalker")
        || f.class.starts_with("java/lang/StackStreamFactory")
        || f.class.starts_with("java/lang/StackFrameInfo")
}

/// 一次 walk 的续块状态：`anchor → 快照 + 游标`
struct WalkState {
    frames: Vec<FrameInfo>,
    cursor: usize,
}

fn walk_table() -> &'static std::sync::Mutex<HashMap<i64, WalkState>> {
    static T: OnceLock<std::sync::Mutex<HashMap<i64, WalkState>>> = OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

static WALK_SEQ: AtomicU64 = AtomicU64::new(1);

/// `StackFrameInfo` 的字段填好一帧（`ste` 也预置好，`toStackTraceElement` 不再回头找 VM）
/// 给 `ClassFrameInfo.classOrMemberName` 造一个 `ResolvedMethodName`。
///
/// `StackFrameInfo.declaringClass()` 走 `JLIA.getDeclaringClass(Object)`，而那个实现
/// **强转 `ResolvedMethodName`**（不是 Class！）。真正的 Class 挂在它的 `vmholder` 字段上。
/// 只有 `DROP_METHOD_INFO` 模式的 `ClassFrameInfo` 才直接放 Class —— 两条路的字段类型不同。
fn make_resolved_method_name(vm: &mut Vm, class: &str) -> i32 {
    let class_obj = vm.class_obj(class);
    let rmn = vm
        .new_instance_args("java/lang/invoke/ResolvedMethodName", "()V", &[])
        .unwrap_or(0);
    if rmn != 0 {
        vm.put_named(rmn, "java/lang/invoke/ResolvedMethodName", "vmholder", Slot::I(class_obj));
    }
    rmn
}

fn fill_stack_frame(vm: &mut Vm, sfi: i32, f: &FrameInfo) {
    let name = vm.str_new(f.method.clone());
    let rmn = make_resolved_method_name(vm, &f.class);
    let ste = make_element(vm, f);
    vm.put_named(sfi, "java/lang/StackFrameInfo", "name", Slot::I(name));
    vm.put_named(sfi, "java/lang/ClassFrameInfo", "classOrMemberName", Slot::I(rmn));
    // bci 槽存**行号**：`initStackTraceElement` 从这里取，StackFrame API 不暴露真实 bci
    vm.put_named(sfi, "java/lang/StackFrameInfo", "bci", Slot::I(f.line));
    vm.put_named(sfi, "java/lang/StackFrameInfo", "ste", Slot::I(ste));
}
/// 往 `frames[start..]` 里填最多 `count` 帧，返回填了几帧
fn fill_batch(vm: &mut Vm, walker: i32, frames: &[FrameInfo], from: usize, arr: i32, start: usize, count: usize) -> usize {
    let n = count.min(frames.len().saturating_sub(from));
    let walker_h = vm.get_named(walker, "java/lang/StackStreamFactory$AbstractStackWalker", "walker").as_i32();
    for i in 0..n {
        let sfi = vm
            .new_instance_args("java/lang/StackFrameInfo", "(Ljava/lang/StackWalker;)V", &[Slot::I(walker_h)])
            .unwrap_or(0);
        if sfi == 0 {
            break;
        }
        fill_stack_frame(vm, sfi, &frames[from + i]);
        vm.sh.heap.set(arr, start + i, Slot::I(sfi));
    }
    n
}

fn stack_call_walk(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    // 这是**实例** native：`this` 占槽 0，声明的参数从 `arg(0)` 数（`Frame::arg` 的约定）。
    // 描述符 `(IILjdk/internal/vm/ContinuationScope;Ljdk/internal/vm/Continuation;II[Ljava/lang/Object;)`
    // 的实参顺序（`javap -c beginStackWalk`）：mode, maxDepth, contScope, cont,
    // **currentBatchSize**, **startIndex(=1)**, frames —— 即 arg(4)=批量、arg(5)=数组起点。
    let this = f.this();
    let max_depth = f.arg_i(1).max(0) as usize;
    let start = f.arg_i(5).max(0) as usize;
    let batch = f.arg_i(4).max(0) as usize;
    let arr = f.arg_i(6);
    let mut frames = frames_current();
    frames.retain(|fr| !is_walker_internal(fr));
    if max_depth > 0 && frames.len() > max_depth {
        frames.truncate(max_depth);
    }
    let id = WALK_SEQ.fetch_add(1, Ordering::Relaxed) as i64;
    let n = fill_batch(vm, this, &frames, 0, arr, start, batch);
    // 第一批填完要**告诉 FrameBuffer**：Java 侧的 `fetchStackFrames()` 会在 native 之后
    // 自己调 `setBatch(depth, startIndex, 填了几帧)`，而 `callStackWalk` 是 native，
    // 这一步必须由我们做 —— 不做的话 buffer 的 origin/fence 还是 (1,0)，Stream 认为没帧。
    if n > 0 {
        let depth = vm.get_named(this, "java/lang/StackStreamFactory$AbstractStackWalker", "depth").as_i32();
        let fb = vm.get_named(this, "java/lang/StackStreamFactory$AbstractStackWalker", "frameBuffer").as_i32();
        if fb != 0 {
            if let Some((ci, mi)) = vm
                .store
                .resolve_method("java/lang/StackStreamFactory$FrameBuffer", "setBatch", "(III)V")
            {
                let args = [Slot::I(fb), Slot::I(depth), Slot::I(start as i32), Slot::I(n as i32)];
                let _ = vm.execute(ci, mi, "(III)V", &args, true);
            }
        }
    }
    vm.put_named(this, "java/lang/StackStreamFactory$AbstractStackWalker", "anchor", Slot::L(id));
    walk_table().lock().unwrap_or_else(|e| e.into_inner()).insert(id, WalkState { frames, cursor: n });
    // 回调 consumeFrames()：Java 侧建 Stream、跑用户函数；期间可能再调 fetchStackFrames
    let cls = vm.obj_class_name(this).unwrap_or_else(|| "java/lang/Object".into());
    let target = vm.store.resolve_method(&cls, "consumeFrames", "()Ljava/lang/Object;")
        .or_else(|| vm.store.resolve_method("java/lang/StackStreamFactory$AbstractStackWalker", "consumeFrames", "()Ljava/lang/Object;"));
    let r = match target {
        Some((ci, mi)) => vm.execute(ci, mi, "()Ljava/lang/Object;", &[Slot::I(this)], true),
        None => Err(VmError { msg: "StackWalker: 找不到 consumeFrames()".into(), exc: None }),
    };
    walk_table().lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
    r
}

fn stack_fetch_frames(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    // 实例 native：arg(0)=mode, arg(1)=anchor(long), arg(2)=numFrames,
    // arg(3)=nextBatchSize, arg(4)=startIndex, arg(5)=frames（`javap -c fetchStackFrames`）
    let this = f.this();
    let anchor = f.arg_l(1);
    let start = f.arg_i(4).max(0) as usize;
    let count = f.arg_i(3).max(0) as usize;
    let arr = f.arg_i(5);
    let (from, snapshot) = {
        let mut g = walk_table().lock().unwrap_or_else(|e| e.into_inner());
        match g.get_mut(&anchor) {
            Some(st) => {
                let from = st.cursor;
                (from, st.frames[from.min(st.frames.len())..].to_vec())
            }
            None => return Ok(Slot::I(0)),
        }
    };
    let n = fill_batch(vm, this, &snapshot, 0, arr, start, count);
    if let Some(st) = walk_table().lock().unwrap_or_else(|e| e.into_inner()).get_mut(&anchor) {
        st.cursor = from + n;
    }
    Ok(Slot::I(n as i32))
}
/// `StackFrameInfo.expandStackFrameInfo()`：正常路径上字段已经填好（见 `fill_stack_frame`）。
/// 走到这里说明 `name` 为 null —— 保底给一个空串，绝不让 Java 侧断言炸。
fn stack_expand(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let this = f.this();
    if vm.get_named(this, "java/lang/StackFrameInfo", "name").as_i32() == 0 {
        let s = vm.str_new(String::new());
        vm.put_named(this, "java/lang/StackFrameInfo", "name", Slot::I(s));
    }
    Ok(Slot::I(0))
}

/// `StackTraceElement.initStackTraceElement(Ljava/lang/StackTraceElement;Ljava/lang/StackFrameInfo;)V`
fn ste_init_element(vm: &mut Vm, f: &mut Frame) -> Result<Slot, VmError> {
    let ste = f.arg_i(0);
    let sfi = f.arg_i(1);
    if ste == 0 || sfi == 0 {
        return Ok(Slot::I(0));
    }
    let name = vm.get_named(sfi, "java/lang/StackFrameInfo", "name").as_i32();
    if name == 0 {
        // 没填过：从 ste 自己（fill_stack_frame 预置的）复制不了，给个空方法名
        return Ok(Slot::I(0));
    }
    let method = vm.obj_to_string(name);
    let mut class_h = vm.get_named(sfi, "java/lang/ClassFrameInfo", "classOrMemberName").as_i32();
    // 这里可能是 `ResolvedMethodName`（StackFrameBuffer 的常规形态），也可能是类对象
    // （`DROP_METHOD_INFO` 的 ClassFrameBuffer）。两种都要能反查出**类名**。
    if class_h != 0 && vm.class_obj_name(class_h).is_none() {
        let holder = vm.get_named(class_h, "java/lang/invoke/ResolvedMethodName", "vmholder").as_i32();
        if holder != 0 {
            class_h = holder;
        }
    }
    let class = vm
        .class_obj_name(class_h)
        .or_else(|| vm.class_of(class_h))
        .map(|c| c.replace('/', "."))
        .unwrap_or_default();
    let ste_pre = vm.get_named(sfi, "java/lang/StackFrameInfo", "ste").as_i32();
    // 预置的 STE 里已经带了 file / line / module：直接按字段搬过去
    let file = if ste_pre != 0 {
        vm.get_named(ste_pre, "java/lang/StackTraceElement", "fileName").as_i32()
    } else {
        0
    };
    let module = if ste_pre != 0 {
        vm.get_named(ste_pre, "java/lang/StackTraceElement", "moduleName").as_i32()
    } else {
        0
    };
    let line = vm.get_named(sfi, "java/lang/StackFrameInfo", "bci").as_i32();
    let class_s = vm.str_new(class);
    let method_s = vm.str_new(method);
    vm.put_named(ste, "java/lang/StackTraceElement", "declaringClass", Slot::I(class_s));
    vm.put_named(ste, "java/lang/StackTraceElement", "methodName", Slot::I(method_s));
    vm.put_named(ste, "java/lang/StackTraceElement", "fileName", Slot::I(file));
    vm.put_named(ste, "java/lang/StackTraceElement", "moduleName", Slot::I(module));
    vm.put_named(ste, "java/lang/StackTraceElement", "lineNumber", Slot::I(line));
    Ok(Slot::I(0))
}

// ================================================================ 注册

/// 把这些 native / VM 接管塞进注册表。
///
/// **在 `register_all` 之后调**：注册表是 `HashMap::insert`，后登记的覆盖先登记的 ——
/// 于是这里顺带把 `nat_util.rs` 里那条「一条假帧」的 `Thread.getStackTrace` 接管
/// **删掉**，让 JDK 真字节码（`new Exception().getStackTrace()`）跑起来。
pub(crate) fn register(r: &mut NativeRegistry) {
    // 1) 旧的假帧实现全部退场
    r.remove_override("java/lang/Thread", "getStackTrace", "()[Ljava/lang/StackTraceElement;");
    r.remove("java/lang/Thread", "getStackTrace0", "()Ljava/lang/Object;");
    r.remove("java/lang/Thread", "dumpThreads", "([Ljava/lang/Thread;)[[Ljava/lang/StackTraceElement;");
    r.remove("java/lang/Thread", "getThreads", "()[Ljava/lang/Thread;");
    r.remove("java/lang/Throwable", "fillInStackTrace", "(I)Ljava/lang/Throwable;");
    r.remove("java/lang/Throwable", "getStackTraceDepth", "()I");
    r.remove("java/lang/StackTraceElement", "initStackTraceElements",
        "([Ljava/lang/StackTraceElement;Ljava/lang/Object;I)V");
    r.remove("java/lang/StackTraceElement", "initStackTraceElement",
        "(Ljava/lang/StackTraceElement;Ljava/lang/StackFrameInfo;)V");
    r.remove("java/lang/StackFrameInfo", "expandStackFrameInfo", "()V");
    r.remove("java/lang/StackStreamFactory$AbstractStackWalker", "callStackWalk",
        "(IILjdk/internal/vm/ContinuationScope;Ljdk/internal/vm/Continuation;II[Ljava/lang/Object;)Ljava/lang/Object;");
    r.remove("java/lang/StackStreamFactory$AbstractStackWalker", "fetchStackFrames",
        "(IJIII[Ljava/lang/Object;)I");

    // 2) 真实现
    r.add("java/lang/Throwable", "fillInStackTrace", "(I)Ljava/lang/Throwable;", throwable_fill_in);
    // 直接返回我们造好的 `backtrace`：JDK 自带的实现会再走一遍 `computeFormat()`，
    // 那一步在公开构造器造出来的元素上必 NPE（见 `throwable_get_our_stack_trace`）
    r.add_override(
        "java/lang/Throwable",
        "getOurStackTrace",
        "()[Ljava/lang/StackTraceElement;",
        throwable_get_our_stack_trace,
    );
    r.add("java/lang/StackTraceElement", "initStackTraceElements",
        "([Ljava/lang/StackTraceElement;Ljava/lang/Object;I)V", ste_init_elements);
    r.add("java/lang/StackTraceElement", "initStackTraceElement",
        "(Ljava/lang/StackTraceElement;Ljava/lang/StackFrameInfo;)V", ste_init_element);
    r.add("java/lang/Thread", "getStackTrace0", "()Ljava/lang/Object;", thread_get_stack_trace0);
    r.add("java/lang/Thread", "dumpThreads", "([Ljava/lang/Thread;)[[Ljava/lang/StackTraceElement;", thread_dump_threads);
    r.add("java/lang/Thread", "getThreads", "()[Ljava/lang/Thread;", thread_get_threads);
    r.add("java/lang/StackFrameInfo", "expandStackFrameInfo", "()V", stack_expand);
    r.add(
        "java/lang/StackStreamFactory$AbstractStackWalker",
        "callStackWalk",
        "(IILjdk/internal/vm/ContinuationScope;Ljdk/internal/vm/Continuation;II[Ljava/lang/Object;)Ljava/lang/Object;",
        stack_call_walk,
    );
    r.add(
        "java/lang/StackStreamFactory$AbstractStackWalker",
        "fetchStackFrames",
        "(IJIII[Ljava/lang/Object;)I",
        stack_fetch_frames,
    );
}

// ================================================================ 诊断：jstack 风格

/// 一条线程的栈顶若干帧，`jstack` 风格的一行一帧（`类.方法(文件:行)`）
pub(crate) fn stack_text(vm: &Vm, tid: u64, max: usize) -> Vec<String> {
    frames_of(vm, tid)
        .into_iter()
        .take(max)
        .map(|f| {
            let class = f.class.replace('/', ".");
            let method = f.method.clone();
            let file = f.source.clone().unwrap_or_else(|| "Unknown Source".to_string());
            let line = if f.line >= 0 { format!(":{}", f.line) } else { String::new() };
            format!("\tat {class}.{method}({file}{line})")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_lookup() {
        let m = FrameMeta {
            class: Arc::from("A"),
            method: Arc::from("m"),
            desc: Arc::from("()V"),
            source: None,
            lines: vec![(0, 3), (10, 7), (20, 9)],
            module: None,
            hidden: false,
        };
        assert_eq!(m.line_at(0), 3);
        assert_eq!(m.line_at(9), 3);
        assert_eq!(m.line_at(10), 7);
        assert_eq!(m.line_at(25), 9);
    }

    #[test]
    fn frame_stack_push_pop_snapshot() {
        let fs = FrameStack::new_for(7);
        let m: &'static FrameMeta = Box::leak(Box::new(FrameMeta {
            class: Arc::from("A"),
            method: Arc::from("m"),
            desc: Arc::from("()V"),
            source: None,
            lines: vec![],
            module: None,
            hidden: false,
        }));
        fs.push(m, 1);
        fs.push(m, 2);
        assert_eq!(fs.len(), 2);
        let s = fs.snapshot();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].1, 2); // 栈顶在前
        fs.set_pc(5);
        let s = fs.snapshot();
        assert_eq!(s[0].1, 5);
        fs.pop();
        assert_eq!(fs.len(), 1);
        fs.pop();
        assert_eq!(fs.len(), 0);
        fs.pop(); // 空弹不能下溢
        assert_eq!(fs.len(), 0);
    }
}
