//! 类级装配：字段表、方法表、常量池收尾、写出。

use crate::ast::*;
use crate::sema::Resolver;
use crate::{write, ClassFile, CpEntry, Member as CfMember};
use std::collections::HashMap;

use super::constpool::CpBuilder;
use super::ctx::Ctx;
use super::emit::Emit;
use super::error::*;
use super::stmt::*;
use super::types::*;

// 类/成员访问标志
pub const ACC_PUBLIC: u16 = 0x0001;
pub const ACC_STATIC: u16 = 0x0008;
pub const ACC_SUPER: u16 = 0x0020;


pub(crate) fn compile_type(
    package: &Option<String>,
    t: &TypeDecl,
    res: &Resolver,
) -> Result<Vec<u8>, CompileError> {
    let self_name = match package {
        Some(p) => format!("{}/{}", p.replace('.', "/"), t.name),
        None => t.name.clone(),
    };
    // 父类：AST 里写了 extends 就用它，否则就是 Object
    let super_name = match res.super_of(&self_name)? {
        Some(s) => s,
        None => "java/lang/Object".to_string(),
    };

    // 第一遍：收集静态方法签名（供 invokestatic 使用）
    let mut static_sigs: HashMap<String, String> = HashMap::new();
    for m in &t.members {
        if let Member::Method(md) = m {
            if md.modifiers.iter().any(|x| x == "static") {
                static_sigs.insert(md.name.clone(), method_descriptor(&md.params, &md.ret));
            }
        }
    }

    // 实例方法：名字 → 描述符（含继承：子类没写的方法调用时落到父类实现）
    let inst_methods = res.instance_methods(&self_name)?;

    // 实例字段布局：父类字段在前、本类字段在后（解释器/JIT 用的是同一套规则）
    let mut field_index: HashMap<String, usize> = HashMap::new();
    let mut field_desc_list: Vec<String> = Vec::new();
    let mut field_ty_list: Vec<Type> = Vec::new();
    for f in res.layout(&self_name)? {
        field_index.insert(f.name.clone(), field_desc_list.len());
        field_ty_list.push(type_from_desc(&f.desc));
        field_desc_list.push(f.desc);
    }

    // 任意一个类的字段/方法表：接收者静态类型可能不是本类（`P p = ...; p.get()`）
    let mut type_fields: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut type_methods: HashMap<String, HashMap<String, String>> = HashMap::new();
    for c in res.unit_classes() {
        if let Ok(f) = res.field_types(&c) {
            type_fields.insert(c.clone(), f);
        }
        if let Ok(m) = res.instance_methods(&c) {
            type_methods.insert(c, m);
        }
    }

    let mut cp = CpBuilder::new();
    let this_class = cp.class(&self_name);
    let super_class = cp.class(&super_name);
    let code_name = cp.utf8("Code");
    let stack_map_name = cp.utf8("StackMapTable");

    let has_ctor = t.members.iter().any(|m| matches!(m, Member::Method(md) if md.ret.is_none() && md.name == t.name));

    let mut methods: Vec<CfMember> = Vec::new();
    let mut fields: Vec<CfMember> = Vec::new();
    for m in &t.members {
        if let Member::Field(f) = m {
            if f.modifiers.iter().any(|x| x == "static") {
                continue;
            }
            let n = cp.utf8(&f.name);
            let d = cp.utf8(&descriptor(&f.ty));
            fields.push(CfMember {
                access_flags: 0x0000,
                name_index: n,
                descriptor_index: d,
                attributes: Vec::new(),
            });
        }
    }

    let super_init = cp.method_ref(&super_name, "<init>", "()V");
    if !has_ctor {
        // 默认构造器：隐式调用父类无参构造器（父类没有就报错，和 javac 一致）
        if !res.has_default_ctor(&super_name)? {
            return err(format!(
                "类 `{}` 的构造器隐式调用了 super()，但父类 `{}` 没有无参构造器",
                t.name,
                super_name.replace('/', ".")
            ));
        }
        let mut ctor = Emit::new(1);
        ctor.op(0x2A); // aload_0
        ctor.push(1);
        ctor.op_u2(0xB7, super_init);
        ctor.pop(1);
        ctor.op(0xB1);
        let ctor_name = cp.utf8("<init>");
        let ctor_desc = cp.utf8("()V");
        methods.push(CfMember {
            access_flags: ACC_PUBLIC,
            name_index: ctor_name,
            descriptor_index: ctor_desc,
            attributes: vec![ctor.to_attribute(code_name)],
        });
    }

    for m in &t.members {
        let md = match m {
            Member::Method(md) => md,
            _ => continue,
        };
        let is_static = md.modifiers.iter().any(|x| x == "static");
        let desc = method_descriptor(&md.params, &md.ret);

        let param_slots: u16 = md.params.iter().map(|p| if matches!(p.ty, Type::Prim("long") | Type::Prim("double")) { 2 } else { 1 }).sum();
        let mut em = Emit::new(if is_static { param_slots } else { param_slots + 1 });
        let is_ctor_m = md.ret.is_none() && md.name == t.name;
        if is_ctor_m {
            // 构造器第一条语句必须是 this(...) / super(...)：
            // 显式写了 super(...) 就交给语句编译去发（不能发两次，否则 VerifyError）；
            // 没写就补一条隐式 super()，父类没有无参构造器时按 javac 的行为报错。
            let ctor_call = ctor_call_position(md.body.as_ref());
            if ctor_call.not_first {
                return err(format!(
                    "类 `{}` 的构造器里 super(...)/this(...) 必须是第一条语句",
                    t.name
                ));
            }
            if !ctor_call.first_is_super {
                if !res.has_default_ctor(&super_name)? {
                    return err(format!(
                        "类 `{}` 的构造器隐式调用了 super()，但父类 `{}` 没有无参构造器",
                        t.name,
                        super_name.replace('/', ".")
                    ));
                }
                em.op(0x2A); // aload_0
                em.push(1);
                em.op_u2(0xB7, super_init);
                em.pop(1);
            }
        }
        let mut ctx = Ctx {
            locals: HashMap::new(),
            next_slot: if is_static { 0 } else { 1 },
            static_methods: static_sigs.clone(),
            instance_methods: inst_methods.clone(),
            fields: field_index.clone(),
            field_descs: field_desc_list.clone(),
            field_tys: field_ty_list.clone(),
            local_kind: HashMap::new(),
            local_ty: HashMap::new(),
            class_name: self_name.clone(),
            super_name: super_name.clone(),
            type_fields: type_fields.clone(),
            type_methods: type_methods.clone(),
            finally_stack: Vec::new(),
            ret_ty: md.ret.clone(),
            in_ctor: is_ctor_m,
        };
        ctx.locals.insert("<this-class>".to_string(), 0);
        if !is_static {
            let this_cp = cp.class(&self_name);
            em.set_local_type(0, this_cp);
        }
        let mut slot = ctx.next_slot;
        for p in &md.params {
            let vt = ver_type(&mut cp, &p.ty);
            em.set_local_type(slot as usize, vt);
            let wide = matches!(p.ty, Type::Prim("long") | Type::Prim("double"));
            if wide {
                em.set_local_type(slot as usize + 1, u16::MAX);
            }
            ctx.locals.insert(p.name.clone(), slot);
            ctx.local_kind.insert(p.name.clone(), kind_of(&p.ty));
            ctx.local_ty.insert(p.name.clone(), p.ty.clone());
            ctx.next_slot += if wide { 2 } else { 1 };
            slot += if wide { 2 } else { 1 };
        }

        // 方法体
        match &md.body {
            Some(body) => {
                let terminated = compile_stmt(body, &mut em, &mut ctx, &mut cp)?;
                if !terminated {
                    // 补默认返回
                    em.op(if desc.ends_with(")V") { 0xB1 } else { 0x03 });
                }
            }
            None => {
                em.op(0xB1);
            }
        }

        em.stack_map_name = stack_map_name;
        em.resolve()?;
        let is_ctor = md.ret.is_none() && md.name == t.name;
        let method_name = if is_ctor { "<init>" } else { md.name.as_str() };
        let name_index = cp.utf8(method_name);
        let desc_index = cp.utf8(&desc);
        let flags = if is_static { ACC_PUBLIC | ACC_STATIC } else { ACC_PUBLIC };
        methods.push(CfMember {
            access_flags: flags,
            name_index,
            descriptor_index: desc_index,
            attributes: vec![em.to_attribute(code_name)],
        });
    }

    // invokestatic 的 owner 用真实类名补齐（此前占位 <SELF>）
    let cp_entries = fix_self_refs(cp, &self_name);

    let cf = ClassFile {
        minor_version: 0,
        major_version: 69,
        constant_pool: cp_entries,
        access_flags: ACC_PUBLIC | ACC_SUPER,
        this_class,
        super_class,
        interfaces: Vec::new(),
        fields,
        methods,
        attributes: Vec::new(),
    };
    Ok(write(&cf))
}

