//! 类元数据、继承链与类型判定。

use super::shared::lk;
use crate::{ClassFile, CpEntry};

use super::err;
use super::heap::RefKind;
use super::Vm;
use super::VmError;

/// java.* 异常类的继承链（我们没有它们的字节码，这些层级只能写死）
pub(crate) fn builtin_super(class: &str) -> Option<&'static str> {
    match class {
        // 包装类：`Integer extends Number`（instanceof Number / Object 要靠它）
        "java/lang/Integer" | "java/lang/Long" | "java/lang/Double" => Some("java/lang/Number"),
        "java/lang/Number" => Some("java/lang/Object"),
        "java/lang/Boolean" | "java/lang/Character" => Some("java/lang/Object"),
        "java/lang/RuntimeException" => Some("java/lang/Exception"),
        "java/lang/Exception" => Some("java/lang/Throwable"),
        "java/lang/Error" => Some("java/lang/Throwable"),
        "java/lang/Throwable" => Some("java/lang/Object"),
        "java/lang/ArithmeticException"
        | "java/lang/ArrayStoreException"
        | "java/lang/ClassCastException"
        | "java/lang/IllegalArgumentException"
        | "java/lang/IllegalStateException"
        | "java/lang/IndexOutOfBoundsException"
        | "java/lang/NegativeArraySizeException"
        | "java/lang/NullPointerException"
        | "java/lang/NumberFormatException"
        | "java/lang/UnsupportedOperationException"
        | "java/lang/SecurityException"
        | "java/lang/ArrayIndexOutOfBoundsException" => Some("java/lang/RuntimeException"),
        "java/lang/StringIndexOutOfBoundsException" => Some("java/lang/IndexOutOfBoundsException"),
        "java/lang/StackOverflowError" | "java/lang/OutOfMemoryError" | "java/lang/AssertionError" => {
            Some("java/lang/Error")
        }
        // ---- java.lang.invoke ----
        // 这一支要么由 VM 接管（句柄/调用点/VarHandle 的语义都在 Rust 里），要么在
        // 独立模式（没挂 java.base）下**根本没有字节码** —— 继承链只能写死，
        // 否则 `instanceof CallSite` / `catch (BootstrapMethodError)` 都会判错。
        "java/lang/invoke/MethodHandle"
        | "java/lang/invoke/MethodType"
        | "java/lang/invoke/MethodHandles"
        | "java/lang/invoke/MethodHandles$Lookup"
        | "java/lang/invoke/VarHandle"
        | "java/lang/invoke/CallSite"
        | "java/lang/invoke/StringConcatFactory"
        | "java/lang/invoke/LambdaMetafactory"
        | "java/lang/invoke/ConstantBootstraps"
        | "java/lang/invoke/MethodHandleInfo" => Some("java/lang/Object"),
        "java/lang/invoke/ConstantCallSite"
        | "java/lang/invoke/MutableCallSite"
        | "java/lang/invoke/VolatileCallSite" => Some("java/lang/invoke/CallSite"),
        "java/lang/invoke/BootstrapMethodError" => Some("java/lang/LinkageError"),
        "java/lang/LinkageError" => Some("java/lang/Error"),
        "java/lang/invoke/WrongMethodTypeException" | "java/lang/invoke/VarHandleAccessException" => {
            Some("java/lang/RuntimeException")
        }
        "java/lang/NoSuchMethodException"
        | "java/lang/NoSuchFieldException"
        | "java/lang/IllegalAccessException"
        | "java/lang/reflect/InvocationTargetException" => Some("java/lang/ReflectiveOperationException"),
        "java/lang/ReflectiveOperationException" => Some("java/lang/Exception"),
        "java/lang/UnsupportedOperationException" => Some("java/lang/RuntimeException"),
        _ => None,
    }
}

/// 常量池里某个 Class 项的类名（instanceof / checkcast / anewarray / 异常表用）
pub(super) fn class_name_of(cf: &ClassFile, index: u16) -> Result<String, VmError> {
    match cf.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
        Some(CpEntry::Class(n)) => {
            Ok(String::from_utf8_lossy(cf.utf8(*n).unwrap_or_default()).into_owned())
        }
        other => err(format!("索引 {index} 不是 Class 项: {other:?}")),
    }
}

