//! 指令分派主循环：`Vm::execute`（含进入方法时的参数落位）。

use super::shared::lk;
use crate::CpEntry;

/// 进程级指令计数（`VMLESS_HANG_DUMP` 卡死探针用）。
static HANG_TICKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

use super::call::count_args;
use super::class::class_name_of;
use super::err;
use super::heap::STR_BASE;
use super::exception::thrown;
use super::frame::Frame;
use super::native_reg;
use super::slot::Slot;
use super::Vm;
use super::call::MethodRefInfo;
use super::VmError;

/// `VMLESS_TRACE` 的常驻判定（诊断开关，进程级，读一次定终身）
#[inline(always)]
fn vm_trace_on() -> bool {
    static B: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *B.get_or_init(|| std::env::var_os("VMLESS_TRACE").is_some())
}

/// `VMLESS_HANG_DUMP=<秒>` → 探针阈值（0 = 关）
#[inline(always)]
fn hang_hz() -> u64 {
    static B: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *B.get_or_init(|| {
        std::env::var_os("VMLESS_HANG_DUMP")
            .and_then(|v| v.to_string_lossy().parse().ok())
            .map(|s: f64| (s * 25_000_000.0) as u64)
            .unwrap_or(0)
    })
}

/// 方法访问标志：`native`（没有 Code 属性，实现由 VM 提供）
pub(super) const ACC_NATIVE: u16 = 0x0100;

/// 方法访问标志：`synchronized`（进入/退出时自动加解对象监视器）
pub(super) const ACC_SYNCHRONIZED: u16 = 0x0020;

/// 深度硬上限：**只兜底**，正常溢出判据是 `thread::stack_exhausted()`（看真实栈指针）。
///
/// 栈指针判据失效时（线程没有绑定栈顶，比如从别的宿主线程钻进 VM）才靠这条。
/// 取 5 万是为了比 8 MB 栈实际能跑的层数（约 8 千~1 万）明显高，不提前误杀。
const MAX_DEPTH_HARD: usize = 50_000;

/// 字段描述符是不是引用类型（写引用字段要过卡表写屏障）
fn is_ref_desc(d: &str) -> bool {
    d.starts_with('L') || d.starts_with('[')
}

/// 这个句柄是不是字符串堆里的（而不是对象堆 / 数组堆 / 类对象）
fn is_str_handle(h: i32) -> bool {
    h >= STR_BASE
}

/// 一个 `new` 调用点的缓存：类名 + 字段槽数。
///
/// `new` 原来每执行一次就重来一遍：`class_name_of` 分配一个 `String`（类名）、
/// `ensure_init` 拿一次 `sh.initialized` 的 `Mutex` + 按名字哈希、`field_count` 再哈希一次。
/// 实测 `new Object()` 1.7 μs，而同样走分配器的 `new char[2]` 只要 350 ns —— 差的就是这三件。
/// 它们的答案对同一个调用点永远不变（类不重定义、初始化只发生一次，JVMS 5.5）。
struct NewSite {
    /// 钉住 ClassFile（键是裸指针，不钉住就可能认成别的类的缓存）
    _cf: std::sync::Arc<crate::ClassFile>,
    /// 泄漏成 `'static str`：快路要把它取出来（Copy）之后再去调 `ensure_init`，
    /// 不能借着一个 `RefCell` 的借用跨过 `&mut self` 的调用。
    name: &'static str,
    /// `<clinit>` 已经跑过（初始化单调、永不回退）
    inited: std::cell::Cell<bool>,
}

/// 一个 native 方法的派发缓存（键：`(ClassFile 指针, 方法下标)`）。
///
/// 派发原来每次调用都要：从常量池 utf8 复制出类名/方法名/描述符三个 `String`，
/// 交给 `require_native` 再做一次 `class.to_string()` + 三串元组哈希，然后 `param_slots(desc)`
/// 再解析一遍描述符。实测 645 ns/次，而 JDK 的热路径全是 native
/// （`Unsafe.putByte` 这类一次 `Integer.toString` 就调四次）。
struct NatSite {
    /// 钉住 ClassFile（键是裸指针）
    _cf: std::sync::Arc<crate::ClassFile>,
    f: native_reg::NativeFn,
    /// 参数起始槽（`jit::param_slots`）
    slots: Vec<usize>,
    /// 帧要几个槽：最后一个参数槽 + 2（与 `place_args` 的老算法一致）
    frame_slots: usize,
}

/// 一个方法的**参数落位**缓存：参数起始槽表 + 「参数里有没有 long/double」。
///
/// `place_args` 每次调用都要 `jit::param_slots(desc)`（分配 + 扫描述符）再 `clone`
/// 一份塞进 `frame.arg_slots`（第二次分配）—— 实测 285 ns/次。绝大多数方法没有宽参数，
/// 起始槽就是 0,1,2,…，可以直接整段拷贝。
/// 一个方法**执行期不变**的元数据：Code 属性位置 + 异常表 + 引用位图。
///
/// 这三样原来每次调用都要重做：`code_of` 线性扫一遍常量池找 `"Code"` 名字、再扫属性表；
/// `method_refs` 拿一次 `sh.refmaps` 的读锁 + 哈希。实测这是每次调用里最贵的一格
/// （~1 μs），而它们的答案对同一个方法永远不变。
/// 常量池「字段/方法引用」的**按调用点**解析缓存。
///
/// `method_ref` 每次都要把类名、方法名、描述符各复制一个 `String`（3 次堆分配）——
/// 同一条常量池条目的答案恒定，一条 `invokevirtual` 执行一百万次就复制一百万次。
/// 值里连 `Arc<ClassFile>` 一起存，钉住它，指针不会被复用。
struct CodeMeta {
    /// 钉住 ClassFile：`body` 指向它的 Code 属性，这块内存必须与缓存同寿
    _cf: std::sync::Arc<crate::ClassFile>,
    max_locals: usize,
    body: *const u8,
    body_len: usize,
    exc: Box<[(u16, u16, u16, u16)]>,
    refs: std::sync::Arc<super::gc::MethodRefs>,
}

struct ArgsSite {
    _cf: std::sync::Arc<crate::ClassFile>,
    /// 参数起始槽（进 `Frame::place_args_slots` 的老路时要 usize）
    slots: Vec<usize>,
    /// 参数里没有 long/double → 起始槽就是下标本身
    flat: bool,
}

