//! `rustjava` 的最小解释器：不依赖任何 JVM，直接执行 class 文件。
//!
//! 覆盖范围（够跑我们自己编译器产出的东西）：
//! int/long/double 的常量、局部变量与算术，lcmp/dcmpl/dcmpg，数值转换，
//! 比较与跳转、goto、pop/pop2、对象与 int 数组、
//! `System.out.println(int|long|double|String)`、invokestatic、各 xreturn。
//!
//! 本文件只剩入口与 `Vm` 的定义 —— 具体实现按职责分散在同目录各模块里。
use crate::{parse, ClassFile};
use std::collections::HashMap;
use std::sync::Mutex;
mod call;
mod class;
mod exception;
mod exec;
mod frame;
mod heap;
mod indy;
mod load;
mod native_reg;
mod natives;
mod rt;
mod slot;
mod thread;

pub use slot::Slot;
pub(crate) use class::builtin_super;
use load::ClassStore;

#[derive(Debug)]
pub struct VmError {
    pub msg: String,
    /// Some(句柄) 表示这是一个**被抛出的 Java 异常对象**（不是 VM 内部错误）:
    /// 本帧找不到 handler 时向上抛给调用方的 `execute` 继续找。
    pub exc: Option<i32>,
}

fn err<T>(msg: impl Into<String>) -> Result<T, VmError> {
    Err(VmError { msg: msg.into(), exc: None })
}

// ------------------------------------------------------- 系统属性（`-Dkey=value`）

/// 启动时由 `rustjava` / `run_jdk` 从 `-D` 参数填进来的一小组系统属性。
///
/// 这不是 Java 的 `System.getProperties()`（这个 VM 的 `System` 还没有那套），
/// 只为让 `-Dvmless.indy.stats` 这类开关能像真 JVM 一样写。环境变量是等价入口。
static PROPS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// 设置一个系统属性
pub fn set_property(key: &str, value: &str) {
    if let Ok(mut p) = PROPS.lock() {
        match p.iter_mut().find(|(k, _)| k == key) {
            Some(e) => e.1 = value.to_string(),
            None => p.push((key.to_string(), value.to_string())),
        }
    }
}

