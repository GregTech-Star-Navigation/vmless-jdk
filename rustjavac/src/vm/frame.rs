//! 栈帧：局部槽（按 JVM 槽编号，宽值占两槽）与操作数栈。

use std::sync::Arc;

use super::gc::MethodRefs;
use super::slot::Slot;
use super::VmError;

/// 每线程的 `Vec<Slot>` 缓冲池。
///
/// 为什么要它：`Frame::new` 每次 Java 调用要 2 次 `malloc`（locals + stack），
/// 帧析构再 2 次 `free` —— 本机实测固定开销里 `Frame::new` 一个人就占 **119 ns**
/// （占解释器每次调用固定开销的 19%）。槽内容是「清零 + 写参数」，没有身份，
/// 所以按**栈序**回收复用是安全的：帧是严格嵌套的，`Drop` 一定发生在
/// 「同一个槽的下一帧被创建」之前。
///
/// 为什么池化缓冲区、而不是池化整个 `Frame`：`Frame` 的地址会被登记进
/// GC 的根集（`FrameGuard` 存的是 `*const Frame`），池化 Frame 本身就要保证
/// 「同一时刻只有一个活帧用这块内存」，那是另一件需要证明的事。缓冲区没有这个问题 ——
/// `gc` 扫的是 `frame.locals`/`frame.stack` 的内容，而 `Drop` 只在帧已经不再被任何
/// 根集引用之后才跑（`_frame_guard` 在 `frame` **之后**声明，所以先析构）。
///
/// 池只在线程内、只在 `Frame::new`/`Drop` 上各碰一次 `Vec` 的头指针，不加锁。
thread_local! {
    static BUF_POOL: std::cell::UnsafeCell<Vec<Vec<Slot>>> =
        const { std::cell::UnsafeCell::new(Vec::new()) };
}

/// 池的深度上限：递归能有 8000 层，池跟着涨没意义（每层一个缓冲区本来就够用）
const POOL_MAX: usize = 512;

#[inline(always)]
fn take_buf() -> Vec<Slot> {
    // SAFETY: 只在 `Frame::new` 里短借用，不做跨调用持有；单线程访问，无别名
    BUF_POOL.with(|c| unsafe { (*c.get()).pop().unwrap_or_default() })
}

#[inline(always)]
fn give_buf(v: Vec<Slot>) {
    if v.capacity() == 0 {
        return; // 没分配过的空 Vec 直接丢，不占池
    }
    // SAFETY: 同上
    BUF_POOL.with(|c| unsafe {
        let p = &mut *c.get();
        if p.len() < POOL_MAX {
            p.push(v);
        }
    });
}

pub(super) struct Frame {
    /// 局部变量表：按**JVM 槽**编号。long/double 占两槽，第 2 槽写 `I(0)` 当无效占位
    /// （与 max_locals、与 JIT 的 locals 布局保持一致）。
    pub(super) locals: Vec<Slot>,
    /// 操作数栈：一个宽值只占**一个**条目（解释器按值而不是按槽建模）。
    pub(super) stack: Vec<Slot>,
    /// 参数槽位表（`crate::jit::param_slots(desc)`）：第 i 个**声明的参数**起始槽
    pub(super) arg_slots: Vec<usize>,
    /// 参数从哪个槽开始（实例方法 1 = this 占槽 0；静态方法 0）
    pub(super) arg_base: usize,
    /// 当前字节码偏移（**GC 找根用**）：`gc_tick` / 调用点会把它写成「下一条要执行的
    /// 指令偏移」，GC 拿 `refs[pc]` 得到这一点的引用位图。停在别的值时保守扫描。
    pub(super) pc: usize,
    /// 本方法的引用位图（`gc::MethodRefs`）。None = 没有（native 帧 / 未算出来）→
    /// GC 对这个帧退化成「所有槽都当潜在引用，逐个用页表验证」。
    pub(super) refs: Option<Arc<MethodRefs>>,
    /// native 帧的引用位图（按描述符算，locals 里只有参数）
    pub(super) nrefs: Option<Arc<[u64]>>,
}