thread_local! {
    static SITE_REFS: std::cell::RefCell<
        std::collections::HashMap<
            (usize, u16),
            (std::sync::Arc<crate::ClassFile>, std::rc::Rc<MethodRefInfo>),
        >,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
    static CODE_SITE: std::cell::RefCell<
        std::collections::HashMap<(usize, u32), std::rc::Rc<CodeMeta>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
    static ARG_SITE: std::cell::RefCell<
        std::collections::HashMap<(usize, u32), std::rc::Rc<ArgsSite>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
    static NAT_SITE: std::cell::RefCell<
        std::collections::HashMap<(usize, u32), std::rc::Rc<NatSite>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
    static SITE_NEW: std::cell::RefCell<std::collections::HashMap<(usize, u16), std::rc::Rc<NewSite>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

impl Vm {
    /// 取（或建）这个调用点的引用解析结果（见 `SITE_REFS`）。
    #[inline]
    fn site_ref(
        &self,
        cf: &std::sync::Arc<crate::ClassFile>,
        index: u16,
    ) -> Result<std::rc::Rc<MethodRefInfo>, VmError> {
        let key = (std::sync::Arc::as_ptr(cf) as usize, index);
        if let Some(r) = SITE_REFS.with(|c| c.borrow().get(&key).map(|&(_, ref r)| std::rc::Rc::clone(r))) {
            return Ok(r);
        }
        let r = std::rc::Rc::new(self.method_ref(cf, index)?);
        SITE_REFS.with(|c| c.borrow_mut().insert(key, (std::sync::Arc::clone(cf), std::rc::Rc::clone(&r))));
        Ok(r)
    }

    /// 静态/特殊调用的方法解析：成功一次就永久缓存（JVMS 5.4.3.3 明确要求缓存解析结果）。
    ///
    /// 为什么要缓存：`resolve_method` 每次都要 `chain()`（沿着继承链克隆类名 —— 每层两次
    /// `String` 分配）、再对每层做一次 `method_matches`（又一次 `this_class_name()` 分配）。
    /// 实测这一步单独占 **907 ns**，是每次 Java 调用固定开销的 45%。解析结果对同一个常量池
    /// 引用是固定的：类的父类链在加载后不再变化，方法只会「解析到」或「解析不到」，
    /// 不会先解析到一个再换成另一个。
    #[inline]
    fn resolved_target(&self, info: &MethodRefInfo) -> Option<(usize, usize)> {
        let c = info.target.get();
        if c != 0 {
            let v = c - 1;
            return Some(((v >> 32) as usize, (v & 0xffff_ffff) as usize));
        }
        let t = self.store.resolve_method(&info.class, &info.name, &info.desc)?;
        info.target.set((((t.0 as u64) << 32) | t.1 as u64) + 1);
        Some(t)
    }

    /// 虚分派目标（单态内联缓存）。
    ///
    /// 为什么要它：`invokevirtual` 每次都要按**接收者的运行时类**解析：
    /// `ref_kind` → 拿到类名 `String`（分配），`resolve_method` 再 `chain()` + 逐层
    /// `method_matches`（又一次 `this_class_name()` 分配）—— rdtsc 实测整条虚调用 1.3 μs，
    /// `String.length()` 这种一个 getfield 的方法要 1 μs，`Integer.toString` 里十几条
    /// 虚调用就是这么堆成 30 μs 的。
    ///
    /// 单态缓存以「类名的 `Arc<str>` 指针」为键：`class_names` 表持有那些 `Arc` 直到进程结束，
    /// 同一个类恒得同一个指针，不同类必不同（`intern_class_id` 保证 id 不复用）。
    /// 命中就跳过 `ref_kind` + `resolve_method`，只剩「取类名 + 指针比较」。
    #[inline]
    fn vtarget(
        &self,
        info: &MethodRefInfo,
        cname: Option<&std::sync::Arc<str>>,
        ckey: usize,
    ) -> Option<(usize, usize)> {
        if ckey != 0 {
            let m = info.mono.borrow();
            if m.0 == ckey && m.1 != 0 {
                let v = m.1 - 1;
                return Some(((v >> 32) as usize, (v & 0xffff_ffff) as usize));
            }
        }
        let t = cname
            .map(|a| a.as_ref())
            .and_then(|c| self.store.resolve_method(c, &info.name, &info.desc))
            .or_else(|| self.store.resolve_method(&info.class, &info.name, &info.desc))?;
        if ckey != 0 {
            info.mono
                .replace((ckey, (((t.0 as u64) << 32) | t.1 as u64) + 1, cname.cloned()));
        }
        Some(t)
    }

    /// 字段偏移（按调用点缓存 + **类表代数守卫**）。
    ///
    /// 为什么要缓存：`store.field_index(owner, name)` 第一件事就是 `chain(owner)` ——
    /// 每层一个 `String` 克隆 + 一个 `Vec`，再对每层做一次 `HashMap<String, _>` 查找。
    /// rdtsc 实测一次 `getfield` 里它占 **477 ns**（`pt.x` 这种单字段读取整条指令 929 ns）。
    ///
    /// 为什么带代数：`own_fields` 的下标是 `build_meta_one` 在**类加载那一刻**按当时的
    /// 继承链算出来的，父类后加载会改变答案（#147 那次 `System.out` 变 null 就是
    /// 「类在表里 ≠ 布局定稿」）。所以缓存只在「类表没变过」的窗口内有效：
    /// `classes.len()` 一变，所有旧条目自动作废，退回逐次解析。只缓存**解析成功**的。
    #[inline]
    fn field_off_cached(&self, info: &MethodRefInfo) -> Option<usize> {
        let gen = self.store.classes.len() as u32;
        let (g, off) = info.field.get();
        if g == gen && off != 0 {
            return Some(off as usize - 1);
        }
        let ix = self.store.field_index(&info.class, &info.name)?;
        info.field.set((gen, ix as u32 + 1));
        Some(ix)
    }

    /// 字符串句柄的字段槽下标（`value`/`coder`/`hash`/`hashIsZero`，名字表固定）
    #[inline]
    fn str_slot_cached(&self, info: &MethodRefInfo) -> Result<usize, VmError> {
        let c = info.str_slot.get();
        if c != 0 {
            return Ok(c as usize - 1);
        }
        let i = self.str_field_slot(&info.name)?;
        info.str_slot.set(i as u32 + 1);
        Ok(i)
    }

    /// 这个方法的引用位图（按 `(ClassFile 地址, 方法下标)` 缓存，第一次执行时算）。
    ///
    /// 缓存里连 `Arc<ClassFile>` 一起存 —— 这样 ClassFile 不会被释放、指针不会被复用，
    /// 也就不会出现「拿到另一份类文件的位图」这种灾难。
    #[allow(clippy::too_many_arguments)]
    fn method_refs(
        &self,
        cf: &std::sync::Arc<crate::ClassFile>,
        method: usize,
        desc: &str,
        max_locals: usize,
        is_static: bool,
        body: &[u8],
        exc: &[(u16, u16, u16, u16)],
    ) -> std::sync::Arc<super::gc::MethodRefs> {
        let key = (std::sync::Arc::as_ptr(cf) as usize, method);
        if let Some((c, m)) = self
            .sh
            .refmaps
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            if std::sync::Arc::ptr_eq(c, cf) {
                return std::sync::Arc::clone(m);
            }
        }
        let m = std::sync::Arc::new(super::gc::analyze(cf, body, desc, max_locals, is_static, exc));
        self.sh.refmap_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if m.is_conservative() {
            self.sh.refmap_cons.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.sh
            .refmaps
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, (std::sync::Arc::clone(cf), std::sync::Arc::clone(&m)));
        m
    }

    /// `ldc` / `ldc_w`：int / float / String / **Class** / **MethodHandle** / **MethodType** /
    /// **CONSTANT_Dynamic** 常量（都只占一个槽）
    pub(super) fn ldc(&mut self, class: usize, index: u16) -> Result<Slot, VmError> {
        let cf = self.store.classes[class].clone();
        match cf.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
            Some(CpEntry::Integer(v)) => Ok(Slot::I(*v as i32)),
            Some(CpEntry::Float(bits)) => Ok(Slot::Fl(f32::from_bits(*bits))),
            Some(CpEntry::String(_)) => Ok(Slot::I(self.string_handle(class, index))), // 句柄进字符串堆
            // `X.class`：压一个**类对象**（同一个类永远同一个句柄，`X.class == X.class` 靠它）
            Some(CpEntry::Class(ix)) => {
                let name = String::from_utf8_lossy(cf.utf8(*ix).unwrap_or_default()).into_owned();
                Ok(Slot::I(self.class_obj(&name)))
            }
            // Java 7 起常量池还能直接放方法句柄 / 方法类型 / 动态常量（后者要跑引导方法，
            // 走 `reify_const` 的那条统一路径，包含 `CONSTANT_Dynamic` 的缓存）
            Some(CpEntry::MethodHandle(..))
            | Some(CpEntry::MethodType(..))
            | Some(CpEntry::Dynamic(..)) => Ok(self.reify_const(class, index)?.1),
            other => err(format!("ldc 不支持的类型: {other:?}")),
        }
    }

    /// `ldc2_w`：long / double 常量
    pub(super) fn ldc2_w(&self, class: usize, index: u16) -> Result<Slot, VmError> {
        let cf = &self.store.classes[class];
        match cf.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
            Some(CpEntry::Long(v)) => Ok(Slot::L(*v as i64)),
            Some(CpEntry::Double(bits)) => Ok(Slot::F(f64::from_bits(*bits))),
            other => err(format!("ldc2_w 不支持的类型: {other:?}")),
        }
    }

    /// 执行一个方法：`class`/`method` 是它在类表里的位置；desc 形如 `(II)I`；
    /// `receiver` 表示 args[0] 是 this（实例方法）。
    ///
    /// `ACC_SYNCHRONIZED` 的方法在这里成对加解锁 —— 放在这一层是因为**每个**调用点
    /// （virtual/special/static/interface/indy/lambda/反射）最终都走 `execute`，
    /// 在这里做一次就不会漏；异常路径同样会解锁（JVMS：监视器随帧弹出释放）。
    pub(super) fn execute(
        &mut self,
        class: usize,
        method: usize,
        desc: &str,
        args: &[Slot],
        receiver: bool,
    ) -> Result<Slot, VmError> {
        // 方法级字节码校验（JVMS 4.10.1）：非法 class 必须**在跑任何一条指令之前**被拒，
        // 这才是 HotSpot 的 VerifyError 语义。放在 `execute` 而不是 `execute_body`，
        // 是为了连下面那条 JIT 快路也一起挡住 —— 否则非法方法被 JIT 编走了就没人验。
        // 结论按 (ClassFile, 方法下标) 缓存，热路径上只剩一次线程本地缓存比对。
        if let Err(why) = super::verify::ensure_verified(self, &self.store.classes[class], method) {
            return Err(self.java_exception("java/lang/VerifyError", why));
        }
        // 方法级回退的**反向**通道：这个方法在 JIT 那边已经编好了（纯数值、非实例），
        // 就直接调机器码。表是空的时侯这里只有一次借用 —— 全解释执行的路径不受影响。
        if !receiver {
            if let Some(r) = super::bridge::try_call_jit(&self.store.classes[class], method, desc, args, receiver) {
                return r;
            }
        }
        let sync = {
            let cf = &self.store.classes[class];
            cf.methods[method].access_flags & ACC_SYNCHRONIZED != 0
        };
        let lock = if !sync {
            0
        } else if receiver {
            args.first().map(|s| s.as_i32()).unwrap_or(0)
        } else {
            // 静态 synchronized 方法锁的是**声明类**的类对象
            let name = self.store.classes[class].this_class_name().unwrap_or_default();
            self.class_obj(&name)
        };
        if lock != 0 {
            self.monitor_enter(lock)?;
        }
        let r = self.execute_body(class, method, desc, args, receiver);
        if lock != 0 {
            match self.monitor_exit(lock) {
                Ok(()) => {}
                // 方法体已经抛异常时，解锁失败不能盖掉原来的异常
                Err(_) if r.is_err() => {}
                Err(e) => return Err(e),
            }
        }
        r
    }

    /// `invoke_api` 说「这个我没接管」时的兜底：**类里真有这个方法就跑真字节码**。
    ///
    /// `is_invoke_api_class` 是「整包接管」的粗筛（`java.lang.invoke.*` 里大量方法是
    /// VM 原语 —— `invokeBasic` / `linkToVirtual` / 隐藏类 —— 执行它们的字节码不可能
    /// 跑通）。但那个包里也有**纯 Java 的辅助方法**，典型是
    /// `MethodHandleImpl.runtimeSetup()`：它只是装 `SharedSecrets.JavaLangInvokeAccess`。
    /// 把它一并拒掉，`ClassFrameInfo.<clinit>`（`StackWalker` 依赖它）就会
    /// `BootstrapMethodError`。所以判据是「VM 没接管 **且** 解析得到方法」→ 跑字节码。
    fn invoke_api_fallback(
        &mut self,
        info: &super::call::MethodRefInfo,
        recv: Option<i32>,
        args: &[Slot],
        op_pc: usize,
    ) -> Result<Slot, VmError> {
        let target = self
            .store
            .resolve_method(&info.class, &info.name, &info.desc)
            .or_else(|| {
                // 虚调用：引用类上没有就按接收者的运行时类再找一次
                recv.and_then(|r| self.obj_class_name(r))
                    .and_then(|c| self.store.resolve_method(&c, &info.name, &info.desc))
            });
        match target {
            Some((ci, mi)) => {
                super::stacktrace::set_pc(op_pc);
                let mut all: Vec<Slot> = Vec::with_capacity(args.len() + 1);
                if let Some(r) = recv {
                    all.push(Slot::I(r));
                }
                all.extend_from_slice(args);
                self.depth += 1;
                let r = self.execute(ci, mi, &info.desc, &all, recv.is_some());
                self.depth -= 1;
                r
            }
            None => Err(self.java_exception(
                "java/lang/BootstrapMethodError",
                format!("VM 没有接管 {}.{}{}", info.class, info.name, info.desc),
            )),
        }
    }

    /// 静态字段的**声明类**：从引用类出发，沿继承链找第一个自己声明了这个静态字段的类。
    ///
    /// JVMS 5.4.3.2 的字段解析要求沿父类链找；javac 对**继承来的**静态字段可能仍用
    /// 子类名做 owner —— JDK 25 的 `StackFrameInfo.declaringClass()` 读的就是
    /// `StackFrameInfo.JLIA`，而 `JLIA` 声明在父类 `java/lang/ClassFrameInfo` 上。
    /// 只按引用类建表会读到 0（静态字段表按声明类存）。
    fn static_owner(&self, class: &str, name: &str) -> String {
        for c in self.store.chain(class) {
            if let Some(cf) = self.store.class_file(&c) {
                let hit = cf.fields.iter().any(|f| {
                    f.access_flags & 0x0008 != 0
                        && cf.utf8(f.name_index).map(|b| b == name.as_bytes()).unwrap_or(false)
                });
                if hit {
                    return c;
                }
            }
        }
        class.to_string()
    }

    fn execute_body(
        &mut self,
        class: usize,
        method: usize,
        desc: &str,
        args: &[Slot],
        receiver: bool,
    ) -> Result<Slot, VmError> {
        // 递归深度上限：**按真实栈余量判**，不是拍一个深度数字。
        //
        // 历史上这里是 `if self.depth > 64 { err(..) }` —— 后果是任何正常递归
        // （快排、树遍历、解析器，甚至 `R5` 那种 `deep(0)` 到 5000 的样张）直接
        // 硬失败。现在判据是「本线程栈还能不能再压一层」：解释器每层 Java 调用
        // 都是真的 Rust 栈帧，所以栈指针才是唯一正确的口径。
        //
        // 撞上限时抛的是**真 Java 异常**（`StackOverflowError`），Java 代码
        // `catch (Throwable e)` 接得住；以前那个 `err(...)` 是 `exc: None`，
        // 用户的 `catch` 形同虚设、程序直接死。
        if super::thread::stack_exhausted() || self.depth > MAX_DEPTH_HARD {
            // `VMLESS_SOE_TRACE=1`：把「这一层用到了多少栈 / 每层平均多少字节」
            // 打出来 —— 定 `VMLESS_STACK_MB` 和判据是否过早触发全靠这个数。
            if std::env::var_os("VMLESS_SOE_TRACE").is_some() {
                let used = super::thread::stack_used();
                let per = if self.depth > 0 { used / self.depth } else { 0 };
                eprintln!(
                    "[soe] depth={} used={}KB 每层≈{}B 栈上限={}MB 判据=真实栈指针",
                    self.depth,
                    used / 1024,
                    per,
                    super::thread::java_stack_bytes() >> 20
                );
            }
            return Err(self.java_exception(
                "java/lang/StackOverflowError",
                "stack overflow（解释器栈耗尽；`VMLESS_STACK_MB` 可调大线程栈）",
            ));
        }
        // 每次调用先把「当前类」钉住：常量池、Code 属性都按它取（继承后方法来自不同类）
        let cf = self.store.classes[class].clone();
        let m = &cf.methods[method];
        // VM 接管的 JDK 方法（`Thread.<init>` / `Thread.start` 之类）：它们**不是** native，
        // 但真字节码要拖进 ThreadGroup / ClassLoader / ThreadContainers 一整条链，
        // 而这一层语义本该由 VM 提供（HotSpot 也是 native/内建）。先查接管表。
        //
        // 代价必须压住：`execute_body` 是解释器最热的函数，所以先用**不分配**的类名
        // 比较挡一道，只有命中接管清单里的类才去做真正的查表。
        if self.sh.natives.has_overrides() && is_override_owner(&cf) {
            let owner = cf.this_class_name().unwrap_or_default();
            let name = String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default()).into_owned();
            let odesc = String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default()).into_owned();
            if let Some(f) = self.sh.natives.lookup_override(&owner, &name, &odesc) {
                let mut frame =
                    Frame::new(crate::jit::param_slots(&odesc).last().map(|s| s + 2).unwrap_or(1));
                native_reg::place_args(&mut frame, &odesc, args, receiver);
                return f(self, &mut frame);
            }
        }
        // native 方法没有 Code 属性：先查注册表，命中就交出去；没命中报 UnsatisfiedLinkError
        if m.access_flags & ACC_NATIVE != 0 {
            let nkey = (std::sync::Arc::as_ptr(&cf) as usize, method as u32);
            let cached_nat = NAT_SITE.with(|c| c.borrow().get(&nkey).map(std::rc::Rc::clone));
            let site = match cached_nat {
                Some(s) => s,
                None => {
                    let owner = cf.this_class_name().unwrap_or_default();
                    let name = String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default())
                        .into_owned();
                    let desc =
                        String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default())
                            .into_owned();
                    // **只缓存静态注册表命中的**：JNI 那条路返回的是共享跳板
                    // （真目标在线程本地 pending 槽），缓存了会串到别人的函数上。
                    let cacheable = self.sh.natives.lookup_static(&owner, &name, &desc);
                    let f = match cacheable {
                        Some(f) => f,
                        None => self.require_native(&owner, &name, &desc)?,
                    };
                    let slots = crate::jit::param_slots(&desc);
                    let frame_slots = slots.last().map(|s| s + 2).unwrap_or(1);
                    let s = std::rc::Rc::new(NatSite {
                        _cf: std::sync::Arc::clone(&cf),
                        f,
                        slots,
                        frame_slots,
                    });
                    if cacheable.is_some() {
                        NAT_SITE.with(|c| c.borrow_mut().insert(nkey, std::rc::Rc::clone(&s)));
                    }
                    s
                }
            };
            let f = site.f;
            let mut frame = Frame::new(site.frame_slots);
            frame.place_args_slots(&site.slots, args, receiver);
            return f(self, &mut frame);
        }
        let ckey = (std::sync::Arc::as_ptr(&cf) as usize, method as u32);
        let cached_c = CODE_SITE.with(|c| c.borrow().get(&ckey).map(std::rc::Rc::clone));
        let cm = match cached_c {
            Some(m) => m,
            None => {
                let (ml, body0, exc0) = code_of(&cf, m).map_err(|e| VmError {
                    msg: format!(
                        "{}.{}{}: {}",
                        cf.this_class_name().unwrap_or_default(),
                        String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default()),
                        String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default()),
                        e.msg
                    ),
                    exc: None,
                })?;
                let is_static0 = m.access_flags & 0x0008 != 0;
                let refs0 = self.method_refs(&cf, method, desc, ml, is_static0, body0, &exc0);
                let cm = std::rc::Rc::new(CodeMeta {
                    _cf: std::sync::Arc::clone(&cf),
                    max_locals: ml,
                    body: body0.as_ptr(),
                    body_len: body0.len(),
                    exc: exc0.into_boxed_slice(),
                    refs: refs0,
                });
                CODE_SITE.with(|c| c.borrow_mut().insert(ckey, std::rc::Rc::clone(&cm)));
                cm
            }
        };
        let max_locals = cm.max_locals;
        // SAFETY: `cm` 里钉着 ClassFile，`body` 指向它的 Code 属性，与缓存同寿
        let body: &[u8] = unsafe { std::slice::from_raw_parts(cm.body, cm.body_len) };
        let exc_table: &[(u16, u16, u16, u16)] = &cm.exc;

        let mut frame = Frame::new(max_locals);
        // 参数落位：按 (ClassFile, 方法) 缓存槽表（见 `ArgsSite`）
        let akey = (std::sync::Arc::as_ptr(&cf) as usize, method as u32);
        let cached_args = ARG_SITE.with(|c| c.borrow().get(&akey).map(std::rc::Rc::clone));
        let asite = match cached_args {
            Some(s) => s,
            None => {
                let slots = crate::jit::param_slots(desc);
                let end = desc.as_bytes().iter().position(|b| *b == b')').unwrap_or(desc.len());
                let flat =
                    !desc.as_bytes()[..end].iter().any(|b| *b == b'J' || *b == b'D');
                let s = std::rc::Rc::new(ArgsSite {
                    _cf: std::sync::Arc::clone(&cf),
                    slots,
                    flat,
                });
                ARG_SITE.with(|c| c.borrow_mut().insert(akey, std::rc::Rc::clone(&s)));
                s
            }
        };
        // 帧链：进方法压一帧（只有原子量；Drop 时弹）。native 方法不压 —— 与 HotSpot 一样
        // 不把 native 帧暴露给 `getStackTrace`/`StackWalker`。
        let _frame_mark = super::stacktrace::enter(&cf, method);
        frame.arg_base = usize::from(receiver);
        let nargs = args.len();
        if asite.flat && nargs == asite.slots.len() + usize::from(receiver) && nargs <= frame.locals.len()
        {
            // 参数槽就是 0,1,2,…（没有宽值）→ 整段拷贝；`this` 本就在 args[0]、槽 0
            frame.locals[..nargs].copy_from_slice(args);
        } else {
            frame.place_args_slots(&asite.slots, args, receiver);
        }
        // 引用位图：这个方法的每个 pc 上「哪些槽是引用」（GC 找根的唯一依据）。
        // 算一次缓存起来，之后每个调用点只是一次读锁 + 一次哈希。
        let is_static = m.access_flags & 0x0008 != 0;
        frame.refs = Some(std::sync::Arc::clone(&cm.refs));
        // 登记到本线程的根集（异常/提前返回时由 Drop 摘掉）
        let _frame_guard = super::gc::FrameGuard::push(&frame, self as *const Vm);

        // 调用点上的异常：被调方抛出来的 Java 异常不能直接往上冒 ——
        // 先看本方法的异常表接不接得住（接住就按 handler 入口继续跑）。
        enum CallOutcome {
            Value(Slot),
            /// 交给本方法的 handler：(handler 偏移, 异常对象句柄)
            Handler(usize, i32),
        }
        macro_rules! call_or_handle {
            ($res:expr, $op_pc:expr) => {{
                match $res {
                    Ok(v) => CallOutcome::Value(v),
                    Err(e) => match e.exc {
                        Some(h) => match self.find_handler(&cf, &exc_table, $op_pc, h)? {
                            Some(t) => CallOutcome::Handler(t, h),
                            None => return Err(e),
                        },
                        None => return Err(e),
                    },
                }
            }};
        }

        let trace = vm_trace_on();
        // 「卡住时它在哪个方法里」的探针：设了 `VMLESS_HANG_DUMP=<秒>` 后，
        // 解释器每跑约 1600 万条指令就往 stderr 打一次当前 类.方法 描述符@pc。
        // 排查死循环时的唯一入口 —— `VMLESS_TRACE` 打的是每条指令，几秒就把屏刷爆了。
        let hang_n: u64 = hang_hz();
        // 计数器**必须是进程级**的：方法体内是局部量的话，「方法进→native→返回」这种
        // 循环每次调用只有几十条指令，永远到不了阈值 —— 探针就永远不打印。

        let dump_ctx = if hang_n > 0 {
            Some((
                cf.this_class_name().unwrap_or_default(),
                String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default()).into_owned(),
                String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default()).into_owned(),
            ))
        } else {
            None
        };
        let mut pc = 0usize;
        while pc < body.len() {
            // 让 GC 知道「当前执行到哪条指令」：栈上只多一个 usize，换来的是任何时刻
            // （包括被调方正在跑、本帧停着不动）都能拿到**精确**的引用位图。
            frame.pc = pc;
            // 卡死探针：每 ~1600 万条指令打一次「我在哪」
            // 卡死探针：每 ~1600 万条指令打一次「我在哪」。
            // 计数器是**进程级原子量**：方法体内局部量的话，「方法进 → native → 返回」
            // 这种循环每次调用只有几十条指令，永远到不了阈值，探针就永远不打印
            // （这正是之前误判「不是 spin 是阻塞」的原因）。
            if hang_n > 0 {
                let g = HANG_TICKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if g % 16_000_000 == 0 {
                    if let Some((c, n, d)) = &dump_ctx {
                        eprintln!(
                            "[hang] {} ms  {}.{}{}  pc={pc}",
                            g / 25_000,
                            c, n, d
                        );
                    }
                }
            }
            // 抢占点：多线程时定期交还 GIL。没有它，一个死循环（或长计算）线程会把
            // 别的 Java 线程饿死 —— `Thread.start` 之后对方永远跑不上来。
            // 单线程时这里只是一次整数比较，不进函数。
            self.tick += 1;
            if self.sh.gc_stress {
                // 压力模式：GC 检查点密得多（把漏根立刻打成崩溃，而不是攒到后面）
                if self.tick >= 64 {
                    self.tick = 0;
                    self.safepoint();
                    self.gc_poll(&mut frame, pc);
                }
            } else if self.tick >= super::thread::SAFEPOINT_EVERY {
                self.tick = 0;
                // 采样点：让别的线程（jstack / 死锁报告 / 帧栈诊断）看到这一帧的当前位置
                super::stacktrace::set_pc(pc);
                self.safepoint();
                self.gc_poll(&mut frame, pc);
            }
            let op = body[pc];
            if trace {
                eprintln!("  pc={pc} op={op:02x} stack={:?} locals={:?}", frame.stack, frame.locals);
            }
            pc += 1;
            match op {
                0x00 => {}
                0x01 => frame.push(Slot::I(0)),             // aconst_null
                0x02 => frame.push(Slot::I(-1)),
                0x03..=0x08 => frame.push(Slot::I(op as i32 - 0x03)),
                0x09 | 0x0a => frame.push(Slot::L(op as i64 - 0x09)), // lconst_0/1
                0x0b..=0x0d => frame.push(Slot::Fl(op as f32 - 0x0b as f32)), // fconst_0/1/2
                0x0e | 0x0f => frame.push(Slot::F(if op == 0x0e { 0.0 } else { 1.0 })),
                0x10 => {
                    let v = body[pc] as i8 as i32;
                    pc += 1;
                    frame.push(Slot::I(v));
                }
                0x11 => {
                    let v = i16::from_be_bytes([body[pc], body[pc + 1]]) as i32;
                    pc += 2;
                    frame.push(Slot::I(v));
                }
                0x12 => {
                    let idx = body[pc] as u16;
                    pc += 1;
                    let v = self.ldc(class, idx)?;
                    frame.push(v);
                }
                0x13 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let v = self.ldc(class, idx)?;
                    frame.push(v);
                }
                0x14 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let v = self.ldc2_w(class, idx)?;
                    frame.push(v);
                }
                // 局部变量读取（显式下标）
                0x15..=0x19 => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    frame.push(frame.load(slot));
                }
                // 局部变量读取（隐式下标 iload_0.., lload_0.., fload_0.., dload_0.., aload_0..）
                0x1a..=0x1d => {
                    let slot = (op - 0x1a) as usize;
                    frame.push(frame.load(slot));
                }
                0x1e..=0x21 => {
                    let slot = (op - 0x1e) as usize;
                    frame.push(frame.load(slot));
                }
                0x22..=0x25 => {
                    let slot = (op - 0x22) as usize;
                    frame.push(frame.load(slot));
                }
                0x26..=0x29 => {
                    let slot = (op - 0x26) as usize;
                    frame.push(frame.load(slot));
                }
                0x2a..=0x2d => {
                    let slot = (op - 0x2a) as usize;
                    frame.push(frame.load(slot));
                }
                // 局部变量存储（istore/lstore/fstore/dstore/astore + _n 形式）
                0x36..=0x3a => {
                    let slot = body[pc] as usize;
                    pc += 1;
                    let v = frame.pop()?;
                    frame.store(slot, v);
                }
                0x3b..=0x3e => {
                    let slot = (op - 0x3b) as usize;
                    let v = frame.pop()?;
                    frame.store(slot, v);
                }
                0x3f..=0x42 => {
                    let slot = (op - 0x3f) as usize;
                    let v = frame.pop()?;
                    frame.store(slot, v);
                }
                0x43..=0x46 => {
                    let slot = (op - 0x43) as usize;
                    let v = frame.pop()?;
                    frame.store(slot, v);
                }
                0x47..=0x4a => {
                    let slot = (op - 0x47) as usize;
                    let v = frame.pop()?;
                    frame.store(slot, v);
                }
                0x4b..=0x4e => {
                    let slot = (op - 0x4b) as usize;
                    let v = frame.pop()?;
                    frame.store(slot, v);
                }
                0x57 => {
                    frame.pop()?;
                }
                // pop2：弹掉一个宽值，或两个普通值
                0x58 => {
                    let v = frame.pop()?;
                    if !v.is_wide() {
                        frame.pop()?;
                    }
                }
                0x59 => {
                    let v = *frame.stack.last().unwrap_or(&Slot::I(0));
                    frame.push(v);
                }
                // dup_x1：… v2, v1 → … v1, v2, v1（两个都是 1 槽）
                0x5a => {
                    let n = frame.stack.len();
                    if n >= 2 {
                        let top = frame.stack[n - 1];
                        frame.stack.insert(n - 2, top);
                    }
                }
                // dup_x2：形式 1 = 全 1 槽（… v3,v2,v1 → … v1,v3,v2,v1）；
                //         形式 2 = 下面那个是宽值（… v2,v1 → … v1,v2,v1）
                0x5b => {
                    let n = frame.stack.len();
                    if n >= 2 {
                        let top = frame.stack[n - 1];
                        if frame.stack[n - 2].is_wide() {
                            frame.stack.insert(n - 2, top);
                        } else if n >= 3 {
                            frame.stack.insert(n - 3, top);
                        }
                    }
                }
                // dup2：复制一个宽值，或栈顶两个普通值
                0x5c => {
                    let n = frame.stack.len();
                    let top = *frame.stack.last().unwrap_or(&Slot::I(0));
                    if top.is_wide() {
                        frame.push(top);
                    } else if n >= 2 {
                        let a = frame.stack[n - 2];
                        frame.push(a);
                        frame.push(top);
                    }
                }
                // dup2_x1：形式 1 = 全 1 槽（… v3,v2,v1 → … v2,v1,v3,v2,v1）；
                //          形式 2 = 栈顶是宽值（… v2,v1 → … v1,v2,v1）
                0x5d => {
                    let n = frame.stack.len();
                    if n >= 2 {
                        let top = frame.stack[n - 1];
                        if top.is_wide() {
                            frame.stack.insert(n - 2, top);
                        } else if n >= 3 {
                            let below = frame.stack[n - 2];
                            frame.stack.insert(n - 3, below);
                            frame.stack.insert(n - 2, top);
                        }
                    }
                }
                // dup2_x2：四个形式，按「栈顶两项的槽宽」区分：
                //   形式 1 = 四个 cat-1；形式 2 = 第三项是宽值；
                //   形式 3 = **栈顶**是宽值、下面两个 cat-1；形式 4 = 栈顶与第二项都是宽值
                0x5e => {
                    let n = frame.stack.len();
                    if n >= 2 {
                        let top = frame.stack[n - 1];
                        if top.is_wide() && frame.stack[n - 2].is_wide() {
                            // 形式 4：… v2(宽), v1(宽) → … v1, v2, v1
                            frame.stack.insert(n - 2, top);
                        } else if top.is_wide() && n >= 3 {
                            // 形式 3：… v3, v2, v1(宽) → … v1, v3, v2, v1
                            frame.stack.insert(n - 3, top);
                        } else if n >= 3 {
                            let below = frame.stack[n - 2];
                            if frame.stack[n - 3].is_wide() {
                                // 形式 2：… v3(宽), v2, v1 → … v2, v1, v3, v2, v1
                                frame.stack.insert(n - 3, below);
                                frame.stack.insert(n - 2, top);
                            } else if n >= 4 {
                                // 形式 1：四个 cat-1 → … v2, v1, v4, v3, v2, v1
                                frame.stack.insert(n - 4, below);
                                frame.stack.insert(n - 3, top);
                            }
                        }
                    }
                }
                // swap：交换栈顶两个 1 槽值
                0x5f => {
                    let n = frame.stack.len();
                    if n >= 2 {
                        frame.stack.swap(n - 1, n - 2);
                    }
                }
                0x84 => {
                    // iinc：局部变量按常量就地加减（索引与常量各 1 字节，都可为负）
                    let slot = body[pc] as usize;
                    let delta = body[pc + 1] as i8 as i32;
                    pc += 2;
                    let v = frame.load(slot).as_i32().wrapping_add(delta);
                    frame.store(slot, Slot::I(v));
                }
                // wide：把后面那条指令的下标扩成 u16（`iinc` 的增量也扩成 i16）。
                // javac 在「局部变量槽 ≥ 256」或「iinc 增量超出 i8」时才发它；
                // 一个方法里局部变量很多（大 `switch`、巨型方法体）就会撞上。
                // 不实现的话报「未实现的字节码 0xc4」，症状是**同一个方法里所有路径都挂**，
                // 看起来跟具体逻辑无关（`fillOval`/`fill(Shape)` 就是这么被挡的）。
                0xc4 => {
                    let op2 = body[pc];
                    let idx = u16::from_be_bytes([body[pc + 1], body[pc + 2]]) as usize;
                    match op2 {
                        // *load
                        0x15..=0x19 => {
                            pc += 3;
                            frame.push(frame.load(idx));
                        }
                        // *store
                        0x36..=0x3a => {
                            pc += 3;
                            let v = frame.pop()?;
                            frame.store(idx, v);
                        }
                        // iinc（u16 下标 + i16 增量）
                        0x84 => {
                            let delta = i16::from_be_bytes([body[pc + 3], body[pc + 4]]) as i32;
                            pc += 5;
                            let v = frame.load(idx).as_i32().wrapping_add(delta);
                            frame.store(idx, Slot::I(v));
                        }
                        // ret：旧版 finally 的产物，本编译器不生成；明确报出来而不是静默跳过
                        other => {
                            return super::err(format!(
                                "wide 后的字节码 {other:#04x} 不支持（pc={pc}）"
                            ))
                        }
                    }
                }
                // int 算术
                0x60 | 0x64 | 0x68 | 0x6c | 0x70 => {
                    let b = frame.pop_i()?;
                    let a = frame.pop_i()?;
                    let r = match op {
                        0x60 => a.wrapping_add(b),
                        0x64 => a.wrapping_sub(b),
                        0x68 => a.wrapping_mul(b),
                        0x6c => {
                            if b == 0 {
                                return err("整数除零");
                            }
                            a.wrapping_div(b)
                        }
                        _ => {
                            if b == 0 {
                                return err("整数取余除零");
                            }
                            a.wrapping_rem(b)
                        }
                    };
                    frame.push(Slot::I(r));
                }
                // long 算术
                0x61 | 0x65 | 0x69 | 0x6d | 0x71 => {
                    let b = frame.pop()?.as_i64();
                    let a = frame.pop()?.as_i64();
                    let r = match op {
                        0x61 => a.wrapping_add(b),
                        0x65 => a.wrapping_sub(b),
                        0x69 => a.wrapping_mul(b),
                        0x6d => {
                            if b == 0 {
                                return err("long 除零");
                            }
                            a.wrapping_div(b)
                        }
                        _ => {
                            if b == 0 {
                                return err("long 取余除零");
                            }
                            a.wrapping_rem(b)
                        }
                    };
                    frame.push(Slot::L(r));
                }
                // double 算术
                0x63 | 0x67 | 0x6b | 0x6f | 0x73 => {
                    let b = frame.pop()?.as_f64();
                    let a = frame.pop()?.as_f64();
                    let r = match op {
                        0x63 => a + b,
                        0x67 => a - b,
                        0x6b => a * b,
                        0x6f => a / b,
                        _ => a % b, // drem：与 fmod 一致（NaN 有 NaN 语义）
                    };
                    frame.push(Slot::F(r));
                }
                // float 算术：**必须按 f32 算再收成 f32**（f64 中间结果会让 `1.0f/3.0f` 变成 double）
                0x62 | 0x66 | 0x6a | 0x6e | 0x72 => {
                    let b = frame.pop()?.as_f32();
                    let a = frame.pop()?.as_f32();
                    let r = match op {
                        0x62 => a + b,
                        0x66 => a - b,
                        0x6a => a * b,
                        0x6e => a / b,
                        _ => a % b, // frem：与 fmodf 一致
                    };
                    frame.push(Slot::Fl(r));
                }
                0x74 => {
                    let a = frame.pop_i()?;
                    frame.push(Slot::I(a.wrapping_neg()));
                }
                0x75 => {
                    let a = frame.pop()?.as_i64();
                    frame.push(Slot::L(a.wrapping_neg()));
                }
                0x76 => {
                    let a = frame.pop()?.as_f32();
                    frame.push(Slot::Fl(-a));
                }
                0x77 => {
                    let a = frame.pop()?.as_f64();
                    frame.push(Slot::F(-a));
                }
                // 数值转换
                0x85 => {
                    let a = frame.pop_i()?;
                    frame.push(Slot::L(a as i64)); // i2l
                }
                0x86 => {
                    let a = frame.pop_i()?;
                    frame.push(Slot::Fl(a as f32)); // i2f
                }
                0x87 => {
                    let a = frame.pop_i()?;
                    frame.push(Slot::F(a as f64)); // i2d
                }
                0x88 => {
                    let a = frame.pop()?.as_i64();
                    frame.push(Slot::I(a as i32)); // l2i
                }
                0x89 => {
                    let a = frame.pop()?.as_i64();
                    frame.push(Slot::Fl(a as f32)); // l2f
                }
                0x8a => {
                    let a = frame.pop()?.as_i64();
                    frame.push(Slot::F(a as f64)); // l2d
                }
                0x8b => {
                    let a = frame.pop()?.as_f32();
                    frame.push(Slot::I(a as i32)); // f2i（Rust 的 as 与 Java 一样饱和）
                }
                0x8c => {
                    let a = frame.pop()?.as_f32();
                    frame.push(Slot::L(a as i64)); // f2l
                }
                0x8d => {
                    let a = frame.pop()?.as_f32();
                    frame.push(Slot::F(a as f64)); // f2d（f32 → f64 是精确的）
                }
                0x8e => {
                    let a = frame.pop()?.as_f64();
                    frame.push(Slot::I(a as i32)); // d2i
                }
                0x8f => {
                    let a = frame.pop()?.as_f64();
                    frame.push(Slot::L(a as i64)); // d2l
                }
                0x90 => {
                    let a = frame.pop()?.as_f64();
                    frame.push(Slot::Fl(a as f32)); // d2f
                }
                0x91 => {
                    let a = frame.pop_i()?;
                    frame.push(Slot::I(a as i8 as i32)); // i2b
                }
                0x92 => {
                    let a = frame.pop_i()?;
                    frame.push(Slot::I(a as u16 as i32)); // i2c
                }
                0x93 => {
                    let a = frame.pop_i()?;
                    frame.push(Slot::I(a as i16 as i32)); // i2s
                }
                // 比较：lcmp / fcmpl / fcmpg / dcmpl / dcmpg 都压成 int
                0x94 | 0x95 | 0x96 | 0x97 | 0x98 => {
                    let nan = if matches!(op, 0x95 | 0x97) { -1 } else { 1 };
                    let b = frame.pop()?.as_f64();
                    let a = frame.pop()?.as_f64();
                    let r = if a > b {
                        1
                    } else if a < b {
                        -1
                    } else if a == b {
                        0
                    } else {
                        nan // NaN
                    };
                    frame.push(Slot::I(r));
                }
                // 一元条件跳转（偏移相对操作码地址）
                0x99 | 0x9a | 0x9b | 0x9c | 0x9d | 0x9e => {
                    let op_pc = pc - 1;
                    let off = i16::from_be_bytes([body[pc], body[pc + 1]]) as isize;
                    pc += 2;
                    let v = frame.pop_i()?;
                    let take = match op {
                        0x99 => v == 0,
                        0x9a => v != 0,
                        0x9b => v < 0,
                        0x9c => v >= 0,
                        0x9d => v > 0,
                        _ => v <= 0,
                    };
                    if take {
                        pc = (op_pc as isize + off) as usize;
                    }
                }
                // 二元整数/引用比较跳转
                0x9f..=0xa4 | 0xa5 | 0xa6 => {
                    let op_pc = pc - 1;
                    let off = i16::from_be_bytes([body[pc], body[pc + 1]]) as isize;
                    pc += 2;
                    let b = frame.pop_i()?;
                    let a = frame.pop_i()?;
                    let take = match op {
                        0x9f | 0xa5 => a == b,
                        0xa0 | 0xa6 => a != b,
                        0xa1 => a < b,
                        0xa2 => a >= b,
                        0xa3 => a > b,
                        _ => a <= b,
                    };
                    if take {
                        pc = (op_pc as isize + off) as usize;
                    }
                }
                0xa7 => {
                    let op_pc = pc - 1;
                    let off = i16::from_be_bytes([body[pc], body[pc + 1]]) as isize;
                    pc = (op_pc as isize + off) as usize;
                }
                // tableswitch：default / low / high / 每个值一个 4 字节偏移
                // padding 让 default 字段落在「相对 Code 起点」的 4 字节边界上
                0xaa => {
                    let op_pc = pc - 1;
                    while pc % 4 != 0 {
                        pc += 1;
                    }
                    let rd = |at: usize| i32::from_be_bytes([body[at], body[at + 1], body[at + 2], body[at + 3]]);
                    let default = rd(pc);
                    let low = rd(pc + 4);
                    let high = rd(pc + 8);
                    let key = frame.pop_i()?;
                    let off = if key >= low && key <= high {
                        rd(pc + 12 + (key - low) as usize * 4)
                    } else {
                        default
                    };
                    pc = (op_pc as isize + off as isize) as usize;
                }
                // lookupswitch：default / npairs / (key, offset)* —— 按 key 升序，二分查找
                0xab => {
                    let op_pc = pc - 1;
                    while pc % 4 != 0 {
                        pc += 1;
                    }
                    let rd = |at: usize| i32::from_be_bytes([body[at], body[at + 1], body[at + 2], body[at + 3]]);
                    let default = rd(pc);
                    let n = rd(pc + 4).max(0) as usize;
                    let key = frame.pop_i()?;
                    let mut off = default;
                    let (mut lo, mut hi) = (0usize, n);
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let at = pc + 8 + mid * 8;
                        let k = rd(at);
                        if k == key {
                            off = rd(at + 4);
                            break;
                        } else if k < key {
                            lo = mid + 1;
                        } else {
                            hi = mid;
                        }
                    }
                    pc = (op_pc as isize + off as isize) as usize;
                }
                0xac => return Ok(Slot::I(frame.pop_i()?)),
                0xad => return Ok(Slot::L(frame.pop()?.as_i64())),
                0xae => return Ok(Slot::Fl(frame.pop()?.as_f32())), // freturn
                0xaf => return Ok(Slot::F(frame.pop()?.as_f64())),
                0xb0 => return Ok(Slot::I(frame.pop_i()?)),
                0xb1 => return Ok(Slot::I(0)),
                // athrow：沿**本方法**的异常表找 handler；本帧没有就带着异常对象往上抛
                0xbf => {
                    let op_pc = pc - 1;
                    // 抛出点：栈轨迹里这一帧的行号 = `throw` 所在行
                    super::stacktrace::set_pc(op_pc);
                    let h = frame.pop_i()?;
                    match self.find_handler(&cf, &exc_table, op_pc, h)? {
                        Some(target) => {
                            pc = target;
                            // handler 入口的约定：操作数栈上就是那个异常对象
                            frame.stack.clear();
                            frame.push(Slot::I(h));
                        }
                        None => return Err(thrown(h)),
                    }
                }
                // iaload / laload / faload / daload / aaload / baload / caload / saload
                // （元素槽本身带宽度，读出来再按指令语义归一）
                0x2e | 0x2f | 0x30 | 0x31 | 0x32 | 0x33 | 0x34 | 0x35 => {
                    let idx = frame.pop_i()?;
                    let arr = frame.pop_i()?;
                    let v = if arr < 0 {
                        self.sh.heap.get(arr, idx as usize).unwrap_or(Slot::I(0))
                    } else {
                        Slot::I(0)
                    };
                    frame.push(match op {
                        0x2f => Slot::L(v.as_i64()),   // laload
                        0x31 => Slot::F(v.as_f64()),   // daload
                        0x30 => Slot::Fl(v.as_f32()),  // faload
                        // **窄类型读出来必须按 JVMS 归一**：`baload` 符号扩展、
                        // `caload` 零扩展、`saload` 符号扩展。
                        // 少了这三条，`ba[i] < 0` 永远为假 —— 后果很隐蔽：
                        // `StringCoding.countPositives`（用它判断「整段是不是纯 ASCII」）
                        // 恒返回 `len`，于是 `Files.readString` / `String.newStringNoRepl`
                        // 把 UTF-8 字节**逐个当成 Latin-1 字符**（实测
                        // `Files.readString(p, UTF_8)` 打出 30 个字符、首字符 0xE4，
                        // 真 JDK 是 16 个字符、首字符 0x4E2D）。
                        0x33 => Slot::I((v.as_i32() as u8) as i8 as i32), // baload
                        0x34 => Slot::I(v.as_i32() as u16 as i32),        // caload
                        0x35 => Slot::I((v.as_i32() as u16) as i16 as i32), // saload
                        _ => Slot::I(v.as_i32()),      // iaload / aaload
                    });
                }
                // iastore / lastore / fastore / dastore / aastore / bastore / castore / sastore
                0x4f | 0x50 | 0x51 | 0x52 | 0x53 | 0x54 | 0x55 | 0x56 => {
                    let v = frame.pop()?;
                    let idx = frame.pop_i()?;
                    let arr = frame.pop_i()?;
                    if arr < 0 && idx >= 0 {
                        let v = match op {
                            0x50 => Slot::L(v.as_i64()),  // lastore
                            0x52 => Slot::F(v.as_f64()),  // dastore
                            0x51 => Slot::Fl(v.as_f32()), // fastore
                            // 窄类型写进去要**截断**（JVMS 6.5）：`bastore` 只留低 8 位，
                            // `castore`/`sastore` 只留低 16 位。不截断的话，写一个
                            // 超过范围的值会让同一格读回来的结果和 HotSpot 不一致
                            // （与 `baload`/`caload`/`saload` 的归一是一对）。
                            0x54 => Slot::I((v.as_i32() as u8) as i8 as i32),      // bastore
                            0x55 => Slot::I(v.as_i32() as u16 as i32),             // castore
                            0x56 => Slot::I((v.as_i32() as u16) as i16 as i32),    // sastore
                            _ => Slot::I(v.as_i32()),     // iastore / aastore
                        };
                        // 引用元素写卡表（GC 用），基本类型直接写
                        if op == 0x53 {
                            self.sh.heap.set_ref(arr, idx as usize, v);
                        } else {
                            self.sh.heap.set(arr, idx as usize, v);
                        }
                    }
                }
                // arraylength
                0xbe => {
                    let arr = frame.pop_i()?;
                    let n = self.sh.heap.array_len(arr).unwrap_or(0) as i32;
                    frame.push(Slot::I(n));
                }
                // newarray：操作数是元素类型码（10 = int / 11 = long / 7 = double ...）
                0xbc => {
                    let atype = body[pc];
                    pc += 1;
                    let n = frame.pop_i()?;
                    if n < 0 {
                        return err("数组长度为负");
                    }
                    let (desc, zero) = match atype {
                        11 => ("[J", Slot::L(0)),
                        7 => ("[D", Slot::F(0.0)),
                        6 => ("[F", Slot::Fl(0.0)),
                        4 => ("[Z", Slot::I(0)),
                        5 => ("[C", Slot::I(0)),
                        8 => ("[B", Slot::I(0)),
                        9 => ("[S", Slot::I(0)),
                        _ => ("[I", Slot::I(0)),
                    };
                    match self.sh.heap.alloc_array(n as usize, desc, zero) {
                        Some(h) => frame.push(Slot::I(h)),
                        None => return err("OutOfMemoryError: 数组堆用尽"),
                    }
                }
                // anewarray：操作数是**元素类**的常量池索引
                0xbd => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let comp = class_name_of(&cf, idx)?;
                    let desc = if comp.starts_with('[') {
                        format!("[{comp}") // 元素本身是数组：[[I 之类
                    } else {
                        format!("[L{comp};")
                    };
                    let n = frame.pop_i()?;
                    if n < 0 {
                        return err("数组长度为负");
                    }
                    match self.sh.heap.alloc_array(n as usize, &desc, Slot::I(0)) {
                        Some(h) => frame.push(Slot::I(h)),
                        None => return err("OutOfMemoryError: 数组堆用尽"),
                    }
                }
                // new：字段个数按**被创建的那个类**的布局（含父类字段）算
                0xbb => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    // 调用点缓存（见 `NewSite`）：类名不再每次分配、`<clinit>` 不再每次查
                    let nkey = (std::sync::Arc::as_ptr(&cf) as usize, idx);
                    let cached_new = SITE_NEW.with(|c| c.borrow().get(&nkey).map(std::rc::Rc::clone));
                    let site = match cached_new {
                        Some(s) => s,
                        None => {
                            let name: &'static str =
                                Box::leak(class_name_of(&cf, idx)?.into_boxed_str());
                            let s = std::rc::Rc::new(NewSite {
                                _cf: std::sync::Arc::clone(&cf),
                                name,
                                inited: std::cell::Cell::new(false),
                            });
                            SITE_NEW.with(|c| c.borrow_mut().insert(nkey, std::rc::Rc::clone(&s)));
                            s
                        }
                    };
                    let cls = site.name;
                    // 首次 new 这个类时先跑它的 <clinit>
                    if !site.inited.get() {
                        self.ensure_init(cls)?;
                        site.inited.set(true);
                    }
                    // 槽数**不能**按调用点缓存（并行加载下「类在表里」不等于「布局定稿」）
                    let n = self.store.field_count(cls);
                    match self.sh.heap.new_object(vec![Slot::I(0); n.max(1)], cls) {
                        Some(h) => frame.push(Slot::I(h)),
                        None => return err("OutOfMemoryError: 对象堆用尽"),
                    }
                }
                // getfield：字段槽自带宽度，直接按槽取
                0xb4 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let info = self.site_ref(&cf, idx)?;
                    let obj = frame.pop_i()?;
                    let v = if is_str_handle(obj) {
                        // 字符串堆句柄的字段视图（`ldc "abc"` 的 `value`/`coder`/`hash`）
                        let f = self.str_slot_cached(&info)?;
                        let slots = self.str_field_slots(obj);
                        slots.get(f).copied().unwrap_or(Slot::I(0))
                    } else {
                        // 偏移沿引用类的继承链解析；布局是「父类在前」，子类偏移与父类一致
                        let f = match self.field_off_cached(&info) {
                            Some(f) => f,
                            // 没解析到（或类表刚变过）→ 走原路，语义（含 Err）完全一致
                            None => self.field_offset(obj, &info.class, &info.name)?,
                        };
                        self.sh.heap.get(obj, f).unwrap_or(Slot::I(0))
                    };
                    frame.push(v);
                }
                // putfield
                0xb5 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let info = self.site_ref(&cf, idx)?;
                    let value = frame.pop()?;
                    let obj = frame.pop_i()?;
                    if is_str_handle(obj) {
                        // 只有缓存字段（hash/hashIsZero）会被真字节码写；写 value/coder 不影响文本
                        let f = self.str_slot_cached(&info)?;
                        self.str_field_write(obj, f, value);
                    } else {
                        let f = match self.field_off_cached(&info) {
                            Some(f) => f,
                            None => self.field_offset(obj, &info.class, &info.name)?,
                        };
                        if is_ref_desc(&info.desc) {
                            self.sh.heap.set_ref(obj, f, value);
                        } else {
                            self.sh.heap.set(obj, f, value);
                        }
                    }
                }
                // instanceof：压 0/1
                0xc1 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let target = class_name_of(&cf, idx)?;
                    let v = frame.pop_i()?;
                    frame.push(Slot::I(if self.is_instance(v, &target) { 1 } else { 0 }));
                }
                // checkcast：不匹配就按 ClassCastException 退出（本 VM 还没有异常机制）
                0xc0 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let target = class_name_of(&cf, idx)?;
                    let v = frame.pop_i()?;
                    if v != 0 && !self.is_instance(v, &target) {
                        // 句柄已经失效（`hdr` 查不到）时 `type_name` 只会说 "null" ——
                        // 那是**悬空句柄**（活对象被回收），不是真的 null。带上调用链和
                        // 原始句柄值，两者一眼可分。
                        super::stacktrace::set_pc(pc - 3); // 行号要停在 checkcast 这一条
                        let mut where_ = String::new();
                        if self.sh.heap.hdr(v).is_none() {
                            where_.push_str(&format!("（悬空句柄 h={v}，对象已被回收）"));
                        }
                        for fr in super::stacktrace::frames_current().iter().take(8) {
                            where_.push_str(&format!(
                                "\n\tat {}.{}({}:{})",
                                fr.class.replace('/', "."),
                                fr.method,
                                fr.source.clone().unwrap_or_else(|| "?".into()),
                                fr.line
                            ));
                        }
                        return err(format!(
                            "ClassCastException: {} cannot be cast to {}{}",
                            self.type_name(v),
                            target.replace('/', "."),
                            where_
                        ));
                    }
                    frame.push(Slot::I(v));
                }
                // monitorenter / monitorexit（对象监视器；语义见 vm/thread.rs）
                0xc2 | 0xc3 => {
                    let op_pc = pc - 1;
                    let obj = frame.pop_i()?;
                    let r = if op == 0xc2 {
                        self.monitor_enter(obj)
                    } else {
                        self.monitor_exit(obj)
                    };
                    // IllegamMonitorStateException / NullPointerException 是**本方法**里抛的，
                    // 要先过自己的异常表（javac 给 synchronized 块生成的 monitorexit 就在
                    // handler 里 —— 这一步没做对，异常路径上的锁就永远不释放）
                    if let Err(e) = r {
                        let (target, h) = self.local_handler(&cf, &exc_table, op_pc, e)?;
                        pc = target;
                        frame.stack.clear();
                        frame.push(Slot::I(h));
                    }
                }
                // ifnull / ifnonnull
                0xc6 | 0xc7 => {
                    let op_pc = pc - 1;
                    let off = i16::from_be_bytes([body[pc], body[pc + 1]]) as isize;
                    pc += 2;
                    let v = frame.pop_i()?;
                    let take = if op == 0xc6 { v == 0 } else { v != 0 };
                    if take {
                        pc = (op_pc as isize + off) as usize;
                    }
                }
                // 移位：int 版本右操作数取低 5 位、long 版本取低 6 位（与 JVM 一致）
                0x78 | 0x7a | 0x7c => {
                    let n = frame.pop_i()? & 31;
                    let a = frame.pop_i()?;
                    let r = match op {
                        0x78 => a.wrapping_shl(n as u32),                  // ishl
                        0x7a => a.wrapping_shr(n as u32),                  // ishr
                        _ => ((a as u32) >> n) as i32,                     // iushr
                    };
                    frame.push(Slot::I(r));
                }
                0x79 | 0x7b | 0x7d => {
                    let n = frame.pop_i()? & 63;
                    let a = frame.pop()?.as_i64();
                    let r = match op {
                        0x79 => a.wrapping_shl(n as u32),                  // lshl
                        0x7b => a.wrapping_shr(n as u32),                  // lshr
                        _ => ((a as u64) >> n) as i64,                     // lushr
                    };
                    frame.push(Slot::L(r));
                }
                // 位运算
                0x7e | 0x80 | 0x82 => {
                    let b = frame.pop_i()?;
                    let a = frame.pop_i()?;
                    let r = match op {
                        0x7e => a & b, // iand
                        0x80 => a | b, // ior
                        _ => a ^ b,    // ixor
                    };
                    frame.push(Slot::I(r));
                }
                0x7f | 0x81 | 0x83 => {
                    let b = frame.pop()?.as_i64();
                    let a = frame.pop()?.as_i64();
                    let r = match op {
                        0x7f => a & b, // land
                        0x81 => a | b, // lor
                        _ => a ^ b,    // lxor
                    };
                    frame.push(Slot::L(r));
                }
                // getstatic：静态字段（System.out 仍是 PrintStream 占位，println 走内建）
                0xb2 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let info = self.site_ref(&cf, idx)?;
                    if info.class == "java/lang/System" && info.name == "out" {
                        // 先把 `System` 自己初始化好：`SharedSecrets` 的 `JavaLangAccess`
                        // 就是它 `<clinit>` 里挂上去的（`StringJoiner.toString` 要用）
                        self.ensure_init("java/lang/System")?;
                        // 真读静态字段：`System.setOut(x)` 之后这里必须拿到新值。
                        // （原来无条件压 0，`setOut` 看起来"成功"但读取端永远拿不到。）
                        let v = lk(&self.sh.statics)
                            .get(&("java/lang/System".to_string(), "out".to_string()))
                            .copied()
                            .unwrap_or(Slot::I(0));
                        frame.push(v);
                    } else if let Some(v) = self.builtin_static_field(&info.class, &info.name) {
                        // 内建常量：`Integer.MAX_VALUE` / `Long.MIN_VALUE`（没有 .class）
                        frame.push(v);
                    } else {
                        // 静态字段解析要**沿继承链**走（JVMS 5.4.3.2）：javac 对从父类继承来的
                        // 静态字段可能仍用**子类名**做字段引用的 owner —— JDK 25 的
                        // `StackFrameInfo.declaringClass()` 读的就是 `StackFrameInfo.JLIA`，
                        // 而 `JLIA` 声明在父类 `java/lang/ClassFrameInfo` 上。只按
                        // `info.class` 建表会读到 0，然后拿 null 当接收者去调
                        // `JavaLangInvokeAccess.getDeclaringClass` → 运行时找不到方法。
                        //
                        // 快路径：键在表里就直接读（绝大多数读是已初始化/已镜像过的）。
                        let direct = lk(&self.sh.statics)
                            .get(&(info.class.clone(), info.name.clone()))
                            .copied();
                        match direct {
                            Some(v) => frame.push(v),
                            None => {
                                let owner = self.static_owner(&info.class, &info.name);
                                self.ensure_init(&owner)?;
                                let v = lk(&self.sh.statics)
                                    .get(&(owner.clone(), info.name.clone()))
                                    .copied()
                                    .unwrap_or(Slot::I(0));
                                // 零值静态字段也要留个槽：否则每次读都要走一遍继承链解析
                                lk(&self.sh.statics)
                                    .entry((info.class.clone(), info.name.clone()))
                                    .or_insert(v);
                                frame.push(v);
                            }
                        }
                    }
                }
                // putstatic
                0xb3 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let info = self.site_ref(&cf, idx)?;
                    let value = frame.pop()?;
                    // 写也要落到**声明类**上，并给「用子类名引用」的那把键留镜像
                    let owner = self.static_owner(&info.class, &info.name);
                    self.ensure_init(&owner)?;
                    lk(&self.sh.statics).insert((owner.clone(), info.name.clone()), value);
                    if owner != info.class {
                        lk(&self.sh.statics).insert((info.class.clone(), info.name.clone()), value);
                    }
                }
                // invokevirtual / invokeinterface：println 走内建；
                // 其余按**接收者的运行时类**虚分派（接口与类走同一条路，只是指令编码多两字节）
                0xb6 | 0xb9 => {
                    let op_pc = pc - 1;
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    if op == 0xb9 {
                        pc += 2; // count + 保留字节
                    }
                    let info = self.site_ref(&cf, idx)?;
                    if Self::is_invoke_api_class(&info.class) {
                        // `java.lang.invoke.*` 的方法在 OpenJDK 里靠 VM 内部原语实现
                        // （`invokeBasic` / `linkToVirtual` / 隐藏类……），执行它们的
                        // 字节码不可能跑通 —— 一律由 VM 接管（`vm::indy`）。
                        let argc = count_args(&info.desc)?;
                        let mut args = Vec::with_capacity(argc);
                        for _ in 0..argc {
                            args.push(frame.pop()?);
                        }
                        args.reverse();
                        let recv = frame.pop()?.as_i32();
                        let r = self.invoke_api(0, &info.class, &info.name, &info.desc, recv, &args);
                        let r = match r {
                            Ok(Some(v)) => Ok(v),
                            Ok(None) => self.invoke_api_fallback(&info, Some(recv), &args, op_pc),
                            Err(e) => Err(e),
                        };
                        match call_or_handle!(r, op_pc) {
                            CallOutcome::Value(v) => {
                                if !info.desc.ends_with(")V") {
                                    frame.push(v);
                                }
                            }
                            CallOutcome::Handler(t, h) => {
                                pc = t;
                                frame.stack.clear();
                                frame.push(Slot::I(h));
                            }
                        }
                        continue;
                    }
                    // `println` 只用**内建**实现顶替独立模式（没有 java.base / PrintStream 的替身）。
                    // 真字节码模式下必须让真 `java.io.PrintStream` 跑：两条不同的输出通道
                    // （内建 println 进 VM 输出缓冲、`PrintStream.print` 直接写 fd 1）
                    // 会把顺序打乱 —— `print("x\n"); println("y")` 会先出 y 再出 x。
                    if info.name == "println" && !self.store.has_class("java/io/PrintStream") {
                        let arg = frame.pop()?;
                        let _recv = frame.pop()?;
                        // `println(Object)` / `println(String)` 要按 java 的语义取文本：
                        // `String.valueOf(x)` → 有覆写的 `toString()` 就**执行它**
                        // （真跑 PrintStream 的字节码会把整条 java.base 拖进来，所以这里
                        //  直接按同样的语义算，`value_text` 就是 `String.valueOf`）
                        let text = match info.desc.as_str() {
                            "(Ljava/lang/Object;)V" | "(Ljava/lang/String;)V" => {
                                self.value_text(arg.as_i32())?
                            }
                            _ => self.println_text(&info.desc, arg),
                        };
                        self.println(text);
                    } else if info.class == "java/lang/StringBuilder"
                        && (!self.store.has_class("java/lang/StringBuilder") || self.sh.prefer_builtin)
                    {
                        // 内置：字符串拼接。**只在 StringBuilder 没有字节码时**才走这里 ——
                        // 一旦 java.base 在 classpath 上，就该执行它自己的 append/toString。
                        if info.name == "append" {
                            let arg = frame.pop()?;
                            let recv = frame.pop()?.as_i32();
                            let v = self.sb_append(recv, &info.desc, arg)?;
                            frame.push(v); // append 返回 this，调用点会 pop 掉
                        } else if info.name == "toString" {
                            let recv = frame.pop()?.as_i32();
                            let h = self.sb_finish(recv);
                            frame.push(Slot::I(h));
                        } else {
                            return Err(VmError {
                                msg: format!("java.lang.StringBuilder 没有方法 `{}`", info.name),
                                exc: None,
                            });
                        }
                    } else {
                        // 参数与接收者直接从操作数栈上取：`argc + 1 <= 8` 时不分配任何 Vec。
                        // （原来这里 `Vec::with_capacity` + `vec![recv]` + `extend` 是三次分配。）
                        let argc = info.argc()?;
                        let nargs = argc + 1;
                        let mut sbuf = [Slot::I(0); 8];
                        let mut vbuf: Vec<Slot> = Vec::new();
                        let all: &[Slot] = if nargs <= sbuf.len() {
                            for i in (0..nargs).rev() {
                                sbuf[i] = frame.pop()?;
                            }
                            &sbuf[..nargs]
                        } else {
                            vbuf.reserve(nargs);
                            for _ in 0..nargs {
                                vbuf.push(frame.pop()?);
                            }
                            vbuf.reverse();
                            &vbuf
                        };
                        let recv = all[0];
                        // 注解代理对象：运行时类 = 注解接口、成员值在 `Vm::ann_objs` 里。
                        // 接口方法都是抽象的，所以必须在方法解析之前分流（与 lambda 同一层）。
                        if !lk(&self.sh.ann_objs).is_empty() {
                            if let Some(r) = self.ann_dispatch(recv.as_i32(), &info.name, &info.desc) {
                                match call_or_handle!(r, op_pc) {
                                    CallOutcome::Value(v) => {
                                        if !info.desc.ends_with(")V") {
                                            frame.push(v);
                                        }
                                    }
                                    CallOutcome::Handler(t, h) => {
                                        pc = t;
                                        frame.stack.clear();
                                        frame.push(Slot::I(h));
                                    }
                                }
                                continue;
                            }
                        }
                        // lambda 对象：它的类是 VM 生成的合成类，**不在类表里** ——
                        // 必须在方法解析之前分流（接口方法是抽象的，解析到了也执行不了）。
                        if let Some(r) = self.lambda_dispatch(recv.as_i32(), &info.name, &info.desc, &all[1..]) {
                            match call_or_handle!(r, op_pc) {
                                CallOutcome::Value(v) => {
                                    if !info.desc.ends_with(")V") {
                                        frame.push(v);
                                    }
                                }
                                CallOutcome::Handler(t, h) => {
                                    pc = t;
                                    frame.stack.clear();
                                    frame.push(Slot::I(h));
                                }
                            }
                            continue;
                        }
                        // 从对象的**实际类**开始解析（这就是覆写的分派点）；
                        // 拿不到运行时类才退回引用类。**真字节码优先**：
                        // `VMLESS_PREFER_BUILTIN=1` 只是「先问内建表」，内建表没有的还能走字节码。
                        // 接收者的运行时类：`heap.class_of` 只克隆 `Arc<str>`（不复制字符串），
                        // 顺带把它的**指针**当类身份喂给单态内联缓存。
                        let rv = recv.as_i32();
                        let cname: Option<std::sync::Arc<str>> = if rv == 0 {
                            None
                        } else if rv >= STR_BASE {
                            // 字符串句柄（`ldc "abc"` 给的那一坨）的运行时类**就是**
                            // `java/lang/String`。少了这一条，`CharSequence.toString()`
                            // 这种按接口声明的调用会解析到 `Object.toString` 上 ——
                            // `String.contains` 的字节码正是 `indexOf(s.toString())`，
                            // 于是它静默给出 false。
                            Some(arc_jlstring())
                        } else {
                            match self.sh.heap.class_of(rv) {
                                // 数组没有自己的方法，全部继承自 Object
                                Some(a) if a.starts_with(0x5b as char) => Some(arc_jlobject()),
                                Some(a) => Some(a),
                                None => None,
                            }
                        };
                        let ckey = match &cname {
                            Some(a) if rv != 0 => std::sync::Arc::as_ptr(a) as *const u8 as usize,
                            _ => 0,
                        };
                        // 打在 null 上的 invokevirtual/invokeinterface = **NullPointerException**
                        // （JVMS 6.5）。以前这里直接掉进内建兜底：「内建方法 remove 未实现
                        // （接收者 null）」—— 与事实无关、**还丢栈**，真正要查的
                        // 「哪个字段本该非 null」完全看不见。
                        // 只在引用类**真有字节码**时抛：独立模式下 `System.out` 之类就是
                        // 句柄 0 的占位对象，那条老路（内建表）必须留着。
                        if recv.as_i32() == 0 && self.store.has_class(&info.class) {
                            // 行号要在**抛出点**上：帧槽里的 pc 是上次调用点同步的，
                            // 这里不补就会指到「上一条 invoke」（离真相差好几行）。
                            super::stacktrace::set_pc(op_pc);
                            // 消息里**不再**嵌帧：未捕获异常的栈现在由 `exc_text` 统一
                            // 按 HotSpot 的格式打（嵌进 message 会重复一层，而且
                            // `getMessage()` 也不该带栈）。
                            let e = self.java_exception(
                                "java/lang/NullPointerException",
                                format!(
                                    "Cannot invoke \"{}.{}{}\" because the receiver is null",
                                    info.class.replace('/', "."),
                                    info.name,
                                    info.desc
                                ),
                            );
                            match call_or_handle!(Err(e), op_pc) {
                                CallOutcome::Value(_) => unreachable!("Err 不会给出值"),
                                CallOutcome::Handler(t, h) => {
                                    pc = t;
                                    frame.stack.clear();
                                    frame.push(Slot::I(h));
                                    continue;
                                }
                            }
                        }
                        let target = self.vtarget(&info, cname.as_ref(), ckey);
                        if self.sh.prefer_builtin {
                            // 只有内建实现**真给出值**才用它；给不出（或者这种接收者内建压根不支持）
                            // 就落到真字节码 —— 这个开关是「优先」不是「覆盖」，绝不能把路走死。
                            if let Some(Ok(v)) =
                                self.builtin_virtual(&info.class, &info.name, &info.desc, recv.as_i32(), &all[1..])
                            {
                                frame.push(v);
                                continue;
                            }
                        }
                        let target = match target {
                            Some(t) => t,
                            None => {
                                // 兜底：这个类**没有字节码**（独立模式下的 java.* 替身），查内建表。
                                // 类字节码在的时候不会走到这里 —— resolve_method 会先命中。
                                let builtin = self.builtin_virtual(&info.class, &info.name, &info.desc, recv.as_i32(), &all[1..]);
                                match builtin {
                                    Some(Ok(v)) => {
                                        frame.push(v);
                                        continue;
                                    }
                                    // 内建表自己报的错也补上**调用点**：只写「内建方法 remove 未实现」
                                    // 时根本看不出是哪个类漏了加载（AWT 里一次就卡半天）。
                                    Some(Err(e)) => {
                                        return Err(VmError {
                                            msg: format!(
                                                "{}（调用点 {}.{}{}，接收者 {}）",
                                                e.msg,
                                                info.class,
                                                info.name,
                                                info.desc,
                                                self.type_name(recv.as_i32())
                                            ),
                                            exc: e.exc,
                                        })
                                    }
                                    None => {
                                        return Err(VmError {
                                            msg: format!(
                                                "找不到方法 {}.{}{}（调用点 {}，接收者 {}）",
                                                info.class,
                                                info.name,
                                                info.desc,
                                                self.store
                                                    .name_of_index(self.last_caller)
                                                    .unwrap_or_else(|| "?".into()),
                                                self.type_name(recv.as_i32())
                                            ),
                                            exc: None,
                                        })
                                    }
                                }
                            }
                        };
                        super::stacktrace::set_pc(op_pc);
                        self.depth += 1;
                        self.last_caller = class;
                        let r = call_or_handle!(self.execute(target.0, target.1, &info.desc, &all, true), op_pc);
                        self.depth -= 1;
                        match r {
                            CallOutcome::Value(v) => {
                                if !info.desc.ends_with(")V") {
                                    frame.push(v);
                                }
                            }
                            CallOutcome::Handler(t, h) => {
                                pc = t;
                                frame.stack.clear();
                                frame.push(Slot::I(h));
                                continue;
                            }
                        }
                    }
                }
                // invokespecial：构造器 / super.m()，从**引用类**（不是运行时类）开始解析
                0xb7 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let info = self.site_ref(&cf, idx)?;
                    let argc = count_args(&info.desc)?;
                    let mut args = Vec::with_capacity(argc + 1);
                    for _ in 0..argc {
                        args.push(frame.pop()?);
                    }
                    let obj = frame.pop()?;
                    args.reverse();
                    let mut all = vec![obj];
                    all.extend(args);
                    if Self::is_invoke_api_class(&info.class) {
                        // `new ConstantCallSite(mh)` / `MutableCallSite.setTarget` 之类：
                        // 由 VM 接管（构造出来的对象要登记进调用点表）
                        let op_pc = pc - 3;
                        let r = self.invoke_api(2, &info.class, &info.name, &info.desc, all[0].as_i32(), &all[1..]);
                        let r = match r {
                            Ok(Some(v)) => Ok(v),
                            Ok(None) => self.invoke_api_fallback(&info, Some(all[0].as_i32()), &all[1..], op_pc),
                            Err(e) => Err(e),
                        };
                        match call_or_handle!(r, op_pc) {
                            CallOutcome::Value(v) => {
                                if !info.desc.ends_with(")V") {
                                    frame.push(v);
                                }
                            }
                            CallOutcome::Handler(t, h) => {
                                pc = t;
                                frame.stack.clear();
                                frame.push(Slot::I(h));
                            }
                        }
                        continue;
                    }
                    self.last_caller = class;
                    match self.resolved_target(&info) {
                        Some((ci, mi)) => {
                            let op_pc = pc - 3;
                            super::stacktrace::set_pc(op_pc);
                            self.depth += 1;
                            let r = self.execute(ci, mi, &info.desc, &all, true);
                            self.depth -= 1;
                            // 构造器/父类方法抛的异常要先过**本方法**的异常表：
                            // `new FileInputStream("不存在")` 里 `<init>` 抛 FileNotFoundException，
                            // 调用方的 try/catch 必须接得住。（原来 `<init>` 那一支把 `r` 直接丢掉，
                            // 异常静默消失，程序带着半初始化对象继续跑。）
                            match call_or_handle!(r, op_pc) {
                                CallOutcome::Value(v) => {
                                    // super.m() / 普通方法有返回值：压回栈上
                                    if info.name != "<init>" && !info.desc.ends_with(")V") {
                                        frame.push(v);
                                    }
                                }
                                CallOutcome::Handler(t, h) => {
                                    pc = t;
                                    frame.stack.clear();
                                    frame.push(Slot::I(h));
                                }
                            }
                        }
                        None if info.desc == "(Ljava/lang/String;)V" => {
                            // java.* 的构造器（`new RuntimeException("boom")`）我们没有字节码：
                            // 只把消息记在异常对象上，打印未捕获异常时才用得到。
                            if let Some(msg) = all.get(1) {
                                lk(&self.sh.exc_msgs).insert(all[0].as_i32(), msg.as_i32());
                            }
                        }
                        None if info.name == "<init>" => {}
                        None => {
                            return Err(VmError {
                                msg: format!("找不到方法 {}.{}{}", info.class, info.name, info.desc),
                                exc: None,
                            })
                        }
                    }
                }
                // invokestatic：沿引用类的继承链解析（静态方法也算继承来的）
                0xb8 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let info = self.site_ref(&cf, idx)?;
                    // `java.lang.invoke` 的静态工厂由 VM 接管 —— **必须在 `<clinit>` 之前**：
                    // 那些类的静态初始化本身就是 VM 内部原语的组合（`MethodType.<clinit>`
                    // 会去碰 `MemberName`），执行它等于把整条 java.lang.invoke 拖进来。
                    if Self::is_invoke_api_class(&info.class) {
                        let argc = count_args(&info.desc)?;
                        let mut args = Vec::with_capacity(argc);
                        for _ in 0..argc {
                            args.push(frame.pop()?);
                        }
                        args.reverse();
                        let op_pc = pc - 3;
                        let r = self.invoke_api(1, &info.class, &info.name, &info.desc, 0, &args);
                        let r = match r {
                            Ok(Some(v)) => Ok(v),
                            Ok(None) => self.invoke_api_fallback(&info, None, &args, op_pc),
                            Err(e) => Err(e),
                        };
                        match call_or_handle!(r, op_pc) {
                            CallOutcome::Value(v) => {
                                if !info.desc.ends_with(")V") {
                                    frame.push(v);
                                }
                            }
                            CallOutcome::Handler(t, h) => {
                                pc = t;
                                frame.stack.clear();
                                frame.push(Slot::I(h));
                            }
                        }
                        continue;
                    }
                    // 首次调用这个类的静态方法时先跑它的 <clinit>
                    if !info.inited.get() {
                        self.ensure_init(&info.class)?;
                        info.inited.set(true);
                    }
                    let argc = count_args(&info.desc)?;
                    let mut args = Vec::with_capacity(argc);
                    for _ in 0..argc {
                        args.push(frame.pop()?);
                    }
                    args.reverse();
                    let op_pc = pc - 3;
                    // 真字节码优先；`VMLESS_PREFER_BUILTIN=1` 只是「先问内建表」，
                    // 内建表没有的照样走字节码（不是硬覆盖）
                    if self.sh.prefer_builtin {
                        if let Some(Ok(v)) = self.builtin_static(&info.class, &info.name, &info.desc, &args) {
                            if !info.desc.ends_with(")V") {
                                frame.push(v);
                            }
                            continue;
                        }
                    }
                    let target = self.resolved_target(&info);
                    let Some(target) = target else {
                        // 兜底：这个类**没有字节码**时的内建替身（`Integer.parseInt` / `System.arraycopy` …）
                        let Some(v) = self.builtin_static(&info.class, &info.name, &info.desc, &args) else {
                            return Err(VmError {
                                msg: format!("找不到方法 {}{}", info.name, info.desc),
                                exc: None,
                            });
                        };
                        match call_or_handle!(v, op_pc) {
                            CallOutcome::Value(v) => {
                                if !info.desc.ends_with(")V") {
                                    frame.push(v);
                                }
                            }
                            CallOutcome::Handler(t, h) => {
                                pc = t;
                                frame.stack.clear();
                                frame.push(Slot::I(h));
                                continue;
                            }
                        }
                        continue;
                    };
                    self.last_caller = class;
                    super::stacktrace::set_pc(op_pc);
                    self.depth += 1;
                    let r = call_or_handle!(self.execute(target.0, target.1, &info.desc, &args, false), op_pc);
                    self.depth -= 1;
                    match r {
                        CallOutcome::Value(v) => {
                            if !info.desc.ends_with(")V") {
                                frame.push(v);
                            }
                        }
                        CallOutcome::Handler(t, h) => {
                            pc = t;
                            frame.stack.clear();
                            frame.push(Slot::I(h));
                            continue;
                        }
                    }
                }
                // multianewarray：一次建出多维数组（内层数组递归建）
                0xc5 => {
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 2;
                    let ndims = body[pc] as usize;
                    pc += 1;
                    let desc = class_name_of(&cf, idx)?;
                    let mut counts = Vec::with_capacity(ndims);
                    for _ in 0..ndims {
                        counts.push(frame.pop_i()?);
                    }
                    counts.reverse();
                    let h = self.new_multi(&desc, &counts)?;
                    frame.push(Slot::I(h));
                }
                // invokedynamic（0xba）：操作数 = 常量池索引(u2) + 两个保留字节。
                //
                // 首次执行时跑引导方法、从 `CallSite.getTarget()` 拿到目标句柄并缓存；
                // 之后**同一个调用点**直接用缓存（`Vm::indy_site_target`）。
                // 参数弹出个数按调用点自己的描述符，不看引导方法。
                0xba => {
                    let op_pc = pc - 1;
                    super::stacktrace::set_pc(op_pc);
                    let idx = u16::from_be_bytes([body[pc], body[pc + 1]]);
                    pc += 4;
                    let desc = self.indy_desc(class, idx)?;
                    let argc = count_args(&desc)?;
                    let mut args = Vec::with_capacity(argc);
                    for _ in 0..argc {
                        args.push(frame.pop()?);
                    }
                    args.reverse();
                    let target = call_or_handle!(self.indy_site_target(class, idx).map(Slot::I), op_pc);
                    let target = match target {
                        CallOutcome::Value(v) => v.as_i32(),
                        CallOutcome::Handler(t, h) => {
                            pc = t;
                            frame.stack.clear();
                            frame.push(Slot::I(h));
                            continue;
                        }
                    };
                    self.indy().indy_calls += 1;
                    let r = call_or_handle!(
                        self.invoke_mh_as(target, &desc, &args, "invokedynamic"),
                        op_pc
                    );
                    match r {
                        CallOutcome::Value(v) => {
                            if !desc.ends_with(")V") {
                                frame.push(v);
                            }
                        }
                        CallOutcome::Handler(t, h) => {
                            pc = t;
                            frame.stack.clear();
                            frame.push(Slot::I(h));
                        }
                    }
                }
                other => {
                    return err(format!("未实现的字节码 0x{other:02x} @pc={}", pc - 1));
                }
            }
        }
        Ok(Slot::I(0))
    }

    /// 多维数组：`desc` 是完整描述符（`[[I`），`counts` 每维一个长度。
    /// 内层数组先建，再塞进外层（和 JVM 的 multianewarray 语义一致）。
    fn new_multi(&mut self, desc: &str, counts: &[i32]) -> Result<i32, VmError> {
        let n = counts.first().copied().unwrap_or(0);
        if n < 0 {
            return err("数组长度为负");
        }
        let handle = match self.sh.heap.alloc_array(n as usize, desc, Slot::I(0)) {
            Some(h) => h,
            None => return err("OutOfMemoryError: 数组堆用尽"),
        };
        if counts.len() > 1 {
            let elem_desc = &desc[1..];
            for i in 0..n as usize {
                let sub = self.new_multi(elem_desc, &counts[1..])?;
                self.sh.heap.set(handle, i, Slot::I(sub));
            }
        }
        Ok(handle)
    }

    /// 字段偏移：先按引用类的继承链找，找不到再按对象的**运行时类**找
    pub(super) fn field_offset(&self, obj: i32, owner: &str, name: &str) -> Result<usize, VmError> {
        // JVMS：字段按常量池里的**声明类**（`owner`）解析。
        // 原来「先按运行时类查」会让 `getfield AbstractWatchKey.events` 也落到子类那个槽上，
        // 遮蔽就反了 —— `WindowsWatchKey.events`(Set) 遮蔽 `AbstractWatchKey.events`(List) 时，
        // `signalEvent()` 会拿到父类那个 List 槽，`List.get(int)` 打到 HashSet 上。
        self.store
            .field_index(owner, name)
            .or_else(|| self.field_index_of_object(obj, name))
            .ok_or_else(|| VmError { msg: format!("未知字段 {owner}.{name}"), exc: None })
    }

    fn field_index_of_object(&self, obj: i32, name: &str) -> Option<usize> {
        if obj <= 0 {
            return None;
        }
        let c = self.sh.heap.class_of(obj)?;
        self.store.field_index(&c, name)
    }

    /// 主循环里的同步异常处理：本方法的异常表接得住就返回 handler 偏移 + 异常句柄，
    /// 接不住就把原错误原样往外抛（交给调用方那层的 handler）。
    fn local_handler(
        &mut self,
        cf: &crate::ClassFile,
        table: &[(u16, u16, u16, u16)],
        op_pc: usize,
        e: VmError,
    ) -> Result<(usize, i32), VmError> {
        match e.exc {
            Some(h) => match self.find_handler(cf, table, op_pc, h)? {
                Some(t) => Ok((t, h)),
                None => Err(e),
            },
            None => Err(e),
        }
    }
}

