//! 编译上下文：符号表、局部槽分配、局部变量种类、finally 栈。

use crate::ast::*;
use std::collections::HashMap;

pub(crate) struct Ctx {
    pub(crate) locals: HashMap<String, u8>,
    pub(crate) next_slot: u8,
    /// 本类里的静态方法名 → 描述符（用于 invokestatic）
    pub(crate) static_methods: HashMap<String, String>,
    /// 本类实例方法：名字 → 描述符
    pub(crate) instance_methods: HashMap<String, String>,
    /// 本类实例字段：名字 → 下标
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
    /// 外层 `try { ... } finally { ... }` 的 finally 体，**外层在前**。
    /// `return` 要先把这些 finally 各编译一份（javac 的复制策略），再真正返回。
    pub(crate) finally_stack: Vec<Stmt>,
    /// 当前方法的声明返回类型（return 的临时槽要按它写验证类型）
    pub(crate) ret_ty: Option<Type>,
}

impl Ctx {
    pub(crate) fn lookup(&self, name: &str) -> Option<u8> {
        self.locals.get(name).copied()
    }
}