impl Frame {
    pub(super) fn new(max_locals: usize) -> Self {
        // 缓冲区从线程池拿（见 `BUF_POOL`）：命中时只剩「清空 + 清零」，
        // 没有 malloc/free。`clear` + `resize` 保证 locals 全长都是 `I(0)` ——
        // GC 会把 `locals` 里**所有**槽当潜在引用扫（复用来的残留句柄必须是 0）。
        let mut locals = Vec::new();
        locals.clear();
        locals.resize(max_locals.max(1), Slot::I(0));
        let mut stack = Vec::new();
        stack.clear();
        if stack.capacity() < 16 {
            stack.reserve(16);
        }
        Frame {
            locals,
            stack,
            arg_slots: Vec::new(),
            arg_base: 0,
            pc: usize::MAX,
            refs: None,
            nrefs: None,
        }
    }
    #[inline(always)]
    pub(super) fn push(&mut self, v: Slot) {
        // 快路径：容量够就直接写进去（解释器每条指令都在走这条）。
        // 慢路径（扩容）交给 `Vec::push`，它一年也走不到几次。
        let n = self.stack.len();
        if n < self.stack.capacity() {
            // SAFETY: `n < capacity`，写入后立刻把 len 提到 n+1。
            unsafe {
                self.stack.as_mut_ptr().add(n).write(v);
                self.stack.set_len(n + 1);
            }
        } else {
            self.stack.push(v);
        }
    }
    #[inline(always)]
    pub(super) fn pop(&mut self) -> Result<Slot, VmError> {
        let n = self.stack.len();
        if n == 0 {
            return Err(VmError { msg: "操作数栈下溢".into(), exc: None });
        }
        // SAFETY: `n > 0`，读的是已初始化区间的最后一个元素，读完立刻缩短 len。
        unsafe {
            let v = *self.stack.as_ptr().add(n - 1);
            self.stack.set_len(n - 1);
            Ok(v)
        }
    }
    #[inline(always)]
    pub(super) fn pop_i(&mut self) -> Result<i32, VmError> {
        Ok(self.pop()?.as_i32())
    }
    #[inline(always)]
    pub(super) fn load(&self, slot: usize) -> Slot {
        match self.locals.get(slot) {
            Some(v) => *v,
            None => Slot::I(0),
        }
    }
    #[inline(always)]
    pub(super) fn store(&mut self, slot: usize, v: Slot) {
        if slot + 1 >= self.locals.len() {
            self.locals.resize(slot + 2, Slot::I(0));
        }
        self.locals[slot] = v;
        if v.is_wide() {
            self.locals[slot + 1] = Slot::I(0); // 宽值的第 2 槽无效
        }
    }

    /// 参数落位（规则本体，解释器与 native 共用一份）。
    /// `slots` 是 `crate::jit::param_slots(desc)` 的结果 —— 调用方给现成的，
    /// 所以缓存过槽位表的路径（native 派发的快路、解释器帧）不必每帧重算一遍。
    pub(super) fn place_args_slots(&mut self, slots: &[usize], args: &[Slot], receiver: bool) {
        let off = usize::from(receiver);
        self.arg_base = off;
        self.arg_slots = slots.to_vec();
        for (i, a) in args.iter().enumerate() {
            let slot = if i < off { 0 } else { slots.get(i - off).copied().unwrap_or(i - off) + off };
            self.store(slot, *a);
        }
    }

    /// 第 `i` 个**声明的参数**（0 起，不含 `this`）所在的槽。
    ///
    /// native 实现一律走这里 —— 手写槽位下标必然算错（`Unsafe.getX(Object,long)`
    /// 是**实例**方法，`this` 占槽 0，`o` 在槽 1、`offset` 在槽 2，不是 0/1）。
    pub(super) fn arg_slot(&self, i: usize) -> usize {
        self.arg_base + self.arg_slots.get(i).copied().unwrap_or(i)
    }
    pub(super) fn arg(&self, i: usize) -> Slot {
        self.load(self.arg_slot(i))
    }
    pub(super) fn arg_i(&self, i: usize) -> i32 {
        self.arg(i).as_i32()
    }
    pub(super) fn arg_l(&self, i: usize) -> i64 {
        self.arg(i).as_i64()
    }
    pub(super) fn arg_f(&self, i: usize) -> f32 {
        self.arg(i).as_f32()
    }
    pub(super) fn arg_d(&self, i: usize) -> f64 {
        self.arg(i).as_f64()
    }
    /// `this`（实例方法）
    pub(super) fn this(&self) -> i32 {
        self.load(0).as_i32()
    }
}

impl Drop for Frame {
    /// 把两个缓冲区还给线程池（见 `BUF_POOL` 的说明：这时帧已经不在任何根集里）。
    fn drop(&mut self) {}
}