/// 这个类的内部名是不是「VM 接管清单」里的那几个（**不分配字符串**，热路径友好）
fn is_override_owner(cf: &crate::ClassFile) -> bool {
    let bytes = match cf.constant_pool.get(cf.this_class as usize).and_then(|e| e.as_ref()) {
        Some(CpEntry::Class(n)) => cf.utf8(*n).unwrap_or_default(),
        _ => return false,
    };
    matches!(
        bytes,
        b"java/lang/Thread"
            | b"java/lang/ClassLoader"
            | b"jdk/internal/reflect/Reflection"
            | b"java/lang/ref/Reference"
            | b"java/lang/ref/PhantomReference"
            | b"java/lang/ref/SoftReference"
            | b"java/lang/ref/WeakReference"
            // ---- 反射层（`vm/reflect.rs`）：反射对象上的访问器语义必须由 VM 给 ----
            | b"java/lang/Class"
            | b"java/lang/reflect/AccessibleObject"
            | b"java/lang/reflect/Executable"
            | b"java/lang/reflect/Field"
            | b"java/lang/reflect/Method"
            | b"java/lang/reflect/Constructor"
            | b"java/lang/reflect/ReflectAccess"
            | b"java/util/StringJoiner"
            // `SunGraphics2D`：`drawString` 的接管点（JDK 25 的字体栈全走 FFM，到不了
            // `DrawGlyphList`；在 `SunGraphics2D` 层拦才能用 GDI 真字体画字）
            | b"sun/java2d/SunGraphics2D"
            // `Throwable.getOurStackTrace` 由 VM 接管：JDK 自己的实现会对每个
            // `StackTraceElement` 调 `computeFormat()`，而公开 7 参构造器造出来的
            // 元素 `declaringClassObject` 是 null → 每打一次异常栈就 NPE
            // （真正的异常会被这个 NPE 盖掉）。接管后直接返回我们造好的 backtrace。
            | b"java/lang/Throwable"
            | b"java/lang/reflect/ReflectAccess"
            | b"jdk/internal/access/SharedSecrets"
    )
}

