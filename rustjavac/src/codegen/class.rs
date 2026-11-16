//! 类级装配：字段表、方法表、常量池收尾、写出。

use crate::ast::*;
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


pub(crate) fn compile_type(package: &Option<String>, t: &TypeDecl) -> Result<Vec<u8>, CompileError> {
    let self_name = match package {
        Some(p) => format!("{}/{}", p.replace('.', "/"), t.name),
        None => t.name.clone(),
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

    // 实例方法：名字 → 描述符
    let mut inst_methods: HashMap<String, String> = HashMap::new();
    for m in &t.members {
        if let Member::Method(md) = m {
            if !md.modifiers.iter().any(|x| x == "static") && !(md.ret.is_none() && md.name == t.name) {
                inst_methods.insert(md.name.clone(), method_descriptor(&md.params, &md.ret));
            }
        }
    }

    // 实例字段：名字 → 下标，以及每个下标的描述符
    let mut field_index: HashMap<String, usize> = HashMap::new();
    let mut field_desc_list: Vec<String> = Vec::new();
    let mut field_ty_list: Vec<Type> = Vec::new();
    for m in &t.members {
        if let Member::Field(f) = m {
            if f.modifiers.iter().any(|x| x == "static") {
                continue;
            }
            field_index.insert(f.name.clone(), field_desc_list.len());
            field_desc_list.push(descriptor(&f.ty));
            field_ty_list.push(f.ty.clone());
        }
    }

    let mut cp = CpBuilder::new();
    let this_class = cp.class(&self_name);
    let super_class = cp.class("java/lang/Object");
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

    let obj_init = cp.method_ref("java/lang/Object", "<init>", "()V");
    if !has_ctor {
        // 默认构造器
        let mut ctor = Emit::new(1);
        ctor.op(0x2A); // aload_0
        ctor.push(1);
        ctor.op_u2(0xB7, obj_init);
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
            // 构造器必须先调用父类构造器
            em.op(0x2A);
            em.push(1);
            em.op_u2(0xB7, obj_init);
            em.pop(1);
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
            finally_stack: Vec::new(),
            ret_ty: md.ret.clone(),
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

