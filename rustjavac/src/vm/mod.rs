//! `rustjava` 的最小解释器：不依赖任何 JVM，直接执行 class 文件。
//!
//! 覆盖范围（够跑我们自己编译器产出的东西）：
//! int/long/double 的常量、局部变量与算术，lcmp/dcmpl/dcmpg，数值转换，
//! 比较与跳转、goto、pop/pop2、对象与 int 数组、
//! `System.out.println(int|long|double|String)`、invokestatic、各 xreturn。
//!
//! ## 线程模型（2026-11 去 GIL）
//!
//! `Vm` 不再是一个"整个 VM"的结构体，而是**一个 Java 线程的解释器上下文**：
//!
//! ```text
//! Arc<Shared>  堆 / 监视器 / 类表快照 / 静态字段 / indy 状态（见 shared.rs）
//! Vm           调用深度、safepoint 计数、当前调用者、本线程持有的监视器（无锁）
//! Frame        局部变量与操作数栈（在 Rust 栈上，天然线程私有）
//! ```
//!
//! 每个 OS 线程一个 `Vm`（`Vm::fork`），彼此只通过 `Shared` 通信，而 `Shared` 里
//! 每一条路径都有明确的同步（原子 / 锁 / 快照）。所以「多线程并行执行字节码」是
//! 真的并行：没有全局锁、没有 safepoint 交还、没有票据排队。
//!
//! 本文件只剩入口与 `Vm` 的定义 —— 具体实现按职责分散在同目录各模块里。
use self::shared::lk;
use crate::{parse, ClassFile};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

mod bridge;
mod call;
mod class;
mod exception;
mod exec;
mod frame;
mod gc;
mod heap;
mod indy;
mod load;
pub mod jpms;
mod monitors;
mod native_reg;
mod natives;
mod reflect;
mod rt;
mod shared;
mod slot;
mod thread;

pub use bridge::{
    call_interpreted_ptr, has_ref, install_jit, interp_ready, interp_targets, jit_table_len, open_interp,
    register_interp,
    JitEntry, JitTable,
};
pub use slot::Slot;
pub(crate) use class::builtin_super;
use load::ClassStore;
use shared::Shared;

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