impl Vm {
    /// 静态初始化：先父类、再自己，只跑一次（JVM 的 `<clinit>` 语义）
    pub(super) fn ensure_init(&mut self, class: &str) -> Result<(), VmError> {
        // 快速路径：已初始化直接返回（与原来「循环第一圈就命中」等价，不多付一次检查）
        if lk(&self.sh.initialized).contains(class) {
            return Ok(());
        }
        // 初始化蕴含**加载**（JVMS 5.5 第 1 步）。类还不在类表里就先按 classpath 拉进来 ——
        // 少了这一步，下面 `method_in_class("<clinit>")` 拿到 None，整个初始化**静默成功**。
        //
        // 这不是理论问题：类名只写在 Rust 侧的 native（`new_obj("java/awt/image/DirectColorModel")`）
        // 时，常量池闭包看不见它，于是对象造不出来 → 返回 0 → Java 侧拿到 null。
        if !self.store.has_class(class) {
            let _ = self.ensure_class_loaded(class);
        }
        let me = super::thread::cur_tid();
        // JVMS 5.5：同一个类在同一时刻只能由**一个**线程初始化，别的线程要**阻塞等**。
        // 不做这件事的后果很具体：`<clinit>` 跑到一半（在 safepoint 上让出 GIL）时，
        // 另一个线程 getstatic 会读到默认值 0 —— `TimeUnit.MILLISECONDS` 变成 null、
        // `Thread.sleep(ms)` 的 `toNanos` 在 null 上取字段，最后是操作数栈下溢。
        loop {
            if lk(&self.sh.initialized).contains(class) {
                return Ok(());
            }
            let init_owner = lk(&self.sh.initializing).get(class).copied();
            match init_owner {
                // 别的线程正在初始化 → 挂进等待队列，被唤醒后重新判定
                Some(owner) if owner != me => {
                    lk(&self.sh.init_waiters).entry(class.to_string()).or_default().push(me);
                    self.set_state(me, super::thread::ST_BLOCKED);
                    self.park_block(None);
                    self.set_state(me, super::thread::ST_RUNNABLE);
                }
                // **自己**正在初始化（`<clinit>` 里又引用了自己的静态字段）→ 直接返回，
                // 不能再跑一遍（会一直递归到深度上限）
                Some(_) => return Ok(()),
                None => break, // 没人做 → 我来
            }
        }
        lk(&self.sh.initializing).insert(class.to_string(), me);
        let r = (|| -> Result<(), VmError> {
            if let Some(s) = self.store.super_of(class) {
                self.ensure_init(&s)?;
            }
            if let Some((ci, mi)) = self.store.method_in_class(class, "<clinit>", "()V") {
                self.depth += 1;
                let r = self.execute(ci, mi, "()V", &[], false);
                self.depth -= 1;
                r?;
            }
            Ok(())
        })();
        lk(&self.sh.initializing).remove(class);
        if r.is_ok() {
            // **完成之后**才记进 initialized：先记会让别的线程看到「已完成」却读到默认值
            lk(&self.sh.initialized).insert(class.to_string());
        }
        let waiters: Vec<u64> = lk(&self.sh.init_waiters).remove(class).unwrap_or_default();
        for w in waiters {
            self.wake(w);
        }
        r
    }

    /// actual 是不是 target 或它的子类（**接口也算**：实际类 implements 它即成立）
    pub(super) fn is_subclass(&self, actual: &str, target: &str) -> bool {
        if actual == target {
            return true;
        }
        self.store.chain(actual).iter().any(|c| c == target)
            || self.store.iface_closure(actual).iter().any(|c| c == target)
            || {
                // 链上没加载的 java.* 祖先再按内置表走一遍
                let mut cur = actual.to_string();
                for _ in 0..32 {
                    match builtin_super(&cur) {
                        Some(s) => {
                            if s == target {
                                return true;
                            }
                            cur = s.to_string();
                        }
                        None => break,
                    }
                }
                false
            }
    }

    /// **类型之间**的可赋值关系（`isAssignableFrom` / 数组的 `instanceof` 共用）。
    ///
    /// 比 `is_subclass` 多出来的只有数组那一支：JVMS 4.10.1.2 规定
    /// `String[]` 可以赋给 `Object[]`（元素类型可赋即可）、任何数组都能赋给
    /// `Object` / `Cloneable` / `java.io.Serializable`，而**元素是基本类型时必须完全相等**
    /// （`int[]` 不是 `Object[]` —— 这正是不做这一支会判错的地方）。
    pub(super) fn type_assignable(&self, from: &str, to: &str) -> bool {
        if from == to {
            return true;
        }
        let (fa, ta) = (from.starts_with('['), to.starts_with('['));
        match (fa, ta) {
            (false, false) => self.is_subclass(from, to),
            (true, true) => {
                let (fc, tc) = (&from[1..], &to[1..]);
                // 基本类型元素：只能完全相同（上面已经判过相等）
                if fc.len() == 1 || tc.len() == 1 {
                    return false;
                }
                self.type_assignable(elem_class_name(fc), elem_class_name(tc))
            }
            (true, false) => {
                matches!(to, "java/lang/Object" | "java/lang/Cloneable" | "java/io/Serializable")
            }
            (false, true) => false,
        }
    }

    /// `instanceof` 的判定：null 对任何类型都是 false（含 Object）
    pub(super) fn is_instance(&self, handle: i32, target: &str) -> bool {
        match self.ref_kind(handle) {
            RefKind::Null => false,
            _ if target == "java/lang/Object" => true,
            // 字符串句柄的运行时类**就是** `java.lang.String`：它实现了
            // `CharSequence` / `Comparable` / `Serializable` …，只认名字会让
            // `Collections.sort(List<String>)` 里的那个 checkcast 判失败
            RefKind::Str => self.type_assignable("java/lang/String", target),
            RefKind::Obj(c) => {
                // lambda 对象：它实现的是若干个接口，而这个合成类**不在类表里** ——
                // 只能按登记过的接口集合判（`instanceof Runnable` / checkcast 都靠这里）
                if let Some(l) = self.indy().lambdas.get(&handle) {
                    return l.ifaces.iter().any(|i| self.is_subclass(i, target));
                }
                self.is_subclass(&c, target)
            }
            RefKind::Arr(d) => self.type_assignable(&d, target),
        }
    }

    /// 出错信息里用的可读类型名
    pub(super) fn type_name(&self, handle: i32) -> String {
        match self.ref_kind(handle) {
            RefKind::Null => "null".into(),
            RefKind::Str => "java.lang.String".into(),
            RefKind::Obj(c) => c.replace('/', "."),
            RefKind::Arr(d) => d.replace('/', "."),
        }
    }
}

/// 数组元素描述符 → 类型名（`Ljava/lang/String;` → `java/lang/String`，`[I` → `[I`）
fn elem_class_name(desc: &str) -> &str {
    if desc.len() >= 2 && desc.starts_with('L') && desc.ends_with(';') {
        &desc[1..desc.len() - 1]
    } else {
        desc
    }
}