/// 把常量池里指向占位类 `<SELF>` 的 MethodRef 改成真实类名索引。
/// 做法简单：追加一个真实类名 Utf8，并把 MethodRef 的 class_index 指过去。
pub(crate) fn fix_self_refs(cp: CpBuilder, self_name: &str) -> Vec<Option<CpEntry>> {
    let mut entries = cp.entries();
    // 找 <SELF> 的 Utf8 索引
    let mut self_utf8 = None;
    for (i, e) in entries.iter().enumerate() {
        if let Some(CpEntry::Utf8(b)) = e {
            if b == b"<SELF>" {
                self_utf8 = Some(i as u16);
            }
        }
    }
    let Some(self_utf8) = self_utf8 else {
        return entries;
    };
    // 找指向它的 Class 项
    let mut self_class = None;
    for (i, e) in entries.iter().enumerate() {
        if let Some(CpEntry::Class(n)) = e {
            if *n == self_utf8 {
                self_class = Some(i as u16);
            }
        }
    }
    // 追加真实类名
    let real_utf8 = entries.len() as u16;
    entries.push(Some(CpEntry::Utf8(self_name.as_bytes().to_vec())));
    let real_class = entries.len() as u16;
    entries.push(Some(CpEntry::Class(real_utf8)));
    let _ = self_class;
    // 所有 MethodRef 的 class 指向占位类的，改指真实类
    for e in entries.iter_mut() {
        if let Some(CpEntry::MethodRef(c, nt)) = e {
            if Some(*c) == self_class {
                *c = real_class;
            }
            let _ = nt;
        }
    }
    entries
}


