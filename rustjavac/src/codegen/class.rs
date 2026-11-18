//! 类级装配：字段表、方法表、常量池收尾、写出。

use crate::ast::*;
use crate::sema::Resolver;
use crate::{write, ClassFile, CpEntry, Member as CfMember};
use std::collections::HashMap;

use super::constpool::CpBuilder;
use super::ctx::Ctx;
use super::emit::Emit;
use super::error::*;
use super::expr::{compile_expr, emit_init, emit_val, needs_scratch, spill, unspill};
use super::stmt::*;
use super::types::*;

// 类/成员访问标志
pub const ACC_PUBLIC: u16 = 0x0001;
pub const ACC_STATIC: u16 = 0x0008;
pub const ACC_SUPER: u16 = 0x0020;
/// 接口的访问标志：public interface abstract（接口**不能**带 ACC_SUPER）
pub const ACC_INTERFACE: u16 = 0x0200;
pub const ACC_ABSTRACT: u16 = 0x0400;

/// 声明里的修饰符 → access_flags。
///
/// 以前字段/方法**一律写成 ACC_PUBLIC**：`javap` 读得出来，但真 JVM 会因为
/// 语义不对而拒载或行为错乱 —— `private static native int tty();` 被标成
/// `public abstract`（非抽象类里有 abstract 方法 = ClassFormatError）、
/// 子类覆写父类 `final` 方法（`overrides final method`）、接口字段不是
/// `public static final`（JVM 规范 4.5 强制要求）。
fn modifier_flags(mods: &[String]) -> u16 {
    let mut f = 0u16;
    for m in mods {
        f |= match m.as_str() {
            "public" => 0x0001,
            "private" => 0x0002,
            "protected" => 0x0004,
            "static" => ACC_STATIC,
            "final" => 0x0010,
            "synchronized" => 0x0020,
            "volatile" => 0x0040,
            "transient" => 0x0080,
            "native" => 0x0100,
            "abstract" => ACC_ABSTRACT,
            "strictfp" => 0x0800,
            _ => 0,
        };
    }
    f
}


