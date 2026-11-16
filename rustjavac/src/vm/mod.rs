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
    /// (类下标, 常量池索引) → 字符串句柄（同一常量必须拿到同一句柄，否则引用相等会错）
    string_handles: HashMap<(usize, u16), i32>,
    /// 异常对象句柄 → 构造函数里那个 message 的字符串句柄。
    /// `new RuntimeException("boom")` 的构造器在 java.* 里（我们没有它的字节码），
    /// 所以构造时只把消息记下来，抛出来时再拼。
    exc_msgs: HashMap<i32, i32>,
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
            string_handles: HashMap::new(),
            exc_msgs: HashMap::new(),
        }
    }

    /// 跑 main([Ljava/lang/String;)V
    pub fn run_main(&mut self) -> Result<ExecResult, VmError> {
        let mi = self
            .store
            .main_method()
            .ok_or_else(|| VmError { msg: "找不到 main([Ljava/lang/String;)V".into(), exc: None })?;
        let main_class = self.store.main;
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
}

/// 从字节加载并执行 main（只有这一个类，父类找不到就是运行时错误）
pub fn run_class_bytes(bytes: &[u8]) -> Result<ExecResult, VmError> {
    let cf = parse(bytes).map_err(|e| VmError { msg: format!("class 解析失败: {e}"), exc: None })?;
    let name = cf
        .this_class_name()
        .ok_or_else(|| VmError { msg: "class 没有 this_class".into(), exc: None })?;
    Vm::new(vec![cf], &name)?.run_main()
}

/// 从文件加载并执行 main：所在目录里的 .class 一起读进来（继承链上的父类）
pub fn run_class_file(path: &str) -> Result<ExecResult, VmError> {
    let bytes = std::fs::read(path).map_err(|e| VmError { msg: format!("读取 {path} 失败: {e}"), exc: None })?;
    let cf = parse(&bytes).map_err(|e| VmError { msg: format!("class 解析失败: {e}"), exc: None })?;
    let name = cf
        .this_class_name()
        .ok_or_else(|| VmError { msg: "class 没有 this_class".into(), exc: None })?;
    let dir = std::path::Path::new(path).parent().map(|p| p.to_path_buf()).unwrap_or_else(|| ".".into());
    let store = ClassStore::load_dir(&dir, &name)?;
    let mut vm = Vm::from_store(store);
    vm.run_main()
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
    fn counts_descriptor_args() {
        assert_eq!(count_args("()V").unwrap(), 0);
        assert_eq!(count_args("(II)I").unwrap(), 2);
        assert_eq!(count_args("([Ljava/lang/String;)V").unwrap(), 1);
        assert_eq!(count_args("(ILjava/lang/String;)V").unwrap(), 2);
        assert_eq!(count_args("(JD)D").unwrap(), 2);
    }
}