/// 返回 (max_locals, 字节码, 异常表)。异常表每项 (start, end, handler, catch_type)，
/// catch_type = 0 表示 catch-all（finally）。
pub(super) fn code_of<'a>(
    cf: &crate::ClassFile,
    m: &'a crate::Member,
) -> Result<(usize, &'a [u8], Vec<(u16, u16, u16, u16)>), VmError> {
    let code_name = cf.constant_pool.iter().enumerate().find_map(|(i, e)| match e {
        Some(CpEntry::Utf8(b)) if b == b"Code" => Some(i as u16),
        _ => None,
    });
    let code_name = match code_name {
        Some(c) => c,
        None => return err("常量池里没有 Code"),
    };
    for a in &m.attributes {
        if a.name_index == code_name && a.info.len() >= 8 {
            let max_locals = u16::from_be_bytes([a.info[2], a.info[3]]) as usize;
            let code_len = u32::from_be_bytes([a.info[4], a.info[5], a.info[6], a.info[7]]) as usize;
            let start = 8;
            let end = (start + code_len).min(a.info.len());
            // 异常表紧跟在字节码后面：u2 个数 + 每项 4 个 u2
            let mut table = Vec::new();
            let mut q = end;
            if q + 2 <= a.info.len() {
                let n = u16::from_be_bytes([a.info[q], a.info[q + 1]]) as usize;
                q += 2;
                for _ in 0..n {
                    if q + 8 > a.info.len() {
                        break;
                    }
                    let rd = |i: usize| u16::from_be_bytes([a.info[i], a.info[i + 1]]);
                    table.push((rd(q), rd(q + 2), rd(q + 4), rd(q + 6)));
                    q += 8;
                }
            }
            return Ok((max_locals, &a.info[start..end], table));
        }
    }
    err("方法没有 Code 属性（抽象方法？）")
}

/// 字符串句柄的运行时类名（共享 `Arc<str>`：`Arc::as_ptr` 就是单态缓存的键，
/// 每次现造一个 `Arc::from` 会多一次分配，键也不稳定）
fn arc_jlstring() -> std::sync::Arc<str> {
    static S: std::sync::OnceLock<std::sync::Arc<str>> = std::sync::OnceLock::new();
    std::sync::Arc::clone(S.get_or_init(|| std::sync::Arc::from("java/lang/String")))
}

/// 数组的运行时类（数组不声明方法，全部继承自 `Object`）
fn arc_jlobject() -> std::sync::Arc<str> {
    static S: std::sync::OnceLock<std::sync::Arc<str>> = std::sync::OnceLock::new();
    std::sync::Arc::clone(S.get_or_init(|| std::sync::Arc::from("java/lang/Object")))
}
