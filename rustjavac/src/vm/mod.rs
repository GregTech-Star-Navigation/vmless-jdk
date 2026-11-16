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
mod call;
mod class;
mod exception;
mod exec;
mod frame;
mod heap;
mod load;
mod natives;
mod rt;
mod slot;

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
    /// 已经跑过 `<clinit>` 的类（静态初始化只跑一次，且先跑父类）
    initialized: std::collections::HashSet<String>,
}

impl Vm {
    /// 用一批已解析的类建解释器（`main_class` 是入口类名）
    pub fn new(classes: Vec<ClassFile>, main_class: &str) -> Result<Self, VmError> {
        Ok(Self::from_store(ClassStore::new(classes, Some(main_class))?))
    }

    pub(crate) fn from_store(store: ClassStore) -> Self {
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
        }
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
        match self.execute(main_class, mi, "([Ljava/lang/String;)V", &[Slot::I(0)], false) {
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
        Ok(actual)
    }

    /// 运行期已加载的类名（诊断用）
    pub fn loaded_classes(&self) -> Vec<String> {
        self.store.class_names()
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
    let dir = std::path::Path::new(path).parent().map(|p| p.to_path_buf()).unwrap_or_else(|| ".".into());
    let root = package_root(&dir, &name);
    let cp = crate::load::ClassPath::from_spec(&root.to_string_lossy())
        .map_err(|e| VmError { msg: e, exc: None })?;
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
    let cp = crate::load::ClassPath::from_spec(spec).map_err(|e| VmError { msg: e, exc: None })?;
    let name = internal_name(main_class);
    let store = ClassStore::load_path(&cp, &name)?;
    Vm::from_store(store).run_main()
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
    let cp = crate::load::ClassPath::from_spec(path).map_err(|e| VmError { msg: e, exc: None })?;
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