pub(crate) fn compile_type(
    package: &Option<String>,
    t: &TypeDecl,
    res: &Resolver,
) -> Result<Vec<u8>, CompileError> {
    let self_name = match package {
        Some(p) => format!("{}/{}", p.replace('.', "/"), t.name),
        None => t.name.clone(),
    };
    let is_iface = t.kind == TypeKind::Interface;
    // 父类：AST 里写了 extends 就用它，否则就是 Object（接口的 extends 是父接口，见 interfaces）
    let super_name = match res.super_of(&self_name)? {
        Some(s) => s,
        None => "java/lang/Object".to_string(),
    };

    // 第一遍：收集静态方法签名（供 invokestatic 使用）。同名重载全留，调用点按实参挑
    let mut static_sigs: HashMap<String, Vec<String>> = HashMap::new();
    for m in &t.members {
        if let Member::Method(md) = m {
            if md.modifiers.iter().any(|x| x == "static") {
                static_sigs.entry(md.name.clone()).or_default().push(method_descriptor(&md.params, &md.ret));
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
    // 平台部分按 classpath 进程级缓存，本单元声明的类由 `world()` 覆盖上去 —— 每类型
    // 从零重建这六张表在 classpath 有几千个类时是不可接受的。
    let w = res.world();
    let type_fields = w.type_fields;
    let type_methods = w.type_methods;
    let type_static_fields = w.type_static_fields;
    let type_static_methods = w.type_static_methods;
    // 接口类型名集合：调用点据此选 invokeinterface（0xB9）而不是 invokevirtual
    let iface_types = w.iface_types;
    let type_ctors = w.type_ctors;
    let iface_sam = w.iface_sam;
    let type_hidden = w.type_hidden;
    let type_supers = w.type_supers;
    let type_method_generics = w.type_method_generics;
    // 本类（含继承）的静态字段：名字 → (声明类, 描述符)
    let static_fields_map = res.static_fields(&self_name)?;

    let mut cp = CpBuilder::new();
    let this_class = cp.class(&self_name);
    let super_class = cp.class(&super_name);
    let code_name = cp.utf8("Code");
    let stack_map_name = cp.utf8("StackMapTable");

    // 方法/静态初始化块共用的上下文底板（每个方法 clone 一份再按需改）。
    // 放在这里是因为**隐式默认构造器**（下面那段）也要用它编实例字段初始化式。
    let base_ctx = Ctx {
        locals: HashMap::new(),
        next_slot: 0,
        static_methods: static_sigs.clone(),
        inherited_statics: {
            // 继承来的静态方法（父类 / 外层类）。本类自己声明的已经在 static_methods 里
            let mut m: HashMap<String, (String, Vec<String>)> = HashMap::new();
            if let Ok(all) = res.static_methods(&self_name) {
                for (name, mut cands) in all {
                    if static_sigs.contains_key(&name) {
                        continue;
                    }
                    if let Some((desc, owner)) = cands.pop() {
                        m.insert(name, (owner, cands.into_iter().map(|(d, _)| d).chain(std::iter::once(desc)).collect()));
                    }
                }
            }
            m
        },
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
        iface_types: iface_types.clone(),
        static_fields: static_fields_map.clone(),
        type_static_fields: type_static_fields.clone(),
        type_static_methods: type_static_methods.clone(),
        finally_stack: Vec::new(),
        ret_ty: None,
        in_ctor: false,
        break_labels: Vec::new(),
        cont_labels: Vec::new(),
        tmp_seq: 0,
        pending_label: None,
        static_imports: res.static_imports().clone(),
        type_ctors: type_ctors.clone(),
        type_hidden: type_hidden.clone(),
        type_supers: type_supers.clone(),
        type_method_generics: type_method_generics.clone(),
        iface_sam: iface_sam.clone(),
        synth: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
        lambda_seq: std::rc::Rc::new(std::cell::RefCell::new(0u32)),
        method_name: String::new(),
        lambda_target: std::rc::Rc::new(std::cell::RefCell::new(None)),
        has_this: false,
    };

    let has_ctor = t.members.iter().any(|m| matches!(m, Member::Method(md) if md.ret.is_none() && md.name == t.name));

    let mut methods: Vec<CfMember> = Vec::new();
    let mut fields: Vec<CfMember> = Vec::new();
    for m in &t.members {
        if let Member::Field(f) = m {
            // 接口里的字段一律是 public static final
            let is_static_f = f.modifiers.iter().any(|x| x == "static") || is_iface;
            let n = cp.utf8(&f.name);
            let d = cp.utf8(&descriptor(&f.ty));
            // 静态字段也要写进字段表：getstatic/putstatic 靠它解析（值本身在 <clinit> 里初始化）
            let mut flags = modifier_flags(&f.modifiers);
            if is_iface {
                // JVM 规范 4.5：接口字段**必须** ACC_PUBLIC|ACC_STATIC|ACC_FINAL，否则 ClassFormatError
                flags |= ACC_PUBLIC | ACC_STATIC | 0x0010;
            } else if is_static_f {
                flags |= ACC_STATIC;
            }
            fields.push(CfMember {
                access_flags: flags,
                name_index: n,
                descriptor_index: d,
                attributes: Vec::new(),
            });
        }
    }

    // 实例字段的初始化式：要在每个构造器里、`super(...)` 之后执行（JLS 8.8.7）。
    // 以前这里根本没编 —— `class W { long f = 5; }` 读出来是 0，且没有任何报错。
    let instance_field_inits: Vec<(String, String, Expr)> = t
        .members
        .iter()
        .filter_map(|m| match m {
            Member::Field(f)
                if f.init.is_some()
                    && !f.modifiers.iter().any(|x| x == "static")
                    && !is_iface =>
            {
                Some((f.name.clone(), descriptor(&f.ty), f.init.clone().unwrap()))
            }
            _ => None,
        })
        .collect();

    let super_init = cp.method_ref(&super_name, "<init>", "()V");
    if !has_ctor && !is_iface {
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
        // 隐式构造器同样要跑实例字段初始化（绝大多数类没有显式构造器，漏了这里
        // 就等于「所有字段都只拿到默认值」）
        {
            let mut cctx = base_ctx.clone();
            // 实例字段的初始化式跑在构造器里 —— `this` 可用（lambda 体里访问实例字段
            // 要靠它把合成方法编成实例方法，否则会生成 `static` 方法里 `aload_0` 的坏字节码）
            cctx.has_this = true;
            cctx.method_name = "<init>".to_string();
            cctx.locals.insert("<this-class>".to_string(), 0);
            emit_instance_field_inits(&instance_field_inits, &mut ctor, &mut cctx, &mut cp)?;
        }
        ctor.op(0xB1);
        let ctor_name = cp.utf8("<init>");
        let ctor_desc = cp.utf8("()V");
        methods.push(CfMember {
            access_flags: modifier_flags(&t.modifiers) & 0x0007,
            name_index: ctor_name,
            descriptor_index: ctor_desc,
            attributes: vec![ctor.to_attribute(code_name)],
        });
    }

    // 实例字段的初始化式：要在每个构造器里、`super(...)` 之后执行（JLS 8.8.7）。
    // 以前这里根本没编 —— `class W { long f = 5; }` 读出来是 0，且没有任何报错。
    let instance_field_inits: Vec<(String, String, Expr)> = t
        .members
        .iter()
        .filter_map(|m| match m {
            Member::Field(f)
                if f.init.is_some()
                    && !f.modifiers.iter().any(|x| x == "static")
                    && !is_iface =>
            {
                Some((f.name.clone(), descriptor(&f.ty), f.init.clone().unwrap()))
            }
            _ => None,
        })
        .collect();

    // 有没有抽象方法（决定了类本身必须带 ACC_ABSTRACT，否则 ClassFormatError）
    let mut has_abstract = false;
    for m in &t.members {
        let md = match m {
            Member::Method(md) => md,
            _ => continue,
        };
        let is_static = md.modifiers.iter().any(|x| x == "static");
        let desc = method_descriptor(&md.params, &md.ret);
        let method_label = format!("{}.{}{}", t.name, md.name, desc);

        // 抽象方法（含接口里**没有方法体**的声明）没有 Code 属性 —— 写进去 HotSpot 直接 ClassFormatError。
        // 接口里带方法体的是 default 方法（Java 8+），静态接口方法也一样，都要真编出字节码。
        let is_abstract_m = md.modifiers.iter().any(|x| x == "abstract") || md.body.is_none();
        if is_abstract_m {
            let name_index = cp.utf8(&md.name);
            let desc_index = cp.utf8(&desc);
            // `native` 方法也**没有 Code**，但它不是 abstract：把 native 标成
            // abstract 会让「非抽象类里有抽象方法」成为 ClassFormatError。
            let mut flags = modifier_flags(&md.modifiers);
            if is_iface {
                // 接口里没有方法体的方法必须 ACC_PUBLIC|ACC_ABSTRACT（JVM 规范 4.6）
                flags = (flags & !0x0002) | ACC_PUBLIC | ACC_ABSTRACT;
            } else if flags & 0x0100 == 0 {
                flags |= ACC_ABSTRACT;
            }
            has_abstract = has_abstract || flags & ACC_ABSTRACT != 0;
            methods.push(CfMember {
                access_flags: flags,
                name_index,
                descriptor_index: desc_index,
                attributes: Vec::new(),
            });
            continue;
        }

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
            if !ctor_call.first_is_ctor_call {
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
        let mut ctx = base_ctx.clone();
        ctx.next_slot = if is_static { 0 } else { 1 };
        ctx.ret_ty = md.ret.clone();
        ctx.in_ctor = is_ctor_m;
        ctx.has_this = !is_static;
        if ctx.has_this {
            ctx.locals.insert("<this-class>".to_string(), 0);
        }
        ctx.method_name = md.name.clone();
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
                // 报错要带方法名：codegen 没有行号信息，一个类几百个方法时光看
                // 「未找到实例方法 clone」根本定位不到是哪一处
                let in_method = |e: CompileError| CompileError {
                    msg: format!("{}（方法 `{}`）", e.msg, method_label),
                };
                let terminated = if is_ctor_m && !ctor_delegates_to_this(md.body.as_ref()) {
                    // 构造器：实例字段的初始化式要在 `super(...)` / `this(...)` 之后、
                    // 构造器体之前执行（JLS 8.8.7）。`this(...)` 委托时由被委托的
                    // 那个构造器负责初始化，这里不能再做一遍。
                    compile_ctor_body(body, &instance_field_inits, &mut em, &mut ctx, &mut cp)
                        .map_err(in_method)?
                } else {
                    compile_stmt(body, &mut em, &mut ctx, &mut cp).map_err(in_method)?
                };
                if !terminated {
                    // 补一条默认返回。方法体里所有路径都 return 过时这里是死代码，
                    // 但字节码**必须**能通过验证（HotSpot: control flow falls through code end），
                    // 所以按返回类型给一条合法的「常量 + xreturn」，不能只压个常量。
                    push_default_return(&mut em, desc.rsplit(')').next().unwrap_or("V"));
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
        let mut flags = if is_static { ACC_STATIC } else { 0 } | modifier_flags(&md.modifiers);
        if is_iface && flags & 0x0007 == 0 {
            // 接口的实例/静态方法必须是 public（Java 9+ 只放开 private）：
            // 包可见的方法放在接口里是 ClassFormatError
            flags |= ACC_PUBLIC;
        }
        methods.push(CfMember {
            access_flags: flags,
            name_index,
            descriptor_index: desc_index,
            attributes: vec![em.to_attribute(code_name)],
        });
    }

    // ---- 桥接方法 ----
    //
    // 泛型擦除后父类型上的方法描述符是**上界**（`interface Holder<T> { T value(); }`
    // 擦成 `()Ljava/lang/Object;`），实现类写的是具体类型 `()Ljava/lang/String;`。
    // 没有桥，`Holder<String> h = ...; h.value()`（调用点按擦除后的描述符发
    // invokeinterface）在运行时就找不到方法。
    //
    // 桥只做一次转发：`aload_0; <各形参>; invokevirtual 具体实现; xreturn`。
    // 判据：同名、形参描述符逐个相同、返回描述符不同 —— 这正是协变返回/泛型实现的样子。
    {
        let mut parents: Vec<String> = vec![super_name.clone()];
        for i in t.implements.iter() {
            if let Some(n) = type_internal(i) {
                if !parents.contains(&n) {
                    parents.push(n);
                }
            }
        }
        let mut bridges: Vec<(String, String, String)> = Vec::new(); // (名字, 桥描述符, 目标描述符)
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for m in &t.members {
            let Member::Method(md) = m else { continue };
            if md.ret.is_none() || md.name == t.name || md.body.is_none() {
                continue;
            }
            if md.modifiers.iter().any(|x| x == "static") {
                continue;
            }
            let ours = method_descriptor(&md.params, &md.ret);
            let our_params = desc_params(&ours);
            for p in &parents {
                let Some(map) = type_methods.get(p) else { continue };
                for pdesc in map.get(&md.name).into_iter().flatten() {
                    if *pdesc == ours || desc_params(pdesc) != our_params {
                        continue;
                    }
                    if !seen.insert(format!("{}{}", md.name, pdesc)) {
                        continue;
                    }
                    bridges.push((md.name.clone(), pdesc.clone(), ours.clone()));
                }
            }
        }
        for (bname, bdesc, tdesc) in bridges {
            let pdescs = desc_params(&bdesc);
            let mut slots: u16 = 1; // 0 是 this
            let mut em = Emit::new(1 + pdescs.iter().map(|p| if p.starts_with('J') || p.starts_with('D') { 2 } else { 1 }).sum::<u16>());
            em.op(0x2A); // aload_0
            em.push(1);
            for p in &pdescs {
                let k = kind_from_desc(p);
                em.load_slot(slots as u8, k)?;
                slots += if kind_wide(k) { 2 } else { 1 };
            }
            let idx = cp.method_ref(&self_name, &bname, &tdesc);
            em.op_u2(0xB6, idx); // invokevirtual 具体实现
            em.pop(desc_slots(&tdesc) + 1);
            let rk = kind_from_ret(&bdesc);
            em.push(if kind_wide(rk) { 2 } else { 1 });
            super::method::emit_xreturn(&mut em, rk);
            let n = cp.utf8(&bname);
            let d = cp.utf8(&bdesc);
            methods.push(CfMember {
                // ACC_PUBLIC | ACC_BRIDGE | ACC_SYNTHETIC
                access_flags: ACC_PUBLIC | 0x0040 | 0x1000,
                name_index: n,
                descriptor_index: d,
                attributes: vec![em.to_attribute(code_name)],
            });
        }
    }

    // 静态初始化 `<clinit>`：静态字段的初始化式与 `static { }` 块按**源码顺序**执行
    // （javac 的行为，字段与块交替出现时也是这个顺序）。
    let has_clinit = t.members.iter().any(|m| match m {
        Member::Init { is_static, .. } => *is_static,
        Member::Field(f) => {
            f.init.is_some() && (f.modifiers.iter().any(|x| x == "static") || is_iface)
        }
        _ => false,
    });
    if has_clinit {
        let mut em = Emit::new(0);
        let mut ctx = base_ctx.clone();
        let mut terminated = false;
        for m in &t.members {
            match m {
                Member::Field(f)
                    if f.init.is_some()
                        && (f.modifiers.iter().any(|x| x == "static") || is_iface) =>
                {
                    let desc = descriptor(&f.ty);
                    let init = f.init.as_ref().unwrap();
                    emit_init(&f.ty, init, &mut em, &ctx, &mut cp)?;
                    // `static long X = 1;` / `static double D = 1;`：初始化式是 int，
                    // 不插 i2l/i2d 就会写出 `bipush 1; putstatic long`（HotSpot VerifyError）
                    coerce_to(&f.ty, init, &ctx, &mut em, &mut cp)?;
                    let idx = cp.field_ref(&self_name, &f.name, &desc);
                    em.op_u2(0xB3, idx); // putstatic
                    em.pop(if kind_wide(kind_from_desc(&desc)) { 2 } else { 1 });
                }
                Member::Init { is_static: true, body } => {
                    terminated = compile_stmt(body, &mut em, &mut ctx, &mut cp)?;
                }
                _ => {}
            }
        }
        if !terminated {
            em.op(0xB1); // return
        }
        em.stack_map_name = stack_map_name;
        em.resolve()?;
        let n = cp.utf8("<clinit>");
        let d = cp.utf8("()V");
        methods.push(CfMember {
            access_flags: ACC_STATIC,
            name_index: n,
            descriptor_index: d,
            attributes: vec![em.to_attribute(code_name)],
        });
    }

    // ---- 合成方法：lambda 体 / 方法引用的转发体 ----
    //
    // 它们是在编方法体的过程中「顺手登记」进来的（那时还不知道要编成什么），
    // 所以在这里统一编。合成方法体里可能又冒出新的 lambda，所以用**下标推进**的
    // while 循环，让队列自己长大。
    {
        let mut i = 0usize;
        loop {
            let item = { base_ctx.synth.borrow().get(i).cloned() };
            let Some(s) = item else { break };
            i += 1;
            let ret_desc = s.desc.rsplit(')').next().unwrap_or("V").to_string();
            let slots: u16 = s.params.iter().map(|(_, t)| kind_width(t)).sum();
            let cap_slots: u16 = s.captures.iter().map(|(_, t)| kind_width(t)).sum();
            let mut em = Emit::new(slots + cap_slots + if s.is_instance { 1 } else { 0 });
            let mut c = base_ctx.clone();
            c.next_slot = 0;
            c.ret_ty = Some(type_from_desc(&ret_desc));
            c.in_ctor = false;
            c.has_this = s.is_instance;
            c.method_name = s.name.clone();
            c.lambda_target.replace(None);
            c.break_labels.clear();
            c.cont_labels.clear();
            if s.is_instance {
                let this_cp = cp.class(&self_name);
                em.set_local_type(0, this_cp);
                c.locals.insert("<this-class>".to_string(), 0);
                c.next_slot = 1;
            }
            for (n, t) in s.captures.iter().chain(s.params.iter()) {
                let slot = c.next_slot;
                let wide = kind_width(t) == 2;
                let vt = ver_type(&mut cp, t);
                em.set_local_type(slot as usize, vt);
                if wide {
                    em.set_local_type(slot as usize + 1, u16::MAX);
                }
                c.locals.insert(n.clone(), slot);
                c.local_kind.insert(n.clone(), kind_of(t));
                c.local_ty.insert(n.clone(), t.clone());
                c.next_slot += if wide { 2 } else { 1 };
            }
            // 体：`return e;` / 语句块
            let terminated = compile_stmt(&s.body, &mut em, &mut c, &mut cp)
                .map_err(|e| CompileError { msg: format!("{}（lambda `{}`）", e.msg, s.name) })?;
            if !terminated {
                push_default_return(&mut em, &ret_desc);
            }
            em.stack_map_name = stack_map_name;
            em.resolve()?;
            let n = cp.utf8(&s.name);
            let d = cp.utf8(&s.desc);
            methods.push(CfMember {
                // ACC_PRIVATE | ACC_SYNTHETIC（+ STATIC）：javac 也把 lambda 体写成
                // private synthetic，LambdaMetafactory 只在同一个类里找得到它
                access_flags: 0x0002 | 0x1000 | if s.is_instance { 0 } else { ACC_STATIC },
                name_index: n,
                descriptor_index: d,
                attributes: vec![em.to_attribute(code_name)],
            });
        }
    }

    // 接口表：`implements A, B`（接口自己的 `extends A, B` 也算父接口）
    let iface_idxs: Vec<u16> = res.interfaces_of(&self_name).iter().map(|n| cp.class(n)).collect();

    // invokestatic 的 owner 用真实类名补齐（此前占位 <SELF>）
    let bsm = cp.take_bootstrap();
    // BootstrapMethods 属性要在常量池还没被交出去之前登记它的名字
    let bsm_attr = bootstrap_attribute(&mut cp, &bsm);
    let cp_entries = fix_self_refs(cp, &self_name);

    let cf = ClassFile {
        minor_version: 0,
        major_version: 69,
        constant_pool: cp_entries,
        // 接口：public interface abstract，且**不能**带 ACC_SUPER
        access_flags: {
            let m = &t.modifiers;
            let vis = if m.iter().any(|x| x == "public") { ACC_PUBLIC } else { 0 };
            if is_iface {
                vis | ACC_INTERFACE | ACC_ABSTRACT
            } else {
                let mut f = vis | ACC_SUPER;
                if m.iter().any(|x| x == "final") {
                    f |= 0x0010;
                }
                if m.iter().any(|x| x == "abstract") || has_abstract {
                    f |= ACC_ABSTRACT;
                }
                f
            }
        },
        this_class,
        super_class,
        interfaces: iface_idxs,
        fields,
        methods,
        attributes: bsm_attr.into_iter().collect(),
    };
    Ok(write(&cf))
}

/// 声明类型的槽宽度（long/double 占 2）
fn kind_width(t: &Type) -> u16 {
    if kind_wide(kind_of(t)) {
        2
    } else {
        1
    }
}

/// 方法体走完还没 return 时补一条**合法**的默认返回。
///
/// 字节码必须能通过验证（HotSpot: control flow falls through code end），
/// 所以按返回类型给一条「常量 + xreturn」，不能只压个常量、也不能什么都不发。
fn push_default_return(em: &mut Emit, ret: &str) {
    match ret.as_bytes().first().copied() {
        Some(b'V') => em.op(0xB1),
        Some(b'J') => {
            em.op(0x09); // lconst_0
            em.push(2);
            em.op(0xAD); // lreturn
        }
        Some(b'D') => {
            em.op(0x0E); // dconst_0
            em.push(2);
            em.op(0xAF); // dreturn
        }
        Some(b'L') | Some(b'[') => {
            em.op(0x01); // aconst_null
            em.push(1);
            em.op(0xB0); // areturn
        }
        _ => {
            em.op(0x03); // iconst_0
            em.push(1);
            em.op(0xAC); // ireturn
        }
    }
}

/// 把常量池里的 `BootstrapMethods` 表写成类属性。
///
/// `invokedynamic` 的操作数就是这张表的下标，所以属性必须和常量池一起写出来；
/// 一项 `u2 bootstrap_method_ref; u2 num_args; u2 args[]`。
fn bootstrap_attribute(cp: &mut CpBuilder, bsm: &[(u16, Vec<u16>)]) -> Option<crate::Attribute> {
    if bsm.is_empty() {
        return None;
    }
    let name_index = cp.utf8("BootstrapMethods");
    let mut info = Vec::new();
    info.extend_from_slice(&(bsm.len() as u16).to_be_bytes());
    for (m, args) in bsm {
        info.extend_from_slice(&m.to_be_bytes());
        info.extend_from_slice(&(args.len() as u16).to_be_bytes());
        for a in args {
            info.extend_from_slice(&a.to_be_bytes());
        }
    }
    Some(crate::Attribute { name_index, info })
}

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
    /// 第一条语句就是 `super(...)` 或 `this(...)`
    first_is_ctor_call: bool,
    /// `super(...)` / `this(...)` 出现在了非首位（javac 也不允许）
    not_first: bool,
}

/// 发实例字段初始化：`aload_0; <初始化式>; coerce; putfield`，按源码顺序。
/// 调用点必须在 `super(...)` 之后、构造器体之前。
fn emit_instance_field_inits(
    inits: &[(String, String, Expr)],
    em: &mut Emit,
    ctx: &mut Ctx,
    cp: &mut CpBuilder,
) -> Result<(), CompileError> {
    for (name, desc, init) in inits {
        let ty = type_from_desc(desc);
        let em_dir = if needs_scratch(init) { Some(spill(init, em, ctx, cp)?) } else { None };
        em.op(0x2A); // aload_0
        em.push(1);
        match em_dir {
            Some(sp) => unspill(&sp, em)?,
            None => emit_init(&ty, init, em, ctx, cp)?,
        }
        coerce_to(&type_from_desc(desc), init, ctx, em, cp)?;
        let f = cp.field_ref(&ctx.class_name, name, desc);
        em.op_u2(0xB5, f); // putfield
        em.pop(if kind_wide(kind_from_desc(desc)) { 3 } else { 2 });
    }
    Ok(())
}

/// 构造器体里有没有 `this(...)` 委托（有的话字段初始化交给被委托的构造器）
fn ctor_delegates_to_this(body: Option<&Stmt>) -> bool {
    let list: Vec<&Stmt> = match body {
        Some(Stmt::Block(items)) => items.iter().collect(),
        Some(s) => vec![s],
        None => return false,
    };
    for s in list {
        if matches!(s, Stmt::Empty) {
            continue;
        }
        return matches!(s, Stmt::Expr(Expr::Call { recv: None, name, .. }) if name == "this");
    }
    false
}

/// 编译构造器体：`super(...)`（如果显式写了）→ 实例字段初始化 → 其余语句。
///
/// `inits` 是 `(字段名, 描述符, 初始化式)`，按源码顺序。字段初始化必须夹在
/// `super(...)` 和构造器体之间 —— 提前会把父类字段还没就绪的对象写坏。
fn compile_ctor_body(
    body: &Stmt,
    inits: &[(String, String, Expr)],
    em: &mut Emit,
    ctx: &mut Ctx,
    cp: &mut CpBuilder,
) -> Result<bool, CompileError> {
    let mut pre: Vec<&Stmt> = Vec::new();
    let mut post: Vec<&Stmt> = Vec::new();
    match body {
        Stmt::Block(items) => {
            let mut seen = false;
            for it in items {
                if !seen && is_super_call_stmt(it) {
                    pre.push(it);
                    seen = true;
                    continue;
                }
                let _ = seen;
                post.push(it);
            }
        }
        other => post.push(other),
    }
    // 前置部分（显式 super(...)）
    for s in pre {
        if compile_stmt(s, em, ctx, cp)? {
            return Ok(true);
        }
    }
    // 字段初始化
    for (name, desc, init) in inits {
        let sp = if needs_scratch(init) { Some(spill(init, em, ctx, cp)?) } else { None };
        em.op(0x2A); // aload_0
        em.push(1);
        emit_val(init, &sp, em, ctx, cp)?;
        coerce_to(&type_from_desc(desc), init, ctx, em, cp)?;
        let f = cp.field_ref(&ctx.class_name, name, desc);
        em.op_u2(0xB5, f); // putfield
        em.pop(if kind_wide(kind_from_desc(desc)) { 3 } else { 2 });
    }
    // 其余语句
    for s in post {
        if compile_stmt(s, em, ctx, cp)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 判断构造器体里 `super(...)` / `this(...)` 的**位置**
fn ctor_call_position(body: Option<&Stmt>) -> CtorCallPos {
    let mut pos = CtorCallPos { first_is_ctor_call: false, not_first: false };
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
            if is_ctor_call_stmt(s) {
                pos.first_is_ctor_call = true;
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

/// 这条语句是不是 `super(...)` / `this(...)`（构造器委托）
fn is_ctor_call_stmt(s: &Stmt) -> bool {
    is_super_call_stmt(s)
        || matches!(s, Stmt::Expr(Expr::Call { recv: None, name, .. }) if name == "this")
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