/// 构造器第一条语句的情况
struct CtorCallPos {
    /// 第一条语句就是 `super(...)`
    first_is_super: bool,
    /// `super(...)` / `this(...)` 出现在了非首位（javac 也不允许）
    not_first: bool,
}

/// 判断构造器体里 `super(...)` 的**位置**
fn ctor_call_position(body: Option<&Stmt>) -> CtorCallPos {
    let mut pos = CtorCallPos { first_is_super: false, not_first: false };
    let list: Vec<&Stmt> = match body {
        Some(Stmt::Block(items)) => items.iter().collect(),
        Some(s) => vec![s],
        None => return pos,
    };
    let mut first_seen = false;
    for s in &list {
        if matches!(s, Stmt::Empty) {
            continue;
        }
        if !first_seen {
            first_seen = true;
            if is_super_call_stmt(s) {
                pos.first_is_super = true;
                continue;
            }
        }
        if contains_ctor_call(s) {
            pos.not_first = true;
        }
    }
    pos
}

fn is_super_call_stmt(s: &Stmt) -> bool {
    matches!(s, Stmt::Expr(Expr::Call { recv: Some(r), name, .. })
        if name == "super" && matches!(**r, Expr::Super))
}

/// 语句（含嵌套）里有没有 `super(...)` / `this(...)`
fn contains_ctor_call(s: &Stmt) -> bool {
    match s {
        Stmt::Block(items) => items.iter().any(contains_ctor_call),
        Stmt::If { then, els, .. } => contains_ctor_call(then) || els.as_ref().is_some_and(|e| contains_ctor_call(e)),
        Stmt::While { body, .. } | Stmt::DoWhile { body, .. } | Stmt::For { body, .. } | Stmt::ForEach { body, .. } => {
            contains_ctor_call(body)
        }
        Stmt::Try { body, catches, finally } => {
            contains_ctor_call(body)
                || catches.iter().any(|c| contains_ctor_call(&c.body))
                || finally.as_ref().is_some_and(|f| contains_ctor_call(f))
        }
        Stmt::Switch { cases, .. } => cases.iter().any(|c| c.body.iter().any(contains_ctor_call)),
        Stmt::Expr(Expr::Call { recv: None, name, .. }) => name == "this",
        other => is_super_call_stmt(other),
    }
}
