//! 编译上下文：符号表、局部槽分配、局部变量种类、finally 栈。
//!
//! 注意 `fields` / `instance_methods` 都是**含继承**的视图：子类的
//! `get()` 即使声明在父类里，也能按名字解析到（描述符与偏移沿继承链取）。
//! 另有 `type_fields` / `type_methods` 保存「任意一个类」的这两张表 ——
//! 接收者静态类型不是本类时（`P p = ...; p.get()`）按它查。

use crate::ast::*;
use std::collections::HashMap;

/// 一个 `break` / `continue` 的落点，外加它的**语句标签**（`outer: for (…)` 里的 `outer`）。
///
/// 带标签的 `break outer` 从内向外找第一个 `label == Some("outer")` 的落点，
/// 不带标签的取最内层 —— 两者走同一张栈，只是查找规则不同。
#[derive(Clone, Debug)]
pub(crate) struct JumpTarget {
    pub(crate) label: Option<String>,
    pub(crate) target: usize,
}

#[derive(Clone)]
pub(crate) struct Ctx {
    pub(crate) locals: HashMap<String, u8>,
    pub(crate) next_slot: u8,
    /// 本类里的静态方法名 → 描述符（用于 invokestatic）
    pub(crate) static_methods: HashMap<String, Vec<String>>,
    /// 本类**及其父类**的实例方法：名字 → 描述符
    pub(crate) instance_methods: HashMap<String, Vec<String>>,
    /// 本类实例字段（含继承）：名字 → 下标
    pub(crate) fields: HashMap<String, usize>,
    /// 字段下标 → 描述符
    pub(crate) field_descs: Vec<String>,
    /// 字段下标 → 声明类型（选 getfield/putfield 的宽度与 anewarray 的元素类要用）
    pub(crate) field_tys: Vec<Type>,
    /// 局部变量名 → 类型种类（0=int,1=long,2=ref）
    pub(crate) local_kind: HashMap<String, u8>,
    /// 局部变量名 → 声明类型（选数组元素宽度、anewarray/checkcast 的元素类要用）
    pub(crate) local_ty: HashMap<String, Type>,
    /// 当前类名（内部形式 java/lang/X）
    pub(crate) class_name: String,
    /// 父类内部名（没有 extends 就是 java/lang/Object）
    pub(crate) super_name: String,
    /// 任意类的实例字段表：类内部名 → (字段名 → 描述符)，含继承
    pub(crate) type_fields: HashMap<String, std::sync::Arc<HashMap<String, String>>>,
    /// 任意类的实例方法表：类内部名 → (方法名 → 描述符)，含继承
    pub(crate) type_methods: HashMap<String, std::sync::Arc<HashMap<String, Vec<String>>>>,
    /// 接口类型（内部名）集合：调用点据此发 invokeinterface
    pub(crate) iface_types: std::collections::HashSet<String>,
    /// 本类（含继承）静态字段：名字 → (声明类, 描述符)
    pub(crate) static_fields: HashMap<String, (String, String)>,
    /// 任意类的静态字段表：类内部名 → (字段名 → (声明类, 描述符))
    pub(crate) type_static_fields: HashMap<String, std::sync::Arc<HashMap<String, (String, String)>>>,
    /// 任意类的静态方法表：类内部名 → (方法名 → 描述符)
    pub(crate) type_static_methods: HashMap<String, std::sync::Arc<HashMap<String, Vec<String>>>>,
    /// 外层 `try { ... } finally { ... }` 的 finally 体，**外层在前**。
    /// `return` 要先把这些 finally 各编译一份（javac 的复制策略），再真正返回。
    pub(crate) finally_stack: Vec<Stmt>,
    /// 当前方法的声明返回类型（return 的临时槽要按它写验证类型）
    pub(crate) ret_ty: Option<Type>,
    /// 当前方法是不是构造器（`super(...)` 只允许出现在构造器里）
    pub(crate) in_ctor: bool,
    /// 外层循环/switch 的跳出标签（`break` 跳到最内层的那个）
    pub(crate) break_labels: Vec<JumpTarget>,
    /// 外层循环的下一轮标签（`continue`；switch 不产生 continue 目标）
    pub(crate) cont_labels: Vec<JumpTarget>,
    /// 降糖用的临时名字计数器（for-each → 下标/迭代器变量要有唯一名字）
    pub(crate) tmp_seq: u32,
    /// `label: for (...)` 里刚读到、还没交给循环体的标签名
    pub(crate) pending_label: Option<String>,
    /// 静态导入：成员名 → 声明类内部名（通配记在 `"*"` 下）。
    /// `requireNonNull(x)` / `UTF_8` 这种没有接收者的写法全靠它找到声明类。
    pub(crate) static_imports: HashMap<String, Vec<String>>,
    /// 任意类的构造器描述符表：类内部名 → 所有 `<init>` 的描述符。
    /// `new GenBox<>("x")` 的真实描述符是 `(Ljava/lang/Object;)V`（泛型擦除），
    /// 不能拿实参类型现拼。
    pub(crate) type_ctors: HashMap<String, std::sync::Arc<Vec<String>>>,
}

impl Ctx {
    pub(crate) fn lookup(&self, name: &str) -> Option<u8> {
        self.locals.get(name).copied()
    }

    /// 拿一个独一无二的临时名字后缀（for-each 降糖 / 带标签的语句用）
    pub(crate) fn next_tmp(&mut self) -> u32 {
        self.tmp_seq += 1;
        self.tmp_seq
    }
}