/// 读一个系统属性
pub fn property(key: &str) -> Option<String> {
    PROPS.lock().ok()?.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

fn prop(key: &str) -> Option<String> {
    property(key)
}

/// 把 `-Dkey=value` 形式的参数从命令行里摘出来（返回剩下的参数）。
///
/// 两个 bin 共用：`rustjava -Dvmless.indy.stats -cp … Main` 与
/// `run_jdk -Dvmless.indy.stats <目录> <主类>` 都支持。
pub fn take_property_args(args: &[String]) -> Vec<String> {
    let mut rest = Vec::with_capacity(args.len());
    for a in args {
        match a.strip_prefix("-D") {
            Some(kv) => {
                let (k, v) = match kv.split_once('=') {
                    Some((k, v)) => (k, v),
                    None => (kv, ""),
                };
                set_property(k, v);
            }
            None => rest.push(a.clone()),
        }
    }
    rest
}

/// 执行结果
pub struct ExecResult {
    pub return_value: i32,
    pub output: Vec<String>,
    /// 未捕获的 Java 异常：已经是定好格式的那一行
    /// （`Exception in thread "main" java.lang.RuntimeException: boom`）。
    pub exception: Option<String>,
}

pub struct Vm {
    /// 所有已加载的类 + 继承/字段/方法元数据
    store: ClassStore,
    output: Vec<String>,
    depth: usize,
    /// 对象堆：每个对象是一组字段槽（Slot 自带宽度，long/double 不会被截断）；
    /// 引用 = 下标 + 1（0 表示 null）
    objects: Vec<Vec<Slot>>,
    /// 对象堆的运行时类型名（instanceof / checkcast / 虚分派用）
    object_class: Vec<String>,
    /// 数组堆：每个数组是一组元素槽；句柄 = -(下标 + 1)
    arrays: Vec<Vec<Slot>>,
    /// 数组堆的类描述符，如 `[I` / `[Ljava/lang/String;`
    array_class: Vec<String>,
    /// 字符串常量堆；句柄 = STR_BASE + 下标
    strings: Vec<String>,
    /// `java/lang/StringBuilder` 的缓冲：对象句柄 → 已拼好的文本
    /// （`toString()` 之后从表里移除）
    sb_bufs: std::collections::HashMap<i32, String>,
    /// (类下标, 常量池索引) → 字符串句柄（同一常量必须拿到同一句柄，否则引用相等会错）
    string_handles: HashMap<(usize, u16), i32>,
    /// 包装类的小整数缓存：`Integer#5` → 对象句柄（Java 的 valueOf 也缓存 -128..127）
    wrapper_cache: HashMap<String, i32>,
    /// 异常对象句柄 → 构造函数里那个 message 的字符串句柄。
    /// `new RuntimeException("boom")` 的构造器在 java.* 里（我们没有它的字节码），
    /// 所以构造时只把消息记下来，抛出来时再拼。
    exc_msgs: HashMap<i32, i32>,
    /// 静态字段：(声明类, 字段名) → 值（没有条目就是默认值 0/null）
    statics: HashMap<(String, String), Slot>,
    /// 已经跑完 `<clinit>` 的类（静态初始化只跑一次，且先跑父类）
    initialized: std::collections::HashSet<String>,
    /// 正在跑 `<clinit>` 的类 → 初始化它的线程 id（JVMS 5.5：一个类同时只许一个线程初始化）
    initializing: HashMap<String, u64>,
    /// 等某个类初始化完成的线程
    init_waiters: HashMap<String, Vec<u64>>,
    /// 类对象：句柄 → 它代表的类描述符（`X.class` / `getClass()` 的轻量句柄）
    class_objs: HashMap<i32, String>,
    /// 类描述符 → 类对象句柄（同一个类必须永远同一个句柄，`X.class == X.class` 靠它）
    class_obj_cache: HashMap<String, i32>,
    /// 字符串句柄的**字段视图**：`getfield value/coder/...` 落在字符串堆句柄上时用。
    /// 真 java.base 的 `String.length()` 是 `value.length >> coder`，没有这层就一碰常量就断。
    str_fields: HashMap<i32, Vec<Slot>>,
    /// native 方法注册表：`(类, 方法名, 描述符) → Rust 实现`
    natives: native_reg::NativeRegistry,
    /// 真 JDK 的类是否真的加载进来了（`java/lang/Object` 在类表里 = 真字节码优先）
    jdk_loaded: bool,
    /// `VMLESS_PREFER_BUILTIN=1`：显式要求先走 `jdk.rs` 的内建实现
    prefer_builtin: bool,
    /// 缺 native 的累计清单（`UnsatisfiedLinkError` 时记一条，供 `docs/java-base-native-清单.md`）
    missing_natives: std::collections::BTreeSet<String>,
    /// `invokedynamic` / `java.lang.invoke` 的全部状态（句柄、调用点、lambda、VarHandle）
    mhs: HashMap<i32, indy::MhInfo>,
    mtypes: HashMap<i32, String>,
    lookups: HashMap<i32, String>,
    callsites: HashMap<i32, indy::CallSiteInfo>,
    lambdas: HashMap<i32, indy::LambdaInfo>,
    varhandles: HashMap<i32, indy::VarHandleInfo>,
    /// 引导方法表缓存（按类名：`define_class` 换掉字节码时清空）
    bsm_tables: HashMap<String, indy::BsmTable>,
    /// 调用点 → 已解析的目标句柄：(类下标, 常量池索引)
    indy_sites: HashMap<(usize, u16), i32>,
    /// `ldc Constant_Dynamic` 的常量值
    condy_values: HashMap<(usize, u16), indy::TypedSlot>,
    /// 打印调用点解析统计（`-Dvmless.indy.stats` 或 `VMLESS_INDY_STATS=1`）
    indy_stats: bool,
    /// 解析过的调用点个数（同一调用点只解析一次 —— 这个数就是证据）
    indy_resolved: usize,
    /// `invokedynamic` 执行过的次数
    indy_calls: usize,
    /// 已生成的 lambda 类序号
    lambda_seq: usize,
    /// Java 线程表：tid → 记录（parker / 状态 / 对应的 `Thread` 对象）
    threads: HashMap<u64, thread::ThreadRec>,
    /// 对象监视器表：对象句柄 → 锁状态（owner / 重入计数 / entry 与 cond 两个队列）
    monitors: HashMap<i32, thread::Monitor>,
    /// 存活的 Java 线程数（1 = 单线程，safepoint 什么都不用做）
    nthreads: u32,
    /// 距上一次 safepoint 已经跑了多少条字节码
    tick: u32,
    /// 自动起名计数器（`Thread-N`）
    thread_seq: u32,
    /// 死锁 watchdog 的停滞计数（连续几次 tick 所有线程都不在 RUNNABLE）
    stalled: u32,
}

impl Vm {
    /// 用一批已解析的类建解释器（`main_class` 是入口类名）
    pub fn new(classes: Vec<ClassFile>, main_class: &str) -> Result<Self, VmError> {
        Ok(Self::from_store(ClassStore::new(classes, Some(main_class))?))
    }

    pub(crate) fn from_store(store: ClassStore) -> Self {
        let jdk_loaded = store.has_class("java/lang/Object");
        Vm {
            store,
            output: Vec::new(),
            depth: 0,
            objects: Vec::new(),
            object_class: Vec::new(),
            arrays: Vec::new(),
            array_class: Vec::new(),
            strings: Vec::new(),
            sb_bufs: HashMap::new(),
            string_handles: HashMap::new(),
            wrapper_cache: HashMap::new(),
            exc_msgs: HashMap::new(),
            statics: HashMap::new(),
            initialized: std::collections::HashSet::new(),
            initializing: HashMap::new(),
            init_waiters: HashMap::new(),
            class_objs: HashMap::new(),
            class_obj_cache: HashMap::new(),
            str_fields: HashMap::new(),
            natives: native_reg::NativeRegistry::new(),
            jdk_loaded,
            prefer_builtin: std::env::var("VMLESS_PREFER_BUILTIN").map(|v| v == "1").unwrap_or(false),
            missing_natives: std::collections::BTreeSet::new(),
            mhs: HashMap::new(),
            mtypes: HashMap::new(),
            lookups: HashMap::new(),
            callsites: HashMap::new(),
            lambdas: HashMap::new(),
            varhandles: HashMap::new(),
            bsm_tables: HashMap::new(),
            indy_sites: HashMap::new(),
            condy_values: HashMap::new(),
            indy_stats: prop("vmless.indy.stats").map(|v| v != "0" && v != "false").unwrap_or_else(|| {
                std::env::var("VMLESS_INDY_STATS").map(|v| v == "1").unwrap_or(false)
            }),
            indy_resolved: 0,
            indy_calls: 0,
            lambda_seq: 0,
            threads: HashMap::new(),
            monitors: HashMap::new(),
            nthreads: 0,
            tick: 0,
            thread_seq: 0,
            stalled: 0,
        }
    }

    /// 真 JDK 的类是否真的进了类表（= 遇到 `java.*` 就该执行真字节码，而不是拿内建顶替）
    pub fn jdk_loaded(&self) -> bool {
        self.jdk_loaded
    }

    /// 这次运行里累计遇到的「缺 native」（`UnsatisfiedLinkError` 的清单）
    pub fn missing_natives(&self) -> Vec<String> {
        self.missing_natives.iter().cloned().collect()
    }

    /// 已注册的 native 条目（`docs/java-base-native-清单.md` 与自检用）
    pub fn native_entries(&self) -> Vec<(&'static str, &'static str, &'static str)> {
        self.natives.entries()
    }

    /// 找一个 native 实现；找不到就记进缺清单并报 `UnsatisfiedLinkError`
    pub(super) fn require_native(
        &mut self,
        class: &str,
        name: &str,
        desc: &str,
    ) -> Result<native_reg::NativeFn, VmError> {
        if let Some(f) = self.natives.lookup(class, name, desc) {
            return Ok(f);
        }
        let key = format!("{class}.{name}{desc}");
        self.missing_natives.insert(key.clone());
        Err(VmError {
            msg: format!("UnsatisfiedLinkError: {key} not registered"),
            exc: None,
        })
    }

    /// 跑 main([Ljava/lang/String;)V
    pub fn run_main(&mut self) -> Result<ExecResult, VmError> {
        let mi = self
            .store
            .main_method()
            .ok_or_else(|| VmError { msg: "找不到 main([Ljava/lang/String;)V".into(), exc: None })?;
        let main_class = self.store.main;
        // JVM 在调 main 之前会先初始化入口类
        let main_name = self.store.classes[main_class].this_class_name().unwrap_or_default();
        self.ensure_init(&main_name)?;
        // 主线程进 VM：登记 tid=1 + 拿到 GIL（此后 main 的一切都在锁里跑）
        thread::set_vm_ptr(self as *mut Vm);
        thread::set_main_tid();
        thread::gil_lock();
        self.thread_register(1, 0, "main", false);
        let r = self.execute(main_class, mi, "([Ljava/lang/String;)V", &[Slot::I(0)], false);
        // 主线程返回 ≠ 进程退出：非 daemon 线程要继续跑完（`System.exit` 已经在 native 里直接退）
        self.wait_non_daemon();
        self.thread_terminate_main();
        // `-Dvmless.indy.stats`：调用点解析次数（「同一调用点只解析一次」的证据）
        if self.indy_stats {
            for line in self.indy_stats_lines() {
                eprintln!("{line}");
            }
        }
        thread::clear_vm_ptr();
        thread::gil_unlock();
        match r {
            Ok(r) => Ok(ExecResult {
                return_value: r.as_i32(),
                output: std::mem::take(&mut self.output),
                exception: None,
            }),
            // 未捕获的 Java 异常：输出照常给出，另附一行 "Exception in thread ..."
            Err(e) if e.exc.is_some() => {
                let text = self.exc_text(e.exc.unwrap());
                Ok(ExecResult {
                    return_value: 1,
                    output: std::mem::take(&mut self.output),
                    exception: Some(format!("Exception in thread \"main\" {text}")),
                })
            }
            Err(e) => Err(e),
        }
    }

    /// 运行期定义一个类（`ClassLoader.defineClass` 的最小入口）。
    ///
    /// 边界：**不做双亲委派**（这个 VM 里还没有 `java.lang.ClassLoader` 类，也就没有
    /// 父子关系可言），也不校验字节码；`name` 给了就必须和 class 文件自己声明的类名
    /// 一致（空串表示照文件说的算）。同名类会被替换，随后重建方法表与字段布局。
    pub fn define_class(&mut self, name: &str, bytes: &[u8]) -> Result<String, VmError> {
        let cf = parse(bytes).map_err(|e| VmError { msg: format!("class 解析失败: {e}"), exc: None })?;
        let actual = cf
            .this_class_name()
            .ok_or_else(|| VmError { msg: "class 没有 this_class".into(), exc: None })?;
        if !name.is_empty() && internal_name(name) != actual {
            return err(format!(
                "defineClass 的名字 {name:?} 与 class 文件声明的 {actual} 不一致"
            ));
        }
        self.store.add_class(cf);
        // 类表被换过：调用点与常量缓存全部失效（常量池下标的含义可能已经变了）
        self.indy_sites.clear();
        self.condy_values.clear();
        self.bsm_tables.clear();
        Ok(actual)
    }

    /// 运行期已加载的类名（诊断用）
    pub fn loaded_classes(&self) -> Vec<String> {
        self.store.class_names()
    }

    /// 按 classpath 建 VM（**公开入口**，`examples/run_jdk.rs` 与 `rustjava -cp` 共用）。
    ///
    /// `VMLESS_JDK` 设了就把它挂到 classpath **末尾**（用户自己的类优先）：
    /// 从此 `java.*` 不再是 `natives` 里的替身，而是 **OpenJDK 的真字节码**。
    pub fn open(spec: &str, main_class: &str) -> Result<Self, VmError> {
        let mut cp = crate::load::ClassPath::from_spec(spec).map_err(|e| VmError { msg: e, exc: None })?;
        push_jdk(&mut cp);
        let name = internal_name(main_class);
        Ok(Self::from_store(ClassStore::load_path(&cp, &name)?))
    }
}

/// `VMLESS_JDK`：真 JDK 的 `lib/modules`（jimage）**或**解包出来的模块目录树。
///
/// 不设它 = 独立模式（只有用户自己的类，`java.*` 由 `natives` 的替身实现）。
/// 设了它 = **真字节码模式**：`java.*` 全部真执行，缺 native 就报 `UnsatisfiedLinkError`，
/// 绝不静默回退（要回退得显式 `VMLESS_PREFER_BUILTIN=1`）。
fn push_jdk(cp: &mut crate::load::ClassPath) {
    let Ok(spec) = std::env::var("VMLESS_JDK") else { return };
    let spec = spec.trim().to_string();
    if spec.is_empty() {
        return;
    }
    let p = std::path::Path::new(&spec);
    let r = if p.is_dir() { cp.push_module_tree(p) } else { cp.push(p) };
    if let Err(e) = r {
        eprintln!("VMLESS_JDK 挂载失败（{spec}）: {e}");
    }
}

/// 从字节加载并执行 main（只有这一个类，父类找不到就是运行时错误）
pub fn run_class_bytes(bytes: &[u8]) -> Result<ExecResult, VmError> {
    let cf = parse(bytes).map_err(|e| VmError { msg: format!("class 解析失败: {e}"), exc: None })?;
    let name = cf
        .this_class_name()
        .ok_or_else(|| VmError { msg: "class 没有 this_class".into(), exc: None })?;
    Vm::new(vec![cf], &name)?.run_main()
}

/// 从文件加载并执行 main：从「类文件所在目录的包根」当 classpath，按需把
/// 继承链/签名里引用到的类找回来（父类、兄弟编译单元里的类）
pub fn run_class_file(path: &str) -> Result<ExecResult, VmError> {
    let bytes = std::fs::read(path).map_err(|e| VmError { msg: format!("读取 {path} 失败: {e}"), exc: None })?;
    let cf = parse(&bytes).map_err(|e| VmError { msg: format!("class 解析失败: {e}"), exc: None })?;
    let name = cf
        .this_class_name()
        .ok_or_else(|| VmError { msg: "class 没有 this_class".into(), exc: None })?;
    let dir = std::path::Path::new(path)
        .parent()
        // `I1.class`（没有目录部分）的 `parent()` 是空串 —— 那就是当前目录，
        // 不能当成「classpath 为空」报错
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let root = package_root(&dir, &name);
    let mut cp = crate::load::ClassPath::from_spec(&root.to_string_lossy())
        .map_err(|e| VmError { msg: e, exc: None })?;
    push_jdk(&mut cp);
    let mut vm = Vm::from_store(ClassStore::load_path(&cp, &name)?);
    vm.run_main()
}

/// `a.b.C` / `a/b/C` / `a/b/C.class` → `a/b/C`
fn internal_name(n: &str) -> String {
    let n = n.trim();
    let n = n.strip_suffix(".class").unwrap_or(n);
    if n.contains('/') {
        n.to_string()
    } else {
        n.replace('.', "/")
    }
}

/// 按 classpath（`a.jar:b.jar:dir`）加载并执行主类
pub fn run_classpath(spec: &str, main_class: &str) -> Result<ExecResult, VmError> {
    Vm::open(spec, main_class)?.run_main()
}

/// 从 jar 执行 main；`main` 为 None 时读 `META-INF/MANIFEST.MF` 的 `Main-Class`
pub fn run_jar(path: &str, main: Option<&str>) -> Result<ExecResult, VmError> {
    let name = match main {
        Some(m) => internal_name(m),
        None => {
            let jar = crate::load::jar::JarSource::open(path).map_err(|e| VmError { msg: e, exc: None })?;
            let m = jar.main_class().map_err(|e| VmError { msg: e, exc: None })?;
            let m = m.ok_or_else(|| VmError {
                msg: format!("{path} 的 MANIFEST.MF 里没有 Main-Class（可用 -cp <主类名> 指定）"),
                exc: None,
            })?;
            internal_name(&m)
        }
    };
    let mut cp = crate::load::ClassPath::from_spec(path).map_err(|e| VmError { msg: e, exc: None })?;
    push_jdk(&mut cp);
    let store = ClassStore::load_path(&cp, &name)?;
    Vm::from_store(store).run_main()
}

/// 类文件所在目录 → **包根**：`…/out/app/X.class` 的包根是 `…/out`。
///
/// 目录末尾的若干级必须正好等于包的各级名字，否则原样返回（非标准布局时保底）。
/// 带包名的类分布在包根的子目录里，从包根加载才能同时看到 `lib/` 和 `app/`。
pub fn package_root(dir: &std::path::Path, class_name: &str) -> std::path::PathBuf {
    let Some((pkg, _)) = class_name.rsplit_once('/') else {
        return dir.to_path_buf();
    };
    let mut root = dir.to_path_buf();
    for seg in pkg.split('/').rev() {
        let matched = root.file_name().and_then(|n| n.to_str()) == Some(seg);
        if !matched || !root.pop() {
            return dir.to_path_buf();
        }
    }
    root
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::call::count_args;
    use crate::compiler::compile;
    use crate::parser::parse_compilation_unit;

    fn compile_and_run(src: &str) -> Vec<String> {
        let cu = parse_compilation_unit(src).expect("parse");
        let classes = compile(&cu).expect("compile");
        let (_name, bytes) = &classes[0];
        run_class_bytes(bytes).expect("run").output
    }

    #[test]
    fn runs_arithmetic() {
        let out = compile_and_run(
            r#"
            class A {
                public static void main(String[] args) {
                    System.out.println(1 + 2 * 3);
                }
            }
        "#,
        );
        assert_eq!(out, vec!["7"]);
    }

    #[test]
    fn runs_while_loop() {
        let out = compile_and_run(
            r#"
            class B {
                static int sum(int n) {
                    int i = 0; int s = 0;
                    while (i < n) { s = s + i; i = i + 1; }
                    return s;
                }
                public static void main(String[] args) { System.out.println(sum(5)); }
            }
        "#,
        );
        assert_eq!(out, vec!["10"]);
    }

    #[test]
    fn runs_if_else() {
        let out = compile_and_run(
            r#"
            class C {
                static int max(int a, int b) { if (a > b) { return a; } return b; }
                public static void main(String[] args) {
                    System.out.println(max(7, 3));
                    System.out.println(max(2, 9));
                }
            }
        "#,
        );
        assert_eq!(out, vec!["7", "9"]);
    }

    #[test]
    fn runs_objects_and_fields() {
        let out = compile_and_run(
            r#"
            class Point {
                int x;
                Point(int v) { this.x = v; }
                static int twice(Point p) { return p.x + p.x; }
                public static void main(String[] args) {
                    Point p = new Point(21);
                    System.out.println(twice(p));
                }
            }
        "#,
        );
        assert_eq!(out, vec!["42"]);
    }

    #[test]
    fn runs_long_and_double() {
        let out = compile_and_run(
            r#"
            class LD {
                static long sum(long n) {
                    long s = 0L; long i = 0L;
                    while (i < n) { s = s + i; i = i + 1L; }
                    return s;
                }
                static double avg(double a, double b) { return (a + b) / 2.0; }
                public static void main(String[] args) {
                    System.out.println(sum(100000L));
                    System.out.println(avg(3.5, 0.1 + 0.2));
                    System.out.println(1.0e7);
                    System.out.println(-0.0);
                }
            }
        "#,
        );
        assert_eq!(out, vec!["4999950000", "1.9", "1.0E7", "-0.0"]);
    }

    #[test]
    fn boxes_and_unboxes_implicitly() {
        // 自动装箱（`Integer i = 5`）、拆箱（`int n = i`）、以及包装类参与算术
        let out = compile_and_run(
            r#"
            class B1 {
                static int twice(Integer n) { return n * 2; }
                public static void main(String[] args) {
                    Integer i = 5;
                    int n = i;
                    System.out.println(n + 1);
                    System.out.println(i + 3);
                    System.out.println(twice(21));
                    Object o = 7;
                    System.out.println(o);
                    int back = (Integer) o;
                    System.out.println(back * 2);
                }
            }
        "#,
        );
        assert_eq!(out, vec!["6", "8", "42", "7", "14"]);
    }

    #[test]
    fn builtin_string_methods() {
        let out = compile_and_run(
            r#"
            class S1 {
                public static void main(String[] args) {
                    String s = "Hello";
                    System.out.println(s.length());
                    System.out.println(s.substring(1, 3));
                    System.out.println(s.toUpperCase());
                    System.out.println(s.indexOf("l"));
                    System.out.println("a,b".split(",")[1]);
                    System.out.println(s.charAt(1));
                }
            }
        "#,
        );
        assert_eq!(out, vec!["5", "el", "HELLO", "2", "b", "e"]);
    }

    #[test]
    fn object_protocol_defaults() {
        // 没覆写时：hashCode 按对象标识（同对象稳定、不同对象不同），toString 是 `Klass@hash`
        let out = compile_and_run(
            r#"
            class O1 {
                int x;
                O1(int x) { this.x = x; }
                public static void main(String[] args) {
                    O1 a = new O1(1);
                    O1 b = new O1(2);
                    System.out.println(a.hashCode() == a.hashCode());
                    System.out.println(a.hashCode() != b.hashCode());
                    System.out.println(a.equals(a));
                    System.out.println(a.toString().startsWith("O1@"));
                }
            }
        "#,
        );
        assert_eq!(out, vec!["true", "true", "true", "true"]);
    }

    #[test]
    fn not_equal_operator_is_not_swallowed() {
        // `!=` 曾经被词法器当成了 `==`（两个都是一样的输出时才不会被发现）
        let out = compile_and_run(
            r#"
            class N1 {
                public static void main(String[] args) {
                    int x = 1;
                    int y = 2;
                    System.out.println(x != y);
                    System.out.println(x == y);
                }
            }
        "#,
        );
        assert_eq!(out, vec!["true", "false"]);
    }

    /// 运行期定义类（`defineClass`）：先只给 A，再把 B 塞进去，然后 B 的方法能用
    #[test]
    fn define_class_at_runtime() {
        let src = r#"
            class Base7 { int v() { return 7; } }
            class Extra7 { static int f() { return 3; } }
        "#;
        let cu = parse_compilation_unit(src).expect("parse");
        let classes = compile(&cu).expect("compile");
        let by_name = |n: &str| {
            classes
                .iter()
                .find(|(name, _)| name == n)
                .map(|(_, b)| b.clone())
                .expect("编译产物里没有这个类")
        };
        let base = parse(&by_name("Base7")).expect("parse Base7");
        let mut vm = Vm::new(vec![base], "Base7").expect("vm");
        assert_eq!(vm.loaded_classes(), vec!["Base7".to_string()]);
        // 名字对不上必须报错，不能悄悄塞进去
        assert!(vm.define_class("Wrong", &by_name("Extra7")).is_err());
        let got = vm.define_class("Extra7", &by_name("Extra7")).expect("defineClass");
        assert_eq!(got, "Extra7");
        assert_eq!(vm.loaded_classes(), vec!["Base7".to_string(), "Extra7".to_string()]);
        // 方法表跟着重建了：能按名字解析到刚定义的静态方法
        assert!(vm.store.method_in_class("Extra7", "f", "()I").is_some());
    }

    #[test]
    fn counts_descriptor_args() {
        assert_eq!(count_args("(II)I").unwrap(), 2);
        assert_eq!(count_args("([Ljava/lang/String;)V").unwrap(), 1);
        assert_eq!(count_args("(ILjava/lang/String;)V").unwrap(), 2);
        assert_eq!(count_args("(JD)D").unwrap(), 2);
    }
}