pub(crate) fn prop(key: &str) -> Option<String> {
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

/// 一个 Java 线程的解释器上下文。
///
/// 字段只有两种：**线程私有的执行状态**（深度 / 计数 / 当前调用者 / 持有的锁），
/// 和两个指向共享世界的 `Arc`。既没有 `&mut` 别名，也没有全局锁 ——
/// 这就是"字节码真并行"的全部含义。
pub struct Vm {
    /// 共享世界（堆、监视器、类表快照、静态字段…）
    pub(crate) sh: Arc<Shared>,
    /// 类表**快照**：加类时 COW 换一份（读路径不拿锁，见 `sync_store`）
    pub(crate) store: Arc<ClassStore>,
    /// 本地快照对应的类表版本
    store_ver: u64,
    /// 调用深度（单线程上限，防递归爆栈）
    pub(crate) depth: usize,
    /// 距上一次 safepoint 查问跑了多少条字节码
    pub(crate) tick: u32,
    /// 最近一次字节码调用点的类下标（反射的访问检查要「调用者是谁」；`usize::MAX` = 无）
    pub(crate) last_caller: usize,
    /// 本线程当前持有的监视器句柄（`count` 从 0 → 1 时入栈）。
    /// 线程异常终止时靠它把锁全放掉；空则说明没持有任何锁。
    pub(crate) held: Vec<i32>,
}

impl Vm {
    /// 用一批已解析的类建解释器（`main_class` 是入口类名）
    pub fn new(classes: Vec<ClassFile>, main_class: &str) -> Result<Self, VmError> {
        Ok(Self::from_store(ClassStore::new(classes, Some(main_class))?))
    }

    pub(crate) fn from_store(store: ClassStore) -> Self {
        let store = Arc::new(store);
        let sh = Arc::new(Shared::new(Arc::clone(&store)));
        thread::register_shared(&sh);
        Vm {
            sh,
            store,
            store_ver: 0,
            depth: 0,
            tick: 0,
            last_caller: usize::MAX,
            held: Vec::new(),
        }
    }

    /// 从一个已有的共享世界造解释器上下文（死锁看门狗用：不跑字节码，只读状态）。
    pub(crate) fn from_shared(sh: Arc<Shared>) -> Vm {
        let store = sh.store_snapshot();
        Vm { sh, store, store_ver: 0, depth: 0, tick: 0, last_caller: usize::MAX, held: Vec::new() }
    }

    /// 造一个**本线程专用**的解释器上下文：共享同一个 `Shared`，执行状态全新。
    ///
    /// `Thread.start` 与死锁看门狗都走它 —— 这是"每个线程有自己的 `Vm`"的唯一入口。
    pub(crate) fn fork(&self) -> Vm {
        Vm {
            sh: Arc::clone(&self.sh),
            store: Arc::clone(&self.store),
            store_ver: self.store_ver,
            depth: 0,
            tick: 0,
            last_caller: usize::MAX,
            held: Vec::new(),
        }
    }

    // ---------------------------------------------------------- 类表快照

    /// 别的线程刚加载过类 → 换一份新快照（版本号没变就什么都不做）
    pub(crate) fn sync_store(&mut self) {
        let v = self.sh.store_ver.load(Ordering::Acquire);
        if v != self.store_ver {
            self.store = self.sh.store_snapshot();
            self.store_ver = v;
        }
    }

    /// 可变访问类表（COW）。改完必须 [`Vm::publish_store`]。
    pub(crate) fn store_mut(&mut self) -> &mut ClassStore {
        Arc::make_mut(&mut self.store)
    }

    /// 把本线程改过的类表发布出去（COW 换快照 + 推进版本号）
    pub(crate) fn publish_store(&mut self) {
        self.store_ver = self.sh.store_publish(Arc::clone(&self.store));
    }

    // ---------------------------------------------------------- 输出

    /// 这一轮 VM 已经打印的行（不取走，含别的线程打印的）
    pub(crate) fn output_snapshot(&self) -> Vec<String> {
        lk(&self.sh.output).clone()
    }

    /// 刷出并清空已打印的行（VM 内部错误时用：先让用户看到前半段输出）
    pub(crate) fn output_drain(&self) -> Vec<String> {
        std::mem::take(&mut *lk(&self.sh.output))
    }

    pub(crate) fn output_take(&self) -> Vec<String> {
        std::mem::take(&mut *lk(&self.sh.output))
    }

    pub(crate) fn println(&self, line: String) {
        lk(&self.sh.output).push(line);
    }

    // ---------------------------------------------------------- 状态查询

    /// 真 JDK 的类是否真的进了类表（= 遇到 `java.*` 就该执行真字节码，而不是拿内建顶替）
    pub fn jdk_loaded(&self) -> bool {
        self.sh.jdk_loaded
    }

    /// 这次运行里累计遇到的「缺 native」（`UnsatisfiedLinkError` 的清单）
    pub fn missing_natives(&self) -> Vec<String> {
        self.sh
            .missing_natives
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// 已注册的 native 条目（`docs/java-base-native-清单.md` 与自检用）
    pub fn native_entries(&self) -> Vec<(&'static str, &'static str, &'static str)> {
        self.sh.natives.entries()
    }

    /// 找一个 native 实现；找不到就记进缺清单并报 `UnsatisfiedLinkError`
    pub(super) fn require_native(
        &mut self,
        class: &str,
        name: &str,
        desc: &str,
    ) -> Result<native_reg::NativeFn, VmError> {
        if let Some(f) = self.sh.natives.lookup(class, name, desc) {
            return Ok(f);
        }
        let key = format!("{class}.{name}{desc}");
        self.sh
            .missing_natives
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone());
        Err(VmError {
            msg: format!("UnsatisfiedLinkError: {key} not registered"),
            exc: None,
        })
    }

    /// 跑 main([Ljava/lang/String;)V
    pub fn run_main(&mut self) -> Result<ExecResult, VmError> {
        let mi = self.store
            .main_method()
            .ok_or_else(|| VmError { msg: "找不到 main([Ljava/lang/String;)V".into(), exc: None })?;
        let main_class = self.store.main;
        // JVM 在调 main 之前会先初始化入口类
        let main_name = self.store.classes[main_class].this_class_name().unwrap_or_default();
        self.ensure_init(&main_name)?;
        thread::set_main_tid();
        self.sh.safepoint.register();
        self.thread_register(1, 0, "main", false);
        // 真字节码模式下，JVM 在跑 main 之前会先执行 `System.initPhase1()`：
        // 它填 `System.props`（`java.home`/`java.class.path` 等）、装 `SharedSecrets`
        // （`JavaLangAccess` / `JavaLangReflectAccess`）、设置 `System.out/err/in`。
        // 不跑这一步，`System.getProperties()` 恒为 null，`System.loadLibrary` 之类
        // 会直接 `InternalError: null property: java.home`。
        //
        // 独立模式（没有 java.base 字节码）里没有这个方法，`resolve_method` 返回 None，跳过即可。
        // `SharedSecrets.getJavaLangReflectAccess()` 的实装是
        // `jdk/internal/reflect/ReflectionFactory` 的 `<clinit>` 里挂上去的。
        // 不先把它跑起来，`initPhase1` 里的 `JavaLangReflectAccess.getRoot` 会以**空接收者**
        // 被调用（"找不到方法 getRoot … 接收者 null"），于是 `System.out/err/in` 一个都装不上 ——
        // 之后任何 `System.err.printf` 直接 NPE。这一步必须在 initPhase1 之前。
        let _ = self.ensure_init("jdk/internal/reflect/ReflectionFactory");
        if self.store.resolve_method("java/lang/System", "initPhase1", "()V").is_some() {
            self.ensure_init("java/lang/System")?;
            if let Some((ci, mi)) = self.store.resolve_method("java/lang/System", "initPhase1", "()V") {
                // 失败不致命：有些 JDK 版本里它有前置条件。后面真出错会以更具体的错误暴露。
                let _ = self.execute(ci, mi, "()V", &[], false);
            }
        }
        let r = self.execute(main_class, mi, "([Ljava/lang/String;)V", &[Slot::I(0)], false);
        // 主线程返回 ≠ 进程退出：非 daemon 线程要继续跑完（`System.exit` 已经在 native 里直接退）
        self.wait_non_daemon();
        self.thread_terminate_main();
        // `-Dvmless.indy.stats`：调用点解析次数（「同一调用点只解析一次」的证据）
        if self.sh.indy_stats {
            for line in self.indy_stats_lines() {
                eprintln!("{line}");
            }
        }
        self.sh.safepoint.unregister();
        match r {
            Ok(r) => Ok(ExecResult {
                return_value: r.as_i32(),
                output: self.output_take(),
                exception: None,
            }),
            // 未捕获的 Java 异常：输出照常给出，另附一行 "Exception in thread ..."
            Err(e) if e.exc.is_some() => {
                let text = self.exc_text(e.exc.unwrap());
                Ok(ExecResult {
                    return_value: 1,
                    output: self.output_take(),
                    exception: Some(format!("Exception in thread \"main\" {text}")),
                })
            }
            // VM 内部错误（不是 Java 异常）：已经打印的行**先刷出去**再报错 ——
            // 否则「前面跑得好好的、后面炸了」的诊断信息全被缓冲区吞掉，排查时活受罪
            Err(e) => {
                for line in self.output_drain() {
                    println!("{line}");
                }
                Err(e)
            }
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
        self.store_mut().add_class(cf);
        self.publish_store();
        // 类表被换过：调用点与常量缓存全部失效（常量池下标的含义可能已经变了）
        let mut indy = self.sh.indy.lock().unwrap_or_else(|e| e.into_inner());
        indy.indy_sites.clear();
        indy.condy_values.clear();
        indy.bsm_tables.clear();
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
        let mut vm = Self::from_store(ClassStore::load_path(&cp, &name)?);
        // classpath 留着：`Class.forName` 要在运行期按需加载闭包（见 `reflect.rs`）
        *vm.sh.cp.lock().unwrap_or_else(|e| e.into_inner()) = Some(cp);
        Ok(vm)
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
