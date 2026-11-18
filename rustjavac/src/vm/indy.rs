//! `invokedynamic`（0xba）、`CONSTANT_Dynamic`（`ldc` 的 tag 17）与 `java.lang.invoke`。
//!
//! # 这一层解决什么
//!
//! javac 从 Java 9 起把三件日常的事编译成 `invokedynamic`：
//!
//! | 源码 | 引导方法 |
//! | --- | --- |
//! | `"a" + b` | `StringConcatFactory.makeConcatWithConstants` |
//! | `() -> x` / `Foo::bar` | `LambdaMetafactory.metafactory` / `altMetafactory` |
//! | `case null, ...` 之类常量 | `ConstantBootstraps.*`（`ldc` 的 `CONSTANT_Dynamic`） |
//!
//! 不实现它们，任何现代 `.class` 一碰字符串拼接就死。
//!
//! # 三层结构
//!
//! 1. **常量与引导方法**（本文件的「常量重建」「调用点解析」两节）：
//!    `BootstrapMethods` 属性 → 引导方法句柄 + 静态参数 → 调一次 → 拿到 `CallSite`。
//!    结果按 `(类下标, 常量池索引)` 缓存，**一个调用点只解析一次**。
//! 2. **方法句柄执行**（「方法句柄」一节）：`MethodHandle.invoke/invokeExact` 的真实语义 ——
//!    按调用点描述符做 asType 适配（拆装箱、宽化/窄化、返回值转换），再分派到
//!    静态/虚/特殊/构造器/字段访问。
//! 3. **`java.lang.invoke` 的字节码级调用**（「java.lang.invoke 调用拦截」一节）：
//!    `MethodHandles.lookup().findStatic(...)`、`CallSite.getTarget()` 这些在
//!    OpenJDK 里是靠 VM 内部原语（`invokeBasic`/`linkToVirtual` …）实现的，
//!    任何 VM 都不可能靠执行它们的字节码跑通，所以按 `(类, 方法名)` 在本层接管。
//!
//! # 为什么引导方法走「快路径 + 通用路径」
//!
//! `StringConcatFactory` / `LambdaMetafactory` / `ConstantBootstraps` 的 Java 实现本身
//! 就要构造方法句柄、定义隐藏类（`Lookup.defineHiddenClass`），依赖 VM 全套内部原语。
//! 对这三个类我们**直接在 Rust 里算出等价的 CallSite**（与 HotSpot 的执行结果一致，
//! 规范里也没规定必须执行它们的 Java 代码）；**其它任何引导方法**（用户自己写的）
//! 都走通用路径：真的用解释器执行那个 Java 方法，再取它返回的 `CallSite.getTarget()`。

use super::shared::lk;
use crate::jdk;
use crate::CpEntry;
use std::sync::Arc;

use super::err;
use super::heap::RefKind;
use super::slot::Slot;
use super::Vm;
use super::VmError;

// ---------------------------------------------------------------- 描述符工具

/// 是不是基本类型
pub(super) fn is_prim(t: &str) -> bool {
    matches!(t, "I" | "J" | "F" | "D" | "Z" | "B" | "C" | "S")
}

/// 从 `i` 开始的一个类型描述符的结束位置（不含）
fn type_end(b: &[u8], mut i: usize) -> usize {
    if i >= b.len() {
        return i;
    }
    if b[i] == b'[' {
        while i < b.len() && b[i] == b'[' {
            i += 1;
        }
    }
    if i < b.len() && b[i] == b'L' {
        while i < b.len() && b[i] != b';' {
            i += 1;
        }
        (i + 1).min(b.len())
    } else {
        (i + 1).min(b.len())
    }
}

/// 方法描述符里的参数类型（按顺序，不含 `this`）
pub(super) fn param_descs(desc: &str) -> Vec<String> {
    let b = desc.as_bytes();
    let mut i = 1usize;
    let mut out = Vec::new();
    while i < b.len() && b[i] != b')' {
        let end = type_end(b, i);
        out.push(String::from_utf8_lossy(&b[i..end]).into_owned());
        i = end;
    }
    out
}

/// 方法描述符的返回类型（`V` 表示 void）
pub(super) fn ret_desc(desc: &str) -> String {
    let b = desc.as_bytes();
    match b.iter().position(|c| *c == b')') {
        Some(close) => {
            let end = type_end(b, close + 1);
            if end <= close + 1 {
                "V".into()
            } else {
                String::from_utf8_lossy(&b[close + 1..end]).into_owned()
            }
        }
        None => "V".into(),
    }
}

/// 把 `prefix`（一串参数类型，形如 `ILjava/lang/String;`）拼到一个完整描述符前面
fn prefix_desc(prefix: &str, desc: &str) -> String {
    match desc.find(')') {
        Some(_) => format!("({}{}", prefix, &desc[1..]),
        None => desc.to_string(),
    }
}

// ---------------------------------------------------------------- 数据结构

/// 一个常量池静态参数 / `ldc` 常量的重建结果：`(类型描述符, 值)`
pub(super) type TypedSlot = (String, Slot);

/// 方法句柄的实现。句柄对象本身是普通 VM 对象（`java.lang.invoke.MethodHandle`），
/// 真正的语义在这里 —— 这样 `getClass()`/`instanceof` 照常工作，句柄状态又不用
/// 硬塞进对象字段。
#[derive(Clone)]
pub(super) enum MhKind {
    /// 直接句柄：`ref_kind` 是 JVM 的引用种类
    /// （1 getField / 2 getStatic / 3 putField / 4 putStatic / 5 invokeVirtual /
    ///  6 invokeStatic / 7 invokeSpecial / 8 newInvokeSpecial / 9 invokeInterface）
    Direct { ref_kind: u8, class: String, name: String, desc: String },
    /// 绑定了前导参数（`bindTo` / `insertArguments` / lambda 捕获值）
    Bound { target: i32, pos: usize, bindings: Vec<Slot> },
    /// 常量句柄（`MethodHandles.constant` / `ConstantBootstraps.nullConstant`）
    Constant(Slot),
    /// 恒等句柄（`MethodHandles.identity`）
    Identity,
    /// 只换了类型的句柄（`asType`）：执行时直接转发到 `target`
    Adapter { target: i32 },
    /// 字符串拼接调用点的目标
    Concat { chunks: Arc<Vec<Chunk>>, consts: Arc<Vec<TypedSlot>> },
    /// 函数式接口实例的工厂（`LambdaMetafactory` 的结果）
    LambdaCtor(Arc<LambdaTemplate>),
}

#[derive(Clone)]
pub(super) struct MhInfo {
    pub kind: MhKind,
    /// 句柄自身的类型，形如 `(I)Ljava/lang/String;`
    pub mtype: String,
}

/// 拼接配方里的一段
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Chunk {
    /// 原样输出的字面量
    Lit(String),
    /// 第 i 个动态参数（附带它在调用点描述符里的类型）
    Arg(usize, String),
    /// 第 i 个引导方法静态常量（附带它的类型）
    Const(usize, String),
}

/// lambda 工厂模板：`invokedynamic` 解析时确定，每次执行只是拿它造一个实例
#[derive(Clone)]
pub(super) struct LambdaTemplate {
    /// 实现方法句柄（已把捕获值之外的类型适配好）
    pub target: i32,
    /// 生成的 lambda 对象用的类名
    pub class_name: String,
    /// 实现的接口（第一个是目标函数式接口，其余是 `altMetafactory` 的 marker）
    pub ifaces: Vec<String>,
    /// 可以按这些 `(方法名, 描述符)` 调用（SAM + 桥接 + 实例化泛型形态）
    pub sams: Vec<(String, String)>,
    /// 工厂自身的类型：`(捕获...)L接口;`
    pub factory_type: String,
}

#[derive(Clone)]
pub(super) struct LambdaInfo {
    pub target: i32,
    /// 捕获值（原始槽，类型见 `captured_desc`）
    pub captured: Vec<Slot>,
    /// 捕获参数的类型串（形如 `ILjava/lang/String;`），拼描述符用
    pub captured_desc: String,
    pub ifaces: Vec<String>,
    pub sams: Vec<(String, String)>,
    pub class_name: String,
}

#[derive(Clone)]
pub(super) struct CallSiteInfo {
    pub target: i32,
    pub mutable: bool,
}

pub(super) struct VarHandleInfo {
    /// 声明字段的类
    pub class: String,
    pub name: String,
    /// 字段类型
    pub desc: String,
    pub is_static: bool,
    /// 这是不是数组形态（`arrayElementVarHandle` / `byteArrayViewVarHandle` 造出来的）
    pub kind: VhKind,
}

/// VarHandle 的三种形态。字段形态用上面那四个字段描述；数组形态不用。
#[derive(Clone, PartialEq)]
pub(super) enum VhKind {
    /// 字段（`findVarHandle` / `staticFieldVarHandle` 那一族）
    Field,
    /// `MethodHandles.arrayElementVarHandle(T[].class)`：坐标是 `(数组, 下标)`
    ArrayElem {
        /// 元素类型描述符，如 `I` / `Ljava/lang/String;`
        elem: String,
    },
    /// `MethodHandles.byteArrayViewVarHandle(int[].class, order)`：
    /// 坐标是 `(byte[], 字节下标)`，按 `view` 宽度一次性读写，注意字节序
    ByteView {
        /// 视图类型描述符（`S`/`C`/`I`/`J`/`F`/`D`）
        view: String,
        big_endian: bool,
    },
}


// ---------------------------------------------------------------- 描述符级适配

impl Vm {
    /// 一处 Java 异常（返回 `VmError`，交给调用方的异常表）
    pub(super) fn java_exception(&mut self, class: &str, msg: impl Into<String>) -> VmError {
        let msg = msg.into();
        let n = self.store.field_count(class).max(1);
        let h = self.sh.heap.new_object(vec![Slot::I(0); n], &class.to_string()).unwrap_or(0);
        // 未捕获时 `Thread-0` 那行下面要打的栈：当场抓（此刻还没开始展开）
        super::exception::note_frames(h, super::stacktrace::frames_current());
        let m = self.str_new(msg.clone());
        lk(&self.sh.exc_msgs).insert(h, m);
        // `getMessage()` 读的是 `Throwable.detailMessage` 字段 —— 只记在 `exc_msgs` 里的话，
        // Java 代码 catch 到之后 `e.getMessage()` 会拿到 null（与 HotSpot 不一致）
        self.put_named(h, class, "detailMessage", Slot::I(m));
        VmError { msg: format!("{}: {msg}", class.replace('/', ".")), exc: Some(h) }
    }

    /// `BootstrapMethodError`：引导方法相关的一切失败都走它（规范要求）
    fn bsm_error(&mut self, msg: impl Into<String>) -> VmError {
        self.java_exception("java/lang/BootstrapMethodError", msg)
    }

    fn wrong_type(&mut self, msg: impl Into<String>) -> VmError {
        self.java_exception("java/lang/invoke/WrongMethodTypeException", msg)
    }

    /// 新建一个 VM 自己管理的对象（`java.lang.invoke.*` 的实例都这么来）
    pub(super) fn alloc_vmobj(&mut self, class: &str) -> i32 {
        let n = self.store.field_count(class).max(1);
        self.sh.heap.new_object(vec![Slot::I(0); n], &class.to_string()).unwrap_or(0)
    }

    // ---------- MethodType ----------

    pub(super) fn mtype_obj(&mut self, desc: &str) -> i32 {
        let h = self.alloc_vmobj("java/lang/invoke/MethodType");
        self.indy().mtypes.insert(h, desc.to_string());
        h
    }

    pub(super) fn mtype_of(&self, h: i32) -> Option<String> {
        self.indy().mtypes.get(&h).cloned()
    }

    // ---------- MethodHandle ----------

    pub(super) fn mh_obj(&mut self, kind: MhKind, mtype: String) -> i32 {
        let h = self.alloc_vmobj("java/lang/invoke/MethodHandle");
        self.indy().mhs.insert(h, MhInfo { kind, mtype });
        h
    }

    fn mh_info(&self, h: i32) -> Option<MhInfo> {
        self.indy().mhs.get(&h).cloned()
    }

    /// 某个句柄是不是我们管的对象（错误信息里区分「不是句柄」和「句柄没有方法」）
    fn is_mh(&self, h: i32) -> bool {
        self.indy().mhs.contains_key(&h)
    }

    // ---------- 常量重建 ----------

    /// 常量池的一项 → `(类型, 值)`。`ldc` 与引导方法静态参数共用这一处。
    ///
    /// 覆盖：数值 / String / Class / **MethodType** / **MethodHandle** / **Dynamic**。
    pub(super) fn reify_const(&mut self, class: usize, index: u16) -> Result<TypedSlot, VmError> {
        let cf = self.store.classes[class].clone();
        let cname = cf.this_class_name().unwrap_or_default();
        let e = cf
            .constant_pool
            .get(index as usize)
            .and_then(|e| e.as_ref())
            .ok_or_else(|| VmError {
                msg: format!("{cname} 的常量池索引 {index} 为空"),
                exc: None,
            })?;
        Ok(match e {
            CpEntry::Integer(v) => ("I".into(), Slot::I(*v as i32)),
            CpEntry::Float(bits) => ("F".into(), Slot::Fl(f32::from_bits(*bits))),
            CpEntry::Long(v) => ("J".into(), Slot::L(*v as i64)),
            CpEntry::Double(bits) => ("D".into(), Slot::F(f64::from_bits(*bits))),
            CpEntry::String(u) => {
                let text = String::from_utf8_lossy(cf.utf8(*u).unwrap_or_default()).into_owned();
                let h = self.str_new(text);
                ("Ljava/lang/String;".into(), Slot::I(h))
            }
            CpEntry::Class(ix) => {
                let name = String::from_utf8_lossy(cf.utf8(*ix).unwrap_or_default()).into_owned();
                let h = self.class_obj(&name);
                ("Ljava/lang/Class;".into(), Slot::I(h))
            }
            CpEntry::MethodType(ix) => {
                let desc = String::from_utf8_lossy(cf.utf8(*ix).unwrap_or_default()).into_owned();
                let h = self.mtype_obj(&desc);
                ("Ljava/lang/invoke/MethodType;".into(), Slot::I(h))
            }
            CpEntry::MethodHandle(kind, refi) => {
                let kind = *kind;
                let (c, n, d) = self.handle_target(&cf, *refi)?;
                let ty = handle_type(kind, &c, &d);
                let h = self.mh_obj(MhKind::Direct { ref_kind: kind, class: c, name: n, desc: d }, ty);
                ("Ljava/lang/invoke/MethodHandle;".into(), Slot::I(h))
            }
            CpEntry::Dynamic(bsm, nat) => {
                let v = self.condy_value(class, *bsm, *nat)?;
                v
            }
            other => {
                return err(format!("{cname} 常量池 #{index} 不能当常量用: {other:?}"));
            }
        })
    }

    /// `MethodRef` / `InterfaceMethodRef` / `FieldRef` → `(类, 名, 描述符)`
    fn handle_target(
        &self,
        cf: &crate::ClassFile,
        index: u16,
    ) -> Result<(String, String, String), VmError> {
        let info = self.method_ref(cf, index)?;
        Ok((info.class, info.name, info.desc))
    }

    /// `CONSTANT_Dynamic`：跑一次引导方法拿值，然后缓存（同一个常量只算一次）
    fn condy_value(&mut self, class: usize, bsm: u16, nat: u16) -> Result<TypedSlot, VmError> {
        let key = (class, nat);
        if let Some(v) = self.indy().condy_values.get(&key) {
            return Ok(v.clone());
        }
        let cf = self.store.classes[class].clone();
        let (name, desc) = nat_of(&cf, nat)?;
        let v = self.run_bootstrap(class, bsm, &name, &desc)?;
        let want = ret_desc(&format!("({desc})V"));
        // 引导方法给的是**值本身**：按常量声明的类型适配一下（拆装箱）
        let got = ("Ljava/lang/Object;".to_string(), v);
        let v = self.coerce(got.1, &got.0, &want, "CONSTANT_Dynamic")?;
        let out = (want, v);
        self.indy().condy_values.insert(key, out.clone());
        Ok(out)
    }

    // ---------- 引导方法表 ----------

    fn bsm_table(&mut self, class: usize) -> Result<Arc<Vec<crate::classfile::attr::BootstrapMethod>>, VmError> {
        let cf = self.store.classes[class].clone();
        let name = cf.this_class_name().unwrap_or_default();
        if let Some(t) = self.indy().bsm_tables.get(&name) {
            return Ok(t.clone());
        }
        let table = Arc::new(crate::classfile::attr::bootstrap_methods(&cf).map_err(|e| VmError {
            msg: format!("{name} 的 BootstrapMethods 属性解析失败: {e}"),
            exc: None,
        })?);
        self.indy().bsm_tables.insert(name, table.clone());
        Ok(table)
    }

    /// 引导方法表 + 静态参数的**类型**（`(类, 名, 描述符)` + 参数）
    fn bsm_entry(
        &mut self,
        class: usize,
        bsm_index: u16,
    ) -> Result<(u8, String, String, String, Vec<u16>), VmError> {
        let table = self.bsm_table(class)?;
        let e = table.get(bsm_index as usize).ok_or_else(|| VmError {
            msg: format!(
                "BootstrapMethods 索引 {bsm_index} 越界（共 {} 项）",
                table.len()
            ),
            exc: None,
        })?;
        let cf = self.store.classes[class].clone();
        let cname = cf.this_class_name().unwrap_or_default();
        let (kind, refi) = match cf.constant_pool.get(e.method_ref as usize).and_then(|x| x.as_ref()) {
            Some(CpEntry::MethodHandle(k, i)) => (*k, *i),
            other => {
                return err(format!(
                    "{cname} 的 BootstrapMethods[{bsm_index}] 的 method_ref 不是 MethodHandle: {other:?}"
                ))
            }
        };
        let (c, n, d) = self.handle_target(&cf, refi)?;
        Ok((kind, c, n, d, e.args.clone()))
    }

    // ---------- invokedynamic / Dynamic 的调用点解析 ----------

    /// 调用点的 `(名字, 描述符, 引导方法索引)`（`invokedynamic` 与 `Dynamic` 共用）
    fn site_info(&self, class: usize, index: u16) -> Result<(String, String, u16), VmError> {
        let cf = &self.store.classes[class];
        let cname = cf.this_class_name().unwrap_or_default();
        match cf.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
            Some(CpEntry::InvokeDynamic(bsm, nat)) | Some(CpEntry::Dynamic(bsm, nat)) => {
                let (n, d) = nat_of(cf, *nat)?;
                Ok((n, d, *bsm))
            }
            other => err(format!(
                "{cname} 常量池 #{index} 不是 invokedynamic/Dynamic: {other:?}"
            )),
        }
    }

    /// `invokedynamic` 的调用点目标句柄：**每个调用点只解析一次**，之后直接复用
    pub(super) fn indy_site_target(&mut self, class: usize, index: u16) -> Result<i32, VmError> {
        if let Some(t) = self.indy().indy_sites.get(&(class, index)) {
            return Ok(*t);
        }
        let (name, desc, bsm) = self.site_info(class, index)?;
        let site = self.run_bootstrap(class, bsm, &name, &desc)?.as_i32();
        let target = self.callsite_target(site, &name, &desc)?;
        self.indy().indy_sites.insert((class, index), target);
        self.indy().indy_resolved += 1;
        Ok(target)
    }

    /// 调用点的描述符（`exec` 需要它来决定弹出多少个参数）
    pub(super) fn indy_desc(&self, class: usize, index: u16) -> Result<String, VmError> {
        Ok(self.site_info(class, index)?.1)
    }

    /// `CallSite.getTarget()`：我们的 side table 优先，其次真字节码构造的实例的 `target` 字段
    fn callsite_target(&mut self, site: i32, name: &str, desc: &str) -> Result<i32, VmError> {
        let site_target = self.indy().callsites.get(&site).map(|i| i.target);
        if let Some(t) = site_target {
            if self.is_mh(t) {
                return Ok(t);
            }
            return Err(self.bsm_error(format!(
                "调用点 {name}{desc} 的 getTarget() 不是方法句柄（句柄 {t}）"
            )));
        }
        // 用户自己 new 出来的 CallSite（例如 ConstantCallSite）：读它的 target 字段
        let cls = match self.ref_kind(site) {
            RefKind::Obj(c) => c,
            k => {
                let kind = match k {
                    RefKind::Null => "null",
                    RefKind::Str => "java.lang.String",
                    RefKind::Arr(_) => "数组",
                    RefKind::Obj(_) => unreachable!(),
                };
                return Err(self.bsm_error(format!(
                    "调用点 {name}{desc} 的引导方法返回了 {kind}，不是 java.lang.invoke.CallSite"
                )));
            }
        };
        if let Some(i) = self.store.field_index(&cls, "target") {
            if let Some(slots) = self.sh.heap.snapshot(site) {
                let t = slots.get(i).map(|s| s.as_i32()).unwrap_or(0);
                if self.is_mh(t) {
                    return Ok(t);
                }
            }
        }
        Err(self.bsm_error(format!(
            "调用点 {name}{desc}：{}.getTarget() 拿不到方法句柄（该类不是 VM 认识的 CallSite）",
            cls.replace('/', ".")
        )))
    }

    /// 跑引导方法，返回它的结果（`invokedynamic` 是 `CallSite`，`Dynamic` 是值本身）
    fn run_bootstrap(
        &mut self,
        caller: usize,
        bsm_index: u16,
        site_name: &str,
        site_desc: &str,
    ) -> Result<Slot, VmError> {
        let (kind, bsm_class, bsm_name, bsm_desc, arg_idx) = self.bsm_entry(caller, bsm_index)?;
        let caller_name = self.store.classes[caller].this_class_name().unwrap_or_default();
        if kind != 6 {
            return Err(self.bsm_error(format!(
                "引导方法 {}.{}{} 的引用种类是 {kind}（规范要求 REF_invokeStatic=6），调用点 {}.{}",
                bsm_class, bsm_name, bsm_desc, caller_name, site_name
            )));
        }
        // 静态参数：解析成 (类型, 值)，快路径与通用路径都要
        let mut typed: Vec<TypedSlot> = Vec::with_capacity(arg_idx.len());
        for i in &arg_idx {
            typed.push(self.reify_const(caller, *i)?);
        }
        // ---- 快路径：javac 用到的三个工厂 ----
        let quick = match (bsm_class.as_str(), bsm_name.as_str()) {
            ("java/lang/invoke/StringConcatFactory", "makeConcatWithConstants") => {
                Some(self.bsm_concat(site_desc, &typed, true)?)
            }
            ("java/lang/invoke/StringConcatFactory", "makeConcat") => {
                Some(self.bsm_concat(site_desc, &typed, false)?)
            }
            ("java/lang/invoke/LambdaMetafactory", "metafactory") => {
                Some(self.bsm_lambda(&caller_name, site_name, site_desc, &typed, false)?)
            }
            ("java/lang/invoke/LambdaMetafactory", "altMetafactory") => {
                Some(self.bsm_lambda(&caller_name, site_name, site_desc, &typed, true)?)
            }
            ("java/lang/invoke/ConstantBootstraps", _) => Some(self.bsm_constant(
                &caller_name, &bsm_name, site_name, site_desc, &typed,
            )?),
            _ => None,
        };
        if let Some(v) = quick {
            return Ok(v);
        }
        // ---- 通用路径：真的执行那个 Java 引导方法 ----
        let params = param_descs(&bsm_desc);
        let mut call_args: Vec<Slot> = Vec::with_capacity(params.len());
        let lookup = self.alloc_vmobj("java/lang/invoke/MethodHandles$Lookup");
        self.indy().lookups.insert(lookup, caller_name.clone());
        call_args.push(Slot::I(lookup));
        let n = self.str_new(site_name.to_string());
        call_args.push(Slot::I(n));
        let mt = self.mtype_obj(site_desc);
        call_args.push(Slot::I(mt));
        // 静态参数：参数表末尾是数组（`Object...`）时打包成一个数组
        let varargs = params.len() == typed.len() + 1
            && params.last().map(|p| p.starts_with('[')).unwrap_or(false);
        if varargs {
            let elem = params.last().cloned().unwrap_or_else(|| "[Ljava/lang/Object;".into());
            let mut items = Vec::with_capacity(typed.len());
            for (ty, v) in &typed {
                items.push(self.coerce(*v, ty, "Ljava/lang/Object;", "引导方法静态参数")?);
            }
            let arr = self.new_ref_array(&elem, items);
            call_args.push(Slot::I(arr));
        } else {
            for (i, (ty, v)) in typed.iter().enumerate() {
                let want = params.get(3 + i).cloned().unwrap_or_else(|| ty.clone());
                call_args.push(self.coerce(*v, ty, &want, "引导方法静态参数")?);
            }
        }
        let Some((ci, mi)) = self.store.resolve_method(&bsm_class, &bsm_name, &bsm_desc) else {
            return Err(self.bsm_error(format!(
                "引导方法 {}.{}{} 没有字节码：调用点 {}.{} 需要一个 VM 已注册的工厂 \
                 （独立模式只能跑 StringConcatFactory / LambdaMetafactory / ConstantBootstraps，\
                 或挂上 VMLESS_JDK 让用户自己的引导方法可执行）",
                bsm_class, bsm_name, bsm_desc, caller_name, site_name
            )));
        };
        self.ensure_init(&bsm_class)?;
        self.depth += 1;
        let r = self.execute(ci, mi, &bsm_desc, &call_args, false);
        self.depth -= 1;
        match r {
            Ok(v) => Ok(v),
            Err(e) if e.exc.is_some() => {
                let text = self.exc_text(e.exc.unwrap());
                Err(self.bsm_error(format!(
                    "引导方法 {}.{}{} 抛出 {text}（调用点 {}.{}）",
                    bsm_class, bsm_name, bsm_desc, caller_name, site_name
                )))
            }
            Err(e) => Err(self.bsm_error(format!(
                "引导方法 {}.{}{} 执行失败: {}（调用点 {}.{}）",
                bsm_class, bsm_name, bsm_desc, e.msg, caller_name, site_name
            ))),
        }
    }

    /// 新建一个引用类型数组（元素按 `elem_desc` 的类）
    fn new_ref_array(&mut self, elem_desc: &str, items: Vec<Slot>) -> i32 {
        self.sh.heap.new_array(items, elem_desc).unwrap_or(0)
    }

    // ---------- 方法句柄：类型适配 ----------

    /// 把一个值从类型 `from` 适配到 `to`（`MethodHandle.asType` 的语义）。
    ///
    /// 覆盖：基本类型之间的宽化/窄化、装箱、拆箱；引用之间直接放行（JVM 侧靠 checkcast，
    /// 这个 VM 不做强制转换）。
    pub(super) fn coerce(
        &mut self,
        v: Slot,
        from: &str,
        to: &str,
        ctx: &str,
    ) -> Result<Slot, VmError> {
        if from == to {
            return Ok(v);
        }
        let (fp, tp) = (is_prim(from), is_prim(to));
        match (fp, tp) {
            (true, true) => Ok(prim_convert(v, from, to)),
            (true, false) => {
                // 装箱：目标是具体包装类就装成它，否则按自然包装类
                let cls = if jdk::wrapper_prim(class_of_desc(to)).is_some() {
                    class_of_desc(to).to_string()
                } else {
                    jdk::box_class(from).map(|s| s.to_string()).unwrap_or_default()
                };
                if cls.is_empty() {
                    return Err(self.wrong_type(format!("{ctx}: 不能把 {from} 装成 {to}")));
                }
                let h = self.new_wrapper(&cls, prim_convert(v, from, wrapper_prim_of(&cls)));
                Ok(Slot::I(h))
            }
            (false, true) => {
                // 拆箱：先按包装类的值取，再转成目标基本类型
                let h = v.as_i32();
                if h == 0 {
                    return Err(self.java_exception(
                        "java/lang/NullPointerException",
                        format!("{ctx}: 要拆箱的值是 null（目标类型 {to}）"),
                    ));
                }
                match self.wrapper_value(h) {
                    Some(w) => {
                        let cls = match self.ref_kind(h) {
                            RefKind::Obj(c) => c,
                            _ => String::new(),
                        };
                        let src = jdk::wrapper_prim(&cls).unwrap_or(to);
                        Ok(prim_convert(w, src, to))
                    }
                    None => Err(self.wrong_type(format!(
                        "{ctx}: {} 不能拆成 {to}",
                        self.type_name(h)
                    ))),
                }
            }
            (false, false) => Ok(v),
        }
    }

    /// 按调用点描述符 `want` 执行一个方法句柄（`invoke` / lambda SAM 调用都走这里）
    pub(super) fn invoke_mh_as(
        &mut self,
        mh: i32,
        want: &str,
        args: &[Slot],
        ctx: &str,
    ) -> Result<Slot, VmError> {
        let Some(info) = self.mh_info(mh) else {
            return Err(self.wrong_type(format!("{ctx}: 句柄 {mh} 不是方法句柄")));
        };
        let have = param_descs(&info.mtype);
        let need = param_descs(want);
        if have.len() != need.len() || args.len() != need.len() {
            return Err(self.wrong_type(format!(
                "{ctx}: 类型不匹配 —— 句柄是 {}（{} 个参数），调用点是 {want}（{} 个参数，实参 {} 个）",
                info.mtype,
                have.len(),
                need.len(),
                args.len()
            )));
        }
        let mut conv = Vec::with_capacity(args.len());
        for (i, a) in args.iter().enumerate() {
            conv.push(self.coerce(*a, &need[i], &have[i], ctx)?);
        }
        let r = self.dispatch_mh(mh, &conv, ctx)?;
        self.coerce_ret(r, &ret_desc(&info.mtype), &ret_desc(want), ctx)
    }

    fn coerce_ret(
        &mut self,
        v: Slot,
        have: &str,
        want: &str,
        ctx: &str,
    ) -> Result<Slot, VmError> {
        if want == "V" {
            return Ok(Slot::I(0));
        }
        if have == "V" {
            return Ok(v);
        }
        self.coerce(v, have, want, ctx)
    }

    /// 执行一个方法句柄（参数已按它的 mtype 适配好）
    fn dispatch_mh(&mut self, mh: i32, args: &[Slot], ctx: &str) -> Result<Slot, VmError> {
        let Some(info) = self.mh_info(mh) else {
            return Err(self.wrong_type(format!("{ctx}: 句柄 {mh} 不是方法句柄")));
        };
        match info.kind {
            MhKind::Direct { ref_kind, class, name, desc } => {
                self.call_direct(ref_kind, &class, &name, &desc, args, ctx)
            }
            MhKind::Bound { target, pos, bindings } => {
                // 把绑定值插回 pos（`insertArguments` 允许插在中间）
                let mut all = Vec::with_capacity(bindings.len() + args.len());
                all.extend_from_slice(&args[..pos.min(args.len())]);
                all.extend_from_slice(&bindings);
                if pos < args.len() {
                    all.extend_from_slice(&args[pos..]);
                }
                self.dispatch_mh(target, &all, ctx)
            }
            MhKind::Constant(v) => Ok(v),
            MhKind::Identity => Ok(args.first().copied().unwrap_or(Slot::I(0))),
            MhKind::Adapter { target } => {
                // 只换了类型：按适配器自己的类型把实参/返回值转换过去
                // （`invoke_mh_as` 的目标类型就是 target 的真实 mtype）
                let want = info.mtype.clone();
                self.invoke_mh_as(target, &want, args, ctx)
            }
            MhKind::Concat { chunks, consts } => {
                let h = self.concat_eval(&chunks, &consts, args)?;
                Ok(Slot::I(h))
            }
            MhKind::LambdaCtor(t) => {
                let h = self.alloc_vmobj(&t.class_name);
                self.indy().lambdas.insert(
                    h,
                    LambdaInfo {
                        target: t.target,
                        captured: args.to_vec(),
                        captured_desc: param_descs(&t.factory_type).concat(),
                        ifaces: t.ifaces.clone(),
                        sams: t.sams.clone(),
                        class_name: t.class_name.clone(),
                    },
                );
                Ok(Slot::I(h))
            }
        }
    }

    /// 直接句柄的执行：静态 / 虚 / 特殊 / 构造器 / 字段
    fn call_direct(
        &mut self,
        ref_kind: u8,
        class: &str,
        name: &str,
        desc: &str,
        args: &[Slot],
        ctx: &str,
    ) -> Result<Slot, VmError> {
        match ref_kind {
            // ---- invokeStatic ----
            6 => {
                self.ensure_init(class)?;
                if let Some((ci, mi)) = self.store.resolve_method(class, name, desc) {
                    self.depth += 1;
                    let r = self.execute(ci, mi, desc, args, false);
                    self.depth -= 1;
                    return r;
                }
                match self.builtin_static(class, name, desc, args) {
                    Some(v) => v,
                    None => {
                        let _ = ctx;
                        err(format!(
                            "方法句柄目标 {}.{}{} 找不到（类没有字节码，也不是内建静态方法）",
                            class, name, desc
                        ))
                    }
                }
            }
            // ---- newInvokeSpecial：分配 + 跑构造器 ----
            8 => self.new_instance(class, desc).map(Slot::I),
            // ---- invokeSpecial ----
            7 => {
                let Some((ci, mi)) = self.store.resolve_method(class, name, desc) else {
                    return err(format!("方法句柄目标 {}.{}{} 找不到", class, name, desc));
                };
                self.depth += 1;
                let r = self.execute(ci, mi, desc, args, true);
                self.depth -= 1;
                r
            }
            // ---- invokeVirtual / invokeInterface ----
            5 | 9 => {
                let recv = args.first().copied().unwrap_or(Slot::I(0)).as_i32();
                let rest = if args.is_empty() { &args[..] } else { &args[1..] };
                // `System.out::println`：`System.out` 在这个 VM 里是占位（句柄 0），
                // 输出走 println 的内建文本格式化
                if name == "println" && class == "java/io/PrintStream" {
                    let v = rest.first().copied().unwrap_or(Slot::I(0));
                    let text = self.println_text(desc, v);
                    self.println(text);
                    return Ok(Slot::I(0));
                }
                let actual = match self.ref_kind(recv) {
                    RefKind::Obj(c) => Some(c),
                    // 与 `exec` 的虚分派保持一致：字符串句柄的运行时类是 String，
                    // 数组的方法都来自 Object
                    RefKind::Str => Some("java/lang/String".to_string()),
                    RefKind::Arr(_) => Some("java/lang/Object".to_string()),
                    RefKind::Null => None,
                };
                let target = actual
                    .as_deref()
                    .and_then(|c| self.store.resolve_method(c, name, desc))
                    .or_else(|| self.store.resolve_method(class, name, desc));
                if let Some((ci, mi)) = target {
                    self.depth += 1;
                    let r = self.execute(ci, mi, desc, args, true);
                    self.depth -= 1;
                    return r;
                }
                if let Some(r) = self.lambda_dispatch(recv, name, desc, rest) {
                    return r;
                }
                match self.builtin_virtual(class, name, desc, recv, rest) {
                    Some(v) => v,
                    None => err(format!(
                        "方法句柄目标 {}.{}{} 找不到（接收者是 {}）",
                        class,
                        name,
                        desc,
                        self.type_name(recv)
                    )),
                }
            }
            // ---- 字段句柄 ----
            1 => {
                // getField
                let obj = args.first().copied().unwrap_or(Slot::I(0)).as_i32();
                let f = self.field_offset(obj, class, name)?;
                Ok(if obj > 0 {
                    self.sh.heap.get(obj, f).unwrap_or(Slot::I(0))
                } else {
                    Slot::I(0)
                })
            }
            2 => {
                // getStatic
                if let Some(v) = self.builtin_static_field(class, name) {
                    return Ok(v);
                }
                self.ensure_init(class)?;
                Ok(lk(&self.sh.statics)
                    .get(&(class.to_string(), name.to_string()))
                    .copied()
                    .unwrap_or(Slot::I(0)))
            }
            3 => {
                // putField
                let obj = args.first().copied().unwrap_or(Slot::I(0)).as_i32();
                let v = args.get(1).copied().unwrap_or(Slot::I(0));
                let f = self.field_offset(obj, class, name)?;
                if obj > 0 {
                    self.sh.heap.set(obj, f, v);
                }
                Ok(Slot::I(0))
            }
            4 => {
                // putStatic
                let v = args.first().copied().unwrap_or(Slot::I(0));
                self.ensure_init(class)?;
                lk(&self.sh.statics).insert((class.to_string(), name.to_string()), v);
                Ok(Slot::I(0))
            }
            other => err(format!("未知的方法句柄引用种类 {other}")),
        }
    }

    /// 绑定前导参数（`bindTo` / `insertArguments`）。`from` 是绑定值的**来源类型**
    /// （`insertArguments` 从 `Object[]` 来，`bindTo` 是 `Object`），要按目标参数类型适配。
    fn make_bound(
        &mut self,
        mh: i32,
        pos: usize,
        vals: Vec<Slot>,
        from: &str,
        ctx: &str,
    ) -> Result<i32, VmError> {
        let Some(info) = self.mh_info(mh) else {
            return Err(self.wrong_type(format!("{ctx}: 句柄 {mh} 不是方法句柄")));
        };
        let params = param_descs(&info.mtype);
        if pos + vals.len() > params.len() {
            return Err(self.wrong_type(format!(
                "{ctx}: 位置 {pos} + {} 个绑定值超过了句柄参数个数 {}（{}）",
                vals.len(),
                params.len(),
                info.mtype
            )));
        }
        let mut bindings = Vec::with_capacity(vals.len());
        for (i, v) in vals.iter().enumerate() {
            bindings.push(self.coerce(*v, from, &params[pos + i], ctx)?);
        }
        let mut rest = params.clone();
        rest.drain(pos..pos + vals.len());
        let mtype = format!("({}){}", rest.concat(), ret_desc(&info.mtype));
        Ok(self.mh_obj(MhKind::Bound { target: mh, pos, bindings }, mtype))
    }

    /// VM 自己管的那些对象（MethodType / MethodHandle / lambda）的文本形态。
    ///
    /// 它们的 `toString()` 在 java.base 里会去读对象字段 —— 而状态在 side table 里，
    /// 真跑那段字节码只会得到垃圾，所以这里直接给答案（与 HotSpot 的输出一致）。
    fn vm_object_text(&self, h: i32) -> Option<String> {
        if let Some(d) = self.indy().mtypes.get(&h) {
            let ps: Vec<String> = param_descs(d).iter().map(|p| java_type_name(p)).collect();
            return Some(format!("({}){}", ps.join(","), java_type_name(&ret_desc(d))));
        }
        if let Some(i) = self.indy().mhs.get(&h) {
            return Some(format!("MethodHandle{}", i.mtype));
        }
        if let Some(l) = self.indy().lambdas.get(&h) {
            return Some(format!("{}$$Lambda/0x00000000", l.class_name));
        }
        None
    }

    /// 通用的「把一个值变成文本」：`String.valueOf` 的语义（会真的调 `toString()`）
    pub(super) fn value_text(&mut self, h: i32) -> Result<String, VmError> {
        if let Some(t) = self.vm_object_text(h) {
            return Ok(t);
        }
        match self.ref_kind(h) {
            RefKind::Null => Ok("null".into()),
            RefKind::Str => Ok(self.string_text(h)),
            RefKind::Arr(_) => Ok(self.obj_to_string(h)),
            RefKind::Obj(ref c) => {
                // 包装类 / String 的文本是确定的，直接按值格式化
                if jdk::wrapper_prim(c).is_some() {
                    return Ok(self.obj_to_string(h));
                }
                if let Some(name) = self.indy().lambdas.get(&h).map(|l| l.class_name.clone()) {
                    return Ok(format!("{}$$Lambda", name));
                }
                // 真的覆写了 toString 就执行它
                if let Some((ci, mi)) = self.store.resolve_method(c, "toString", "()Ljava/lang/String;")
                {
                    self.depth += 1;
                    let r = self.execute(ci, mi, "()Ljava/lang/String;", &[Slot::I(h)], true);
                    self.depth -= 1;
                    let v = r?;
                    return Ok(self.obj_to_string(v.as_i32()));
                }
                Ok(self.obj_to_string(h))
            }
        }
    }

    /// 按**静态类型** `ty` 格式化一个值（字符串拼接 / `String.valueOf` 用）
    fn fmt_value(&mut self, v: Slot, ty: &str) -> Result<String, VmError> {
        Ok(match ty {
            "I" => v.as_i32().to_string(),
            "B" => ((v.as_i32() as i8) as i32).to_string(),
            "S" => ((v.as_i32() as i16) as i32).to_string(),
            "C" => char::from_u32(v.as_i32() as u32).unwrap_or('\u{fffd}').to_string(),
            "Z" => (if v.as_i32() != 0 { "true" } else { "false" }).to_string(),
            "J" => v.as_i64().to_string(),
            "F" => crate::java_float_to_string(v.as_f32()),
            "D" => crate::java_double_to_string(v.as_f64()),
            "V" => String::new(),
            _ => self.value_text(v.as_i32())?,
        })
    }

    // ---------- StringConcatFactory ----------

    /// `makeConcatWithConstants` / `makeConcat` → `CallSite`（返回调用点对象）
    fn bsm_concat(
        &mut self,
        site_desc: &str,
        typed: &[TypedSlot],
        with_constants: bool,
    ) -> Result<Slot, VmError> {
        let params = param_descs(site_desc);
        let mut consts: Vec<TypedSlot> = Vec::new();
        let chunks: Vec<Chunk> = if with_constants {
            // 静态参数只有两类：第 1 个是配方，其余是 `\u0002` 占位符对应的常量。
            // （`Lookup` / 方法名 / `MethodType` 是引导方法的**前三个形参**，不是静态参数。）
            let recipe = match typed.first() {
                Some((t, v)) if t == "Ljava/lang/String;" => self.value_text(v.as_i32())?,
                Some((t, _)) => {
                    return Err(self.bsm_error(format!(
                        "makeConcatWithConstants 的第 1 个静态参数应该是 String 配方，实际是 {t}"
                    )))
                }
                None => {
                    return Err(self.bsm_error(
                        "makeConcatWithConstants 没有静态参数：至少要有拼接配方（String）",
                    ))
                }
            };
            consts = typed.get(1..).map(|s| s.to_vec()).unwrap_or_default();
            parse_recipe(&recipe, &params, &consts).map_err(|e| self.bsm_error(e))?
        } else {
            // makeConcat：所有参数依次拼接
            params
                .iter()
                .enumerate()
                .map(|(i, ty)| Chunk::Arg(i, ty.clone()))
                .collect()
        };
        let mh = self.mh_obj(
            MhKind::Concat { chunks: Arc::new(chunks), consts: Arc::new(consts) },
            site_desc.to_string(),
        );
        Ok(Slot::I(self.new_callsite(mh, "java/lang/invoke/ConstantCallSite", false)))
    }

    fn new_callsite(&mut self, target: i32, class: &str, mutable: bool) -> i32 {
        let h = self.alloc_vmobj(class);
        self.indy().callsites.insert(h, CallSiteInfo { target, mutable });
        // 真字节码路径也要能看到目标（用户可能调用 getTarget() 的字节码版本）
        if let Some(i) = self.store.field_index(class, "target") {
            self.sh.heap.set(h, i, Slot::I(target));
        }
        h
    }

    /// 拼接求值：按配方把字面量/动态参数/常量拼起来
    fn concat_eval(
        &mut self,
        chunks: &[Chunk],
        consts: &[TypedSlot],
        args: &[Slot],
    ) -> Result<i32, VmError> {
        let mut out = String::new();
        for c in chunks {
            match c {
                Chunk::Lit(t) => out.push_str(t),
                Chunk::Arg(i, ty) => {
                    let v = args.get(*i).copied().unwrap_or(Slot::I(0));
                    out.push_str(&self.fmt_value(v, ty)?);
                }
                Chunk::Const(i, ty) => {
                    let v = consts.get(*i).map(|c| c.1).unwrap_or(Slot::I(0));
                    out.push_str(&self.fmt_value(v, ty)?);
                }
            }
        }
        Ok(self.str_new(out))
    }

    // ---------- LambdaMetafactory ----------

    /// `metafactory` / `altMetafactory` → `CallSite`（工厂句柄）
    fn bsm_lambda(
        &mut self,
        caller: &str,
        sam_name: &str,
        factory_type: &str,
        typed: &[TypedSlot],
        alt: bool,
    ) -> Result<Slot, VmError> {
        // 静态参数只有三个：`samMethodType`、`implMethod`、`instantiatedMethodType`。
        // （`Lookup` / 接口方法名 / `MethodType` 是引导方法的**前三个形参**，由 JVM 在
        //  调用引导方法时填，不在静态参数表里 —— 调用点描述符本身就是那个 MethodType。）
        if typed.len() < 3 {
            return Err(self.bsm_error(format!(
                "{} 的静态参数只有 {} 个，至少要有 samMethodType / implMethod / instantiatedMethodType",
                if alt { "altMetafactory" } else { "metafactory" },
                typed.len()
            )));
        }
        let invoked_type = factory_type.to_string();
        let sam_type = self.mtype_arg(&typed[0], "samMethodType")?;
        let implmh = typed[1].1.as_i32();
        if !self.is_mh(implmh) {
            return Err(self.bsm_error("LambdaMetafactory 的 implMethod 不是方法句柄"));
        }
        let instantiated = self.mtype_arg(&typed[2], "instantiatedMethodType")?;
        let iface = ret_desc(&invoked_type);
        if !iface.starts_with('L') {
            return Err(self.bsm_error(format!(
                "LambdaMetafactory 的 invokedType {invoked_type} 返回的不是接口类型"
            )));
        }
        let iface = iface[1..iface.len() - 1].to_string();
        // 捕获值的类型 = invokedType 的参数
        let captured = param_descs(&invoked_type);
        let impl_params = param_descs(
            &self.mh_info(implmh).map(|i| i.mtype).unwrap_or_default(),
        );
        if impl_params.len() != captured.len() + param_descs(&sam_type).len() {
            return Err(self.bsm_error(format!(
                "LambdaMetafactory 类型不匹配：实现方法句柄是 {}（{} 个参数），\
                 捕获 {} 个 + SAM {sam_type}",
                self.mh_info(implmh).map(|i| i.mtype).unwrap_or_default(),
                impl_params.len(),
                captured.len()
            )));
        }
        // SAM 可用两种形态登记：擦除形态与实例化形态（调用点可能用任一个）
        let mut sams = vec![(sam_name.to_string(), sam_type.clone())];
        if instantiated != sam_type {
            sams.push((sam_name.to_string(), instantiated));
        }
        let mut ifaces = vec![iface.clone()];
        // altMetafactory：静态参数第 4 个是 flags，之后按 flags 决定 markers / bridges
        if alt {
            let mut i = 3usize;
            let flags = typed.get(i).map(|t| t.1.as_i32()).unwrap_or(0);
            i += 1;
            // FLAG_SERIALIZABLE = 1：HotSpot 生成的类会实现 `java.io.Serializable`
            // （`(Runnable & Serializable) () -> {}` 走的就是这一支，marker 列表是空的）
            if flags & 1 != 0 {
                ifaces.push("java/io/Serializable".to_string());
            }
            if flags & 2 != 0 {
                let n = typed.get(i).map(|t| t.1.as_i32()).unwrap_or(0).max(0) as usize;
                i += 1;
                for k in 0..n {
                    if let Some((_, v)) = typed.get(i + k) {
                        if let Some(name) = self.class_obj_name(v.as_i32()) {
                            ifaces.push(name);
                        }
                    }
                }
                i += n;
            }
            if flags & 4 != 0 {
                let n = typed.get(i).map(|t| t.1.as_i32()).unwrap_or(0).max(0) as usize;
                i += 1;
                for k in 0..n {
                    if let Some((ty, v)) = typed.get(i + k) {
                        if ty == "Ljava/lang/invoke/MethodType;" {
                            if let Some(d) = self.mtype_of(v.as_i32()) {
                                sams.push((sam_name.to_string(), d));
                            }
                        }
                    }
                }
            }
        }
        let seq = self.indy().lambda_seq;
        self.indy().lambda_seq += 1;
        let class_name = format!("{}$$Lambda${seq}", caller.replace('/', "_"));
        let tpl = LambdaTemplate {
            target: implmh,
            class_name,
            ifaces,
            sams,
            factory_type: invoked_type,
        };
        let mh = self.mh_obj(MhKind::LambdaCtor(Arc::new(tpl)), factory_type.to_string());
        Ok(Slot::I(self.new_callsite(mh, "java/lang/invoke/ConstantCallSite", false)))
    }

    /// 类对象 → 它的**类型描述符**。`Class` 里存的是内部名（`java/lang/String`），
    /// 而基本类型与数组存的已经是描述符形态（`I` / `[I`）—— 拼 MethodType 时必须区分。
    fn class_desc_of(&self, h: i32) -> Option<String> {
        let n = self.class_obj_name(h)?;
        if n.starts_with('[') || (n.len() == 1 && "IJFDZBCSV".contains(&n)) {
            Some(n)
        } else {
            Some(format!("L{n};"))
        }
    }

    fn mtype_arg(&mut self, t: &TypedSlot, what: &str) -> Result<String, VmError> {
        self.mtype_of(t.1.as_i32())
            .ok_or_else(|| self.bsm_error(format!("LambdaMetafactory 的 {what} 不是本 VM 的 MethodType")))
    }

    /// lambda 对象上的方法调用（SAM + Object 协议）
    pub(super) fn lambda_dispatch(
        &mut self,
        recv: i32,
        name: &str,
        desc: &str,
        args: &[Slot],
    ) -> Option<Result<Slot, VmError>> {
        let info = self.indy().lambdas.get(&recv).cloned()?;
        let target = info.target;
        let captured = info.captured.clone();
        let captured_desc = info.captured_desc.clone();
        let sams = info.sams.clone();
        let class_name = info.class_name.clone();
        match (name, desc) {
            ("toString", "()Ljava/lang/String;") => {
                let s = self.str_new(format!("{class_name}$$Lambda/0x00000000"));
                return Some(Ok(Slot::I(s)));
            }
            ("hashCode", "()I") => return Some(Ok(Slot::I(self.identity_hash(recv)))),
            ("equals", "(Ljava/lang/Object;)Z") => {
                let other = args.first().copied().unwrap_or(Slot::I(0)).as_i32();
                return Some(Ok(Slot::I(if other == recv { 1 } else { 0 })));
            }
            _ => {}
        }
        if !sams.iter().any(|(n, d)| n == name && d == desc) {
            // 名字对得上但描述符不对：多半是 SAM 的擦除/实例化形态不一致，报出来有用；
            // 其它方法（`getClass` / `hashCode` / 覆写的 `toString` …）不该被 lambda 拦下，
            // 交给正常的方法解析。
            if sams.iter().any(|(n, _)| n == name) {
                return Some(err(format!(
                    "lambda {class_name} 的 {name} 描述符不匹配：调用点是 {desc}，登记的是 {sams:?}"
                )));
            }
            return None;
        }
        let mut all = captured;
        all.extend_from_slice(args);
        let want = prefix_desc(&captured_desc, desc);
        Some(self.invoke_mh_as(target, &want, &all, "lambda 调用"))
    }

    // ---------- ConstantBootstraps ----------

    fn bsm_constant(
        &mut self,
        caller: &str,
        name: &str,
        site_name: &str,
        site_desc: &str,
        typed: &[TypedSlot],
    ) -> Result<Slot, VmError> {
        match name {
            "nullConstant" => Ok(Slot::I(0)),
            "primitiveClass" => {
                let n = site_name.trim();
                let prim = match n {
                    "I" | "int" => "I",
                    "J" | "long" => "J",
                    "F" | "float" => "F",
                    "D" | "double" => "D",
                    "Z" | "boolean" => "Z",
                    "B" | "byte" => "B",
                    "C" | "char" => "C",
                    "S" | "short" => "S",
                    "V" | "void" => "V",
                    other => {
                        return Err(self.bsm_error(format!(
                            "ConstantBootstraps.primitiveClass 不认识的名字 {other:?}"
                        )))
                    }
                };
                Ok(Slot::I(self.class_obj(prim)))
            }
            "invoke" => {
                // 静态参数：第 1 个是 MethodHandle，其余是它的实参
                // （`Lookup` / 名字 / `Class` 是引导方法的形参，不在静态参数表里）
                let Some(mh) = typed.first().map(|t| t.1.as_i32()) else {
                    return Err(self.bsm_error("ConstantBootstraps.invoke 缺少 MethodHandle 参数"));
                };
                let mtype = self.mh_info(mh)
                    .map(|i| i.mtype)
                    .ok_or_else(|| self.bsm_error("ConstantBootstraps.invoke 的第 1 个参数不是方法句柄"))?;
                let rest: Vec<Slot> = typed[1..].iter().map(|t| t.1).collect();
                let mut ctx_args = Vec::with_capacity(rest.len());
                for (i, v) in rest.iter().enumerate() {
                    let ty = typed.get(1 + i).map(|t| t.0.clone()).unwrap_or_default();
                    let want = param_descs(&mtype).get(i).cloned().unwrap_or(ty.clone());
                    ctx_args.push(self.coerce(*v, &ty, &want, "ConstantBootstraps.invoke")?);
                }
                let r = self.dispatch_mh(mh, &ctx_args, "ConstantBootstraps.invoke")?;
                let want = ret_desc(&format!("({site_desc})V"));
                self.coerce(r, &ret_desc(&mtype), &want, "ConstantBootstraps.invoke")
            }
            "fieldVarHandle" | "staticFieldVarHandle" => {
                // 静态参数：声明类、字段类型
                let decl = typed
                    .first()
                    .and_then(|t| self.class_obj_name(t.1.as_i32()))
                    .ok_or_else(|| self.bsm_error("VarHandle 引导方法缺少声明类参数"))?;
                let ftype = typed
                    .get(1)
                    .and_then(|t| self.class_desc_of(t.1.as_i32()))
                    .unwrap_or_else(|| "Ljava/lang/Object;".into());
                let h = self.alloc_vmobj("java/lang/invoke/VarHandle");
                self.indy().varhandles.insert(
                    h,
                    VarHandleInfo {
                        class: decl,
                        name: site_name.to_string(),
                        desc: ftype,
                        is_static: name == "staticFieldVarHandle",
                        kind: VhKind::Field,
                    },
                );
                Ok(Slot::I(h))
            }
            "getStaticFinal" => {
                // 静态参数：可选的声明类。没给就用**调用方类**（javadoc 的约定）
                let decl = match typed.first().and_then(|t| self.class_obj_name(t.1.as_i32())) {
                    Some(d) => d,
                    None => caller.to_string(),
                };
                self.ensure_init(&decl)?;
                let key = (decl.clone(), site_name.to_string());
                if let Some(v) = lk(&self.sh.statics).get(&key).copied() {
                    return Ok(v);
                }
                Ok(Slot::I(0))
            }
            "explicitCast" => {
                // 静态参数：第 1 个是要转换的值
                let v = typed.first().map(|t| t.1).unwrap_or(Slot::I(0));
                let want = site_desc.to_string();
                let src = typed
                    .first()
                    .map(|t| t.0.clone())
                    .unwrap_or_else(|| "Ljava/lang/Object;".into());
                self.coerce(v, &src, &want, "ConstantBootstraps.explicitCast")
            }
            other => Err(self.bsm_error(format!(
                "ConstantBootstraps.{other} 这个引导方法本 VM 还没实现（调用点 {site_name}{site_desc}）"
            ))),
        }
    }

    // ---------- VarHandle ----------

    /// VarHandle 的 `get/set/compareAndSet…` 一族
    fn varhandle_op(
        &mut self,
        vh: i32,
        name: &str,
        desc: &str,
        args: &[Slot],
    ) -> Result<Option<Slot>, VmError> {
        let Some((kind, class, fname, fdesc, is_static)) = self.indy().varhandles.get(&vh).map(|i| {
            (
                i.kind.clone(),
                i.class.clone(),
                i.name.clone(),
                i.desc.clone(),
                i.is_static,
            )
        }) else {
            return Ok(None);
        };
        // ---- 数组形态：坐标是 (数组引用, 下标)，不是 (holder) ----
        if !matches!(kind, VhKind::Field) {
            let arr = args.first().copied().unwrap_or(Slot::I(0)).as_i32();
            let idx = args.get(1).copied().unwrap_or(Slot::I(0)).as_i32();
            if arr == 0 {
                return Err(self.java_exception(
                    "java/lang/NullPointerException",
                    "VarHandle 访问数组时数组引用是 null".to_string(),
                ));
            }
            let len = self.sh.heap.array_len(arr).unwrap_or(0);
            // 元素宽度（ByteView 按字节寻址，余量要够一次读写）
            let width = match &kind {
                VhKind::ByteView { view, .. } => match view.as_str() {
                    "S" | "C" => 2,
                    "I" | "F" => 4,
                    "J" | "D" => 8,
                    _ => 1,
                },
                _ => 1,
            };
            if idx < 0 || (idx as usize) + width > len.max(1) {
                return Err(self.java_exception(
                    "java/lang/IndexOutOfBoundsException",
                    format!("VarHandle 下标 {idx} 越界（长度 {len}，一次读 {width} 字节）"),
                ));
            }
            // 读：把 idx 起的 width 字节按字节序拼成一个 Slot
            let read = |vm: &mut Vm| -> Result<Slot, VmError> {
                match &kind {
                    VhKind::ArrayElem { .. } => {
                        Ok(vm.sh.heap.get(arr, idx as usize).unwrap_or(Slot::I(0)))
                    }
                    VhKind::ByteView { view, big_endian } => {
                        let mut raw = [0u8; 8];
                        for k in 0..width {
                            let b = vm.sh.heap.get(arr, idx as usize + k).map(|s| s.as_i32()).unwrap_or(0) as u8;
                            // 大端：低地址是高字节
                            if *big_endian { raw[k] = b } else { raw[width - 1 - k] = b }
                        }
                        Ok(match view.as_str() {
                            "S" => Slot::I(i16::from_be_bytes([raw[0], raw[1]]) as i32),
                            "C" => Slot::I(u16::from_be_bytes([raw[0], raw[1]]) as i32),
                            "I" => Slot::I(i32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]])),
                            "F" => Slot::Fl(f32::from_bits(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]))),
                            "J" => Slot::L(i64::from_be_bytes(raw)),
                            "D" => Slot::F(f64::from_bits(u64::from_be_bytes(raw))),
                            _ => Slot::I(raw[0] as i32),
                        })
                    }
                    VhKind::Field => unreachable!(),
                }
            };
            let write = |vm: &mut Vm, v: Slot| -> Result<(), VmError> {
                match &kind {
                    VhKind::ArrayElem { .. } => {
                        vm.sh.heap.set(arr, idx as usize, v);
                    }
                    VhKind::ByteView { view, big_endian } => {
                        let raw: [u8; 8] = match view.as_str() {
                            "S" => {
                                let mut b = [0u8; 8];
                                b[..2].copy_from_slice(&(v.as_i32() as i16).to_be_bytes());
                                b
                            }
                            "C" => {
                                let mut b = [0u8; 8];
                                b[..2].copy_from_slice(&(v.as_i32() as u16).to_be_bytes());
                                b
                            }
                            "I" => {
                                let mut b = [0u8; 8];
                                b[..4].copy_from_slice(&v.as_i32().to_be_bytes());
                                b
                            }
                            "F" => {
                                let mut b = [0u8; 8];
                                b[..4].copy_from_slice(&v.as_f32().to_bits().to_be_bytes());
                                b
                            }
                            "J" => v.as_i64().to_be_bytes(),
                            "D" => v.as_f64().to_bits().to_be_bytes(),
                            _ => [0u8; 8],
                        };
                        for k in 0..width {
                            let b = if *big_endian { raw[k] } else { raw[width - 1 - k] };
                            vm.sh.heap.set(arr, idx as usize + k, Slot::I(b as i32));
                        }
                    }
                    VhKind::Field => unreachable!(),
                }
                Ok(())
            };
            let coord = 2usize; // (数组, 下标) 之后才是值
            let params = param_descs(desc);
            return Ok(Some(match name {
                "get" | "getVolatile" | "getAcquire" | "getOpaque" | "getPlain" => {
                    let v = read(self)?;
                    self.coerce(v, &fdesc, &ret_desc(desc), name)?
                }
                "set" | "setVolatile" | "setRelease" | "setOpaque" | "setPlain" => {
                    let v = args[coord];
                    let v = self.coerce(v, &params[coord], &fdesc, name)?;
                    write(self, v)?;
                    Slot::I(0)
                }
                "getAndSet" => {
                    let old = read(self)?;
                    let v = args[coord];
                    let v = self.coerce(v, &params[coord], &fdesc, name)?;
                    write(self, v)?;
                    self.coerce(old, &fdesc, &ret_desc(desc), name)?
                }
                "compareAndSet" | "weakCompareAndSet" => {
                    let expected = self.coerce(args[coord], &params[coord], &fdesc, name)?;
                    let newv = self.coerce(args[coord + 1], &params[coord + 1], &fdesc, name)?;
                    let old = read(self)?;
                    let same = old == expected;
                    if same {
                        write(self, newv)?;
                    }
                    Slot::I(if same { 1 } else { 0 })
                }
                "compareAndExchange" => {
                    let expected = self.coerce(args[coord], &params[coord], &fdesc, name)?;
                    let newv = self.coerce(args[coord + 1], &params[coord + 1], &fdesc, name)?;
                    let old = read(self)?;
                    if old == expected {
                        write(self, newv)?;
                    }
                    old
                }
                other => {
                    return Err(self.bsm_error(format!(
                        "VM 未实现的数组形态 VarHandle.{other}{desc}"
                    )))
                }
            }));
        }
        let (class, fname, fdesc, is_static) = (class, fname, fdesc, is_static);
        // 坐标参数：静态字段 0 个，实例字段 1 个（holder）
        let coord = usize::from(!is_static);
        let _ = desc;
        let holder = if is_static { 0 } else { args[0].as_i32() };
        let read = |vm: &mut Vm| -> Result<Slot, VmError> {
            if is_static {
                vm.ensure_init(&class)?;
                Ok(lk(&vm.sh.statics)
                    .get(&(class.clone(), fname.clone()))
                    .copied()
                    .unwrap_or(Slot::I(0)))
            } else {
                if holder == 0 {
                    return Err(vm.java_exception(
                        "java/lang/NullPointerException",
                        format!("VarHandle 访问 {class}.{fname} 时 holder 是 null"),
                    ));
                }
                let f = vm.field_offset(holder, &class, &fname)?;
                Ok(vm.sh.heap.get(holder, f).unwrap_or(Slot::I(0)))
            }
        };
        let write = |vm: &mut Vm, v: Slot| -> Result<(), VmError> {
            if is_static {
                vm.ensure_init(&class)?;
                lk(&vm.sh.statics).insert((class.clone(), fname.clone()), v);
            } else {
                if holder == 0 {
                    return Err(vm.java_exception(
                        "java/lang/NullPointerException",
                        format!("VarHandle 访问 {class}.{fname} 时 holder 是 null"),
                    ));
                }
                let f = vm.field_offset(holder, &class, &fname)?;
                vm.sh.heap.set(holder, f, v);
            }
            Ok(())
        };
        match name {
            "get" | "getVolatile" | "getAcquire" | "getOpaque" | "getPlain" => {
                let v = read(self)?;
                Ok(Some(self.coerce(v, &fdesc, &ret_desc(desc), name)?))
            }
            "set" | "setVolatile" | "setRelease" | "setOpaque" | "setPlain" => {
                let v = args[coord];
                let v = self.coerce(v, &param_descs(desc)[coord], &fdesc, name)?;
                write(self, v)?;
                Ok(Some(Slot::I(0)))
            }
            "getAndSet" => {
                let old = read(self)?;
                let v = args[coord];
                let v = self.coerce(v, &param_descs(desc)[coord], &fdesc, name)?;
                write(self, v)?;
                Ok(Some(self.coerce(old, &fdesc, &ret_desc(desc), name)?))
            }
            "compareAndSet" | "weakCompareAndSet" => {
                let expected = args[coord];
                let newv = args[coord + 1];
                let params = param_descs(desc);
                let expected = self.coerce(expected, &params[coord], &fdesc, name)?;
                let newv = self.coerce(newv, &params[coord + 1], &fdesc, name)?;
                let old = read(self)?;
                let same = old == expected;
                if same {
                    write(self, newv)?;
                }
                Ok(Some(Slot::I(if same { 1 } else { 0 })))
            }
            "compareAndExchange" => {
                let expected = args[coord];
                let newv = args[coord + 1];
                let params = param_descs(desc);
                let expected = self.coerce(expected, &params[coord], &fdesc, name)?;
                let newv = self.coerce(newv, &params[coord + 1], &fdesc, name)?;
                let old = read(self)?;
                if old == expected {
                    write(self, newv)?;
                }
                Ok(Some(old))
            }
            "getAndAdd" | "getAndIncrement" | "getAndDecrement" => {
                let old = read(self)?;
                let delta = match name {
                    "getAndIncrement" => Slot::I(1),
                    "getAndDecrement" => Slot::I(-1),
                    _ => args[coord],
                };
                let sum = match fdesc.as_str() {
                    "J" => Slot::L(old.as_i64().wrapping_add(delta.as_i64())),
                    "D" => Slot::F(old.as_f64() + delta.as_f64()),
                    "F" => Slot::Fl(old.as_f32() + delta.as_f32()),
                    _ => Slot::I(old.as_i32().wrapping_add(delta.as_i32())),
                };
                write(self, sum)?;
                Ok(Some(self.coerce(old, &fdesc, &ret_desc(desc), name)?))
            }
            _ => Ok(None),
        }
    }

    // ---------- java.lang.invoke 的字节码级调用 ----------

    /// 这个类的方法调用是否由本模块接管（而不是执行 java.base 的字节码）
    pub(super) fn is_invoke_api_class(class: &str) -> bool {
        class.starts_with("java/lang/invoke/")
    }

    /// 字节码里的 `java.lang.invoke.*` 调用。
    ///
    /// `kind`：0 = invokevirtual/invokeinterface，1 = invokestatic，2 = invokespecial。
    /// 返回 `Some(v)` 表示已处理；`None` 表示不认识的成员（由调用方决定怎么办）。
    pub(super) fn invoke_api(
        &mut self,
        kind: u8,
        class: &str,
        name: &str,
        desc: &str,
        recv: i32,
        args: &[Slot],
    ) -> Result<Option<Slot>, VmError> {
        // ---------------- MethodHandles（静态工厂） ----------------
        if class == "java/lang/invoke/MethodHandles" {
            return Ok(Some(match name {
                "lookup" | "publicLookup" => {
                    let caller = self.store
                        .classes
                        .get(0)
                        .and_then(|c| c.this_class_name())
                        .unwrap_or_default();
                    let h = self.alloc_vmobj("java/lang/invoke/MethodHandles$Lookup");
                    self.indy().lookups.insert(h, caller);
                    Slot::I(h)
                }
                "privateLookupIn" => {
                    let l = args.get(1).copied().unwrap_or(Slot::I(0));
                    l
                }
                "insertArguments" => {
                    let mh = args.first().copied().unwrap_or(Slot::I(0)).as_i32();
                    let pos = args.get(1).copied().unwrap_or(Slot::I(0)).as_i32().max(0) as usize;
                    let arr = args.get(2).copied().unwrap_or(Slot::I(0)).as_i32();
                    let vals = self.array_slots(arr);
                    let h = self.make_bound(mh, pos, vals, "Ljava/lang/Object;", "insertArguments")?;
                    Slot::I(h)
                }
                "constant" => {
                    // (Class, Object)：值是**装箱的**（`constant(int.class, 99)` 传的是 Integer），
                    // 要按声明的类型拆箱
                    let ty = self.class_desc_of(args.first().copied().unwrap_or(Slot::I(0)).as_i32())
                        .unwrap_or_else(|| "Ljava/lang/Object;".into());
                    let raw = args.get(1).copied().unwrap_or(Slot::I(0));
                    let v = self.coerce(raw, "Ljava/lang/Object;", &ty, "MethodHandles.constant")?;
                    let mh = self.mh_obj(MhKind::Constant(v), format!("(){ty}"));
                    Slot::I(mh)
                }
                "identity" => {
                    let ty = self.class_desc_of(args.first().copied().unwrap_or(Slot::I(0)).as_i32())
                        .unwrap_or_else(|| "Ljava/lang/Object;".into());
                    let mh = self.mh_obj(MhKind::Identity, format!("({ty}){ty}"));
                    Slot::I(mh)
                }
                // `MethodHandles.arrayElementVarHandle(T[].class)`：坐标 `(数组, 下标)`。
                // 少了它，`Arrays.copyOf` 之后一大堆走 VarHandle 的 JDK 代码全部不可用。
                "arrayElementVarHandle" => {
                    let adesc = self
                        .class_desc_of(args.first().copied().unwrap_or(Slot::I(0)).as_i32())
                        .ok_or_else(|| self.bsm_error("arrayElementVarHandle 的参数不是 Class".to_string()))?;
                    let elem = adesc
                        .strip_prefix('[')
                        .ok_or_else(|| {
                            self.bsm_error(format!("arrayElementVarHandle 收到非数组类型 {adesc}"))
                        })?
                        .to_string();
                    let h = self.alloc_vmobj("java/lang/invoke/VarHandle");
                    self.indy().varhandles.insert(
                        h,
                        VarHandleInfo {
                            class: String::new(),
                            name: String::new(),
                            desc: elem.clone(),
                            is_static: false,
                            kind: VhKind::ArrayElem { elem },
                        },
                    );
                    Slot::I(h)
                }
                // `MethodHandles.byteArrayViewVarHandle(int[].class, order)`：
                // 坐标 `(byte[], 字节下标)`，一次读写 view 宽度，按字节序拼。
                // `RandomAccessFile.writeInt/readLong` 全家都走这条。
                "byteArrayViewVarHandle" => {
                    let vdesc = self
                        .class_desc_of(args.first().copied().unwrap_or(Slot::I(0)).as_i32())
                        .ok_or_else(|| {
                            self.bsm_error("byteArrayViewVarHandle 的第 1 个参数不是 Class".to_string())
                        })?;
                    let view = vdesc
                        .strip_prefix('[')
                        .ok_or_else(|| {
                            self.bsm_error(format!("byteArrayViewVarHandle 收到非数组类型 {vdesc}"))
                        })?
                        .to_string();
                    // 第 2 个参数是 ByteOrder：BIG_ENDIAN / LITTLE_ENDIAN 的句柄。
                    // 我们没有真对象可比，按「非 null 就查它的单例名」处理；查不到按 native order。
                    let order = args.get(1).copied().unwrap_or(Slot::I(0)).as_i32();
                    let big_endian = if order == 0 {
                        cfg!(target_endian = "big")
                    } else {
                        // `ByteOrder.BIG_ENDIAN` / `LITTLE_ENDIAN` 的 `name` 字段区分
                        let n = self.get_named(order, "java/nio/ByteOrder", "name");
                        let txt = self.text_of(n.as_i32());
                        if txt.is_empty() {
                            cfg!(target_endian = "big")
                        } else {
                            txt != "LITTLE_ENDIAN"
                        }
                    };
                    let h = self.alloc_vmobj("java/lang/invoke/VarHandle");
                    self.indy().varhandles.insert(
                        h,
                        VarHandleInfo {
                            class: String::new(),
                            name: String::new(),
                            desc: view.clone(),
                            is_static: false,
                            kind: VhKind::ByteView { view, big_endian },
                        },
                    );
                    Slot::I(h)
                }
                other => {
                    return Err(self.bsm_error(format!(
                        "VM 未实现的 MethodHandles.{other}{desc}（java.lang.invoke 由 VM 接管，不支持的就是没有）"
                    )))
                }
            }));
        }
        // ---------------- MethodHandles$Lookup ----------------
        if class == "java/lang/invoke/MethodHandles$Lookup" {
            return Ok(Some(self.lookup_op(kind, name, desc, recv, args)?));
        }
        // ---------------- MethodHandle ----------------
        if class == "java/lang/invoke/MethodHandle" {
            return Ok(Some(self.method_handle_op(name, desc, recv, args)?));
        }
        // ---------------- MethodType ----------------
        if class == "java/lang/invoke/MethodType" {
            return Ok(Some(self.method_type_op(name, desc, recv, args)?));
        }
        // ---------------- CallSite 家族 ----------------
        if class.contains("CallSite") {
            return Ok(Some(self.callsite_op(kind, class, name, desc, recv, args)?));
        }
        // ---------------- VarHandle ----------------
        if class == "java/lang/invoke/VarHandle" {
            if let Some(v) = self.varhandle_op(recv, name, desc, args)? {
                return Ok(Some(v));
            }
            return Err(self.bsm_error(format!(
                "VM 未实现的 VarHandle.{name}{desc}"
            )));
        }
        // ---------------- ConstantBootstraps：直接调用（不经过 indy） ----------------
        if class == "java/lang/invoke/ConstantBootstraps" {
            // 它们的前三个形参就是 (Lookup, name, Class)，其余才是工厂参数 —— 和引导方法
            // 那套一模一样，所以直接复用 `bsm_constant`
            let params = param_descs(desc);
            let site_name = self.value_text(args.get(1).copied().unwrap_or(Slot::I(0)).as_i32())?;
            let site_desc = args
                .get(2)
                .and_then(|s| self.class_desc_of(s.as_i32()))
                .unwrap_or_else(|| "Ljava/lang/Object;".into());
            let typed: Vec<TypedSlot> = params
                .iter()
                .skip(3)
                .cloned()
                .zip(args.iter().skip(3).copied())
                .collect();
            let caller = args
                .first()
                .and_then(|s| self.indy().lookups.get(&s.as_i32()).cloned())
                .unwrap_or_else(|| {
                    self.store
                        .classes
                        .get(0)
                        .and_then(|c| c.this_class_name())
                        .unwrap_or_default()
                });
            return Ok(Some(self.bsm_constant(
                &caller, name, &site_name, &site_desc, &typed,
            )?));
        }
        Ok(None)
    }

    fn array_slots(&self, h: i32) -> Vec<Slot> {
        if h < 0 {
            self.sh.heap.snapshot(h).unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// 内建替身表里有没有这个成员（独立模式下的 `findStatic` 校验用）
    fn builtin_has_member(&self, class: &str, name: &str, desc: &str) -> bool {
        jdk::vop(class, name, desc).is_some() || jdk::sop(class, name, desc).is_some()
    }

    fn lookup_op(
        &mut self,
        kind: u8,
        name: &str,
        desc: &str,
        recv: i32,
        args: &[Slot],
    ) -> Result<Slot, VmError> {
        match (kind, name) {
            (2, _) => Err(self.bsm_error(format!(
                "Lookup.{name} 是构造器，不该被静态调用"
            ))),
            (_, "in") | (_, "dropLookupMode") => Ok(args.first().copied().unwrap_or(Slot::I(0))),
            (_, "accessClass") => Ok(args.first().copied().unwrap_or(Slot::I(0))),
            // `Lookup.ensureInitialized(Class)Class`：触发目标类的 `<clinit>` 并把类对象返回。
            // 少了它，`SharedSecrets` 没装好时的兜底路径（`MethodHandles.lookup()
            // .ensureInitialized(FileDescriptor.class)`）会在 `<clinit>` 里
            // 抛 `BootstrapMethodError` —— 于是任何文件写入都跑不起来。
            (_, "ensureInitialized") => {
                let c = args.first().copied().unwrap_or(Slot::I(0));
                let cname = self
                    .class_obj_name(c.as_i32())
                    .ok_or_else(|| self.bsm_error("Lookup.ensureInitialized 的参数不是 Class".to_string()))?;
                self.ensure_init(&cname)?;
                Ok(c)
            }
            (_, "lookupClass") => {
                // 返回 this 的查找类。**必须是真的类对象**：`MhUtil.findVarHandle(lookup, name, type)`
                // 就是 `lookup.findVarHandle(lookup.lookupClass(), ...)`，返回 0 会让
                // `findVarHandle` 报「第一个参数不是 Class」—— 实测卡死
                // `ThreadBuilders$BaseThreadFactory.<clinit>`（进而 ProcessBuilder/ProcessHandle 全废）。
                let name = self.indy().lookups.get(&recv).cloned();
                match name {
                    Some(c) => Ok(Slot::I(self.class_obj(&c))),
                    None => Ok(Slot::I(0)),
                }
            }
            // **`findConstructor` 是 2 个参数**：`(Class, MethodType)`，没有方法名。
            // 被并进下面那个 3 参分支时 `args.get(2)` 恒为空 → 报「第 3 个参数不是 MethodType」，
            // 而这条路径是**反射造对象的必经之路**（`Constructor.newInstance` →
            // `DirectConstructorHandleAccessor` → `findConstructor`）：
            // `FileChannel.map()` 的 `Util.newMappedByteBuffer` 就炸在这。
            (_, "findConstructor") => {
                let c = self.class_obj_name(args.first().copied().unwrap_or(Slot::I(0)).as_i32())
                    .ok_or_else(|| {
                        self.bsm_error("Lookup.findConstructor 的第 1 个参数不是 Class".to_string())
                    })?;
                let mt = self.mtype_of(args.get(1).copied().unwrap_or(Slot::I(0)).as_i32())
                    .ok_or_else(|| {
                        self.bsm_error("Lookup.findConstructor 的第 2 个参数不是 MethodType".to_string())
                    })?;
                let n = "<init>".to_string();
                // 构造器的 MethodType 是 `()V` 形态：参数一致、返回 void；
                // 句柄自身的类型则是 `(参数)类`
                let ctor_desc = {
                    let close = mt.find(')').unwrap_or(1);
                    format!("{}V", &mt[..close + 1])
                };
                let resolved = self.store.resolve_method(&c, &n, &ctor_desc).is_some()
                    || self.builtin_has_member(&c, &n, &ctor_desc);
                if !resolved {
                    return Err(self.java_exception(
                        "java/lang/NoSuchMethodException",
                        format!("no such constructor: {}.<init>{}", c.replace('/', "."), ctor_desc),
                    ));
                }
                let mtype = handle_type(8, &c, &ctor_desc);
                let h = self.mh_obj(
                    MhKind::Direct { ref_kind: 8, class: c, name: n, desc: ctor_desc },
                    mtype,
                );
                Ok(Slot::I(h))
            }
            // `findSpecial(Class, String, MethodType, Class specialCaller)` —— **4 个参数**，
            // 第 4 个是调用者类。我们的 `invokeSpecial` 不需要它（按引用类直接解析），
            // 所以只取前三个。
            (_, "findStatic") | (_, "findVirtual") | (_, "findSpecial") => {
                let c = self.class_obj_name(args.first().copied().unwrap_or(Slot::I(0)).as_i32())
                    .ok_or_else(|| self.bsm_error(format!("Lookup.{name} 的第一个参数不是 Class")))?;
                let n = self.value_text(args.get(1).copied().unwrap_or(Slot::I(0)).as_i32())?;
                let mt = self.mtype_of(args.get(2).copied().unwrap_or(Slot::I(0)).as_i32())
                    .ok_or_else(|| {
                        self.bsm_error(format!("Lookup.{name} 的第 3 个参数不是 MethodType"))
                    })?;
                let ref_kind: u8 = match name {
                    "findStatic" => 6,
                    "findVirtual" => 5,
                    "findSpecial" => 7,
                    _ => 8,
                };
                // `findConstructor` 的 MethodType 是 `()V` 形态的构造器类型：参数一致、
                // 返回 `void`；句柄自身的类型则是 `(参数)类`
                let ctor_desc = if ref_kind == 8 {
                    let close = mt.find(')').unwrap_or(1);
                    format!("{}V", &mt[..close + 1])
                } else {
                    mt.clone()
                };
                // 校验：方法真的存在（错误信息里要能看出找的是什么）
                let resolved = self.store.resolve_method(&c, &n, &ctor_desc).is_some()
                    || self.builtin_has_member(&c, &n, &ctor_desc);
                if ref_kind != 8 && !resolved {
                    // 与 HotSpot 一致：找不到方法抛 `NoSuchMethodException`。
                    // 在引导方法里抛出时，`run_bootstrap` 会按规范包一层 BootstrapMethodError。
                    let kind = match ref_kind {
                        6 => "invokeStatic",
                        5 => "invokeVirtual",
                        _ => "invokeSpecial",
                    };
                    return Err(self.java_exception(
                        "java/lang/NoSuchMethodException",
                        format!("no such method: {}.{}{}/{kind}", c.replace('/', "."), n, ctor_desc),
                    ));
                }
                let mtype = handle_type(ref_kind, &c, &ctor_desc);
                let h = self.mh_obj(
                    MhKind::Direct { ref_kind, class: c, name: n, desc: ctor_desc },
                    mtype,
                );
                Ok(Slot::I(h))
            }
            (_, "findGetter") | (_, "findSetter") | (_, "findStaticGetter") | (_, "findStaticSetter") => {
                let c = self.class_obj_name(args.first().copied().unwrap_or(Slot::I(0)).as_i32())
                    .ok_or_else(|| self.bsm_error(format!("Lookup.{name} 的第一个参数不是 Class")))?;
                let f = self.value_text(args.get(1).copied().unwrap_or(Slot::I(0)).as_i32())?;
                let fdesc = self.class_desc_of(args.get(2).copied().unwrap_or(Slot::I(0)).as_i32())
                    .unwrap_or_else(|| "Ljava/lang/Object;".into());
                let (ref_kind, mtype) = match name {
                    "findGetter" => (1, format!("(L{c};){fdesc}")),
                    "findSetter" => (3, format!("(L{c};{fdesc})V")),
                    "findStaticGetter" => (2, format!("(){fdesc}")),
                    _ => (4, format!("({fdesc})V")),
                };
                let h = self.mh_obj(
                    MhKind::Direct { ref_kind, class: c, name: f, desc: fdesc },
                    mtype,
                );
                Ok(Slot::I(h))
            }
            (_, "findVarHandle") | (_, "findStaticVarHandle") => {
                let c = self.class_obj_name(args.first().copied().unwrap_or(Slot::I(0)).as_i32())
                    .ok_or_else(|| self.bsm_error(format!("Lookup.{name} 的第一个参数不是 Class")))?;
                let f = self.value_text(args.get(1).copied().unwrap_or(Slot::I(0)).as_i32())?;
                let fdesc = self.class_desc_of(args.get(2).copied().unwrap_or(Slot::I(0)).as_i32())
                    .unwrap_or_else(|| "Ljava/lang/Object;".into());
                let h = self.alloc_vmobj("java/lang/invoke/VarHandle");
                self.indy().varhandles.insert(
                    h,
                    VarHandleInfo {
                        class: c,
                        name: f,
                        desc: fdesc,
                        is_static: name.starts_with("findStatic"),
                        kind: VhKind::Field,
                    },
                );
                Ok(Slot::I(h))
            }
            (_, other) => Err(self.bsm_error(format!(
                "VM 未实现的 MethodHandles.Lookup.{other}{desc}"
            ))),
        }
    }

    fn method_handle_op(
        &mut self,
        name: &str,
        desc: &str,
        recv: i32,
        args: &[Slot],
    ) -> Result<Slot, VmError> {
        match name {
            "invokeExact" => {
                let info = self.mh_info(recv).ok_or_else(|| {
                    let msg = format!("MethodHandle.invokeExact: 接收者 {recv} 不是方法句柄");
                    VmError { msg, exc: None }
                })?;
                if info.mtype != desc {
                    return Err(self.wrong_type(format!(
                        "invokeExact 要求句柄类型与调用点完全一致：句柄是 {}，调用点是 {desc}",
                        info.mtype
                    )));
                }
                self.invoke_mh_as(recv, desc, args, "MethodHandle.invokeExact")
            }
            "invoke" => self.invoke_mh_as(recv, desc, args, "MethodHandle.invoke"),
            "invokeWithArguments" => {
                // (Object[]) → 按句柄自身的类型把数组元素当实参
                let arr = args.first().copied().unwrap_or(Slot::I(0)).as_i32();
                let vals = self.array_slots(arr);
                let info = self.mh_info(recv)
                    .ok_or_else(|| self.wrong_type("invokeWithArguments: 接收者不是方法句柄"))?;
                let n = param_descs(&info.mtype).len();
                if vals.len() != n {
                    return Err(self.wrong_type(format!(
                        "invokeWithArguments 给了 {} 个实参，句柄 {} 要 {n} 个",
                        vals.len(),
                        info.mtype
                    )));
                }
                let want = format!(
                    "({}){}",
                    vec!["Ljava/lang/Object;"; n].concat(),
                    ret_desc(&info.mtype)
                );
                self.invoke_mh_as(recv, &want, &vals, "MethodHandle.invokeWithArguments")
            }
            "type" => {
                let t = self.mh_info(recv)
                    .map(|i| i.mtype)
                    .ok_or_else(|| self.wrong_type("MethodHandle.type(): 接收者不是方法句柄"))?;
                Ok(Slot::I(self.mtype_obj(&t)))
            }
            "asType" => {
                let want = self.mtype_of(args.first().copied().unwrap_or(Slot::I(0)).as_i32())
                    .ok_or_else(|| self.bsm_error("MethodHandle.asType: 参数不是 MethodType"))?;
                let have = self.mh_info(recv)
                    .map(|i| i.mtype)
                    .ok_or_else(|| self.wrong_type("MethodHandle.asType: 接收者不是方法句柄"))?;
                // 规范要求：asType **只允许** 宽化 / 装箱 / 拆箱 / 引用转换。
                // 窄化（long→int）不是 asType 的活，必须当场抛 WrongMethodTypeException
                // —— 而不是留到调用时静默截断。
                let hp = param_descs(&have);
                let wp = param_descs(&want);
                if hp.len() != wp.len() {
                    return Err(self.wrong_type(format!(
                        "asType: 参数个数不一致（句柄 {have}，目标 {want}）"
                    )));
                }
                for (i, (h, w)) in hp.iter().zip(wp.iter()).enumerate() {
                    if !can_convert(w, h) {
                        return Err(self.wrong_type(format!(
                            "asType: 第 {} 个参数不能从 {w} 转成 {h}（句柄 {have} → 目标 {want}）",
                            i + 1
                        )));
                    }
                }
                let hr = ret_desc(&have);
                let wr = ret_desc(&want);
                if wr != "V" && !can_convert(&hr, &wr) {
                    return Err(self.wrong_type(format!(
                        "asType: 返回值不能从 {hr} 转成 {wr}（句柄 {have} → 目标 {want}）"
                    )));
                }
                Ok(Slot::I(self.mh_obj(MhKind::Adapter { target: recv }, want)))
            }
            "bindTo" => {
                let v = args.first().copied().unwrap_or(Slot::I(0));
                let h = self.make_bound(recv, 0, vec![v], "Ljava/lang/Object;", "MethodHandle.bindTo")?;
                Ok(Slot::I(h))
            }
            "toString" => {
                let ty = self.mh_info(recv)
                    .map(|i| i.mtype)
                    .unwrap_or_else(|| "?".into());
                let s = self.str_new(format!("MethodHandle{ty}"));
                Ok(Slot::I(s))
            }
            "hashCode" => Ok(Slot::I(self.identity_hash(recv))),
            other => Err(self.bsm_error(format!(
                "VM 未实现的 MethodHandle.{other}{desc}（java.lang.invoke 由 VM 接管）"
            ))),
        }
    }

    fn method_type_op(
        &mut self,
        name: &str,
        desc: &str,
        recv: i32,
        args: &[Slot],
    ) -> Result<Slot, VmError> {
        match name {
            "methodType" => {
                // `methodType` 有 5 个重载：(Class) / (Class,Class) / (Class,Class[]) /
                // (Class,Class,Class[]) / (Class,List)。**按签名走**，不要按「第 2 个参数
                // 长得像什么」猜 —— `methodType(int.class, int.class)` 走的是单参数重载。
                let params = param_descs(desc);
                let r = self.class_desc_of(args.first().copied().unwrap_or(Slot::I(0)).as_i32())
                    .unwrap_or_else(|| "V".into());
                let mut ps = String::new();
                for (i, p) in params.iter().enumerate().skip(1) {
                    let v = args.get(i).copied().unwrap_or(Slot::I(0)).as_i32();
                    match p.as_str() {
                        "[Ljava/lang/Class;" => {
                            // 末尾的 varargs 数组（可能是 null）
                            for s in self.array_slots(v) {
                                ps.push_str(
                                    &self.class_desc_of(s.as_i32())
                                        .unwrap_or_else(|| "Ljava/lang/Object;".into()),
                                );
                            }
                        }
                        "Ljava/lang/Class;" => {
                            ps.push_str(
                                &self.class_desc_of(v)
                                    .unwrap_or_else(|| "Ljava/lang/Object;".into()),
                            );
                        }
                        "Ljava/util/List;" => {
                            // `List` 重载：只支持空 List（我们还没有集合迭代器的通用支持）
                            if v != 0 {
                                return Err(self.bsm_error(format!(
                                    "MethodType.methodType(Class, List) 只支持空 List（收到 {}）",
                                    self.type_name(v)
                                )));
                            }
                        }
                        other => {
                            return Err(self.bsm_error(format!(
                                "MethodType.methodType 的这个重载（第 {} 个参数是 {other}）不认识",
                                i + 1
                            )))
                        }
                    }
                }
                Ok(Slot::I(self.mtype_obj(&format!("({ps}){r}"))))
            }
            "fromMethodDescriptorString" => {
                let s = self.value_text(args.first().copied().unwrap_or(Slot::I(0)).as_i32())?;
                Ok(Slot::I(self.mtype_obj(&s)))
            }
            "parameterCount" => {
                let d = self.mtype_of(recv)
                    .ok_or_else(|| self.bsm_error("MethodType.parameterCount: 接收者不是 MethodType"))?;
                Ok(Slot::I(param_descs(&d).len() as i32))
            }
            "parameterType" => {
                let d = self.mtype_of(recv)
                    .ok_or_else(|| self.bsm_error("MethodType.parameterType: 接收者不是 MethodType"))?;
                let i = args.first().copied().unwrap_or(Slot::I(0)).as_i32().max(0) as usize;
                let p = param_descs(&d).get(i).cloned().unwrap_or_else(|| "V".into());
                Ok(Slot::I(self.class_obj(&p)))
            }
            "parameterArray" => {
                let d = self.mtype_of(recv)
                    .ok_or_else(|| self.bsm_error("MethodType.parameterArray: 接收者不是 MethodType"))?;
                let mut items = Vec::new();
                for p in param_descs(&d) {
                    let h = self.class_obj(&p);
                    items.push(Slot::I(h));
                }
                let arr = self.new_ref_array("[Ljava/lang/Class;", items);
                Ok(Slot::I(arr))
            }
            "returnType" => {
                let d = self.mtype_of(recv)
                    .ok_or_else(|| self.bsm_error("MethodType.returnType: 接收者不是 MethodType"))?;
                let r = ret_desc(&d);
                Ok(Slot::I(self.class_obj(&r)))
            }
            "descriptorString" | "toMethodDescriptorString" => {
                let d = self.mtype_of(recv)
                    .ok_or_else(|| self.bsm_error(format!("MethodType.{name}: 接收者不是 MethodType")))?;
                Ok(Slot::I(self.str_new(d)))
            }
            "toString" => {
                let d = self.mtype_of(recv)
                    .ok_or_else(|| self.bsm_error("MethodType.toString: 接收者不是 MethodType"))?;
                // Java 的形态是 `(int,long)void`（类型名，不是描述符）
                let ps: Vec<String> = param_descs(&d).iter().map(|p| java_type_name(p)).collect();
                let text = format!("({}){}", ps.join(","), java_type_name(&ret_desc(&d)));
                Ok(Slot::I(self.str_new(text)))
            }
            "equals" => {
                let a = self.mtype_of(recv);
                let b = self.mtype_of(args.first().copied().unwrap_or(Slot::I(0)).as_i32());
                Ok(Slot::I(if a.is_some() && a == b { 1 } else { 0 }))
            }
            "hashCode" => {
                let d = self.mtype_of(recv).unwrap_or_default();
                let mut h = 0i32;
                for c in d.chars() {
                    h = h.wrapping_mul(31).wrapping_add(c as i32);
                }
                Ok(Slot::I(h))
            }
            other => Err(self.bsm_error(format!(
                "VM 未实现的 MethodType.{other}{desc}"
            ))),
        }
    }

    fn callsite_op(
        &mut self,
        kind: u8,
        class: &str,
        name: &str,
        desc: &str,
        recv: i32,
        args: &[Slot],
    ) -> Result<Slot, VmError> {
        if kind == 2 && name == "<init>" {
            // `new ConstantCallSite(mh)` / `new MutableCallSite(mh|mt)`
            let target = args.first().copied().unwrap_or(Slot::I(0)).as_i32();
            let mutable = class.contains("Mutable");
            if self.is_mh(target) {
                self.indy().callsites.insert(recv, CallSiteInfo { target, mutable });
                if let Some(i) = self.store.field_index(class, "target") {
                    self.sh.heap.set(recv, i, Slot::I(target));
                }
            } else if let Some(mt) = self.mtype_of(target) {
                // MutableCallSite(MethodType)：先放一个「恒等」目标，等 setTarget 覆盖
                let mh = self.mh_obj(MhKind::Identity, mt.clone());
                self.indy().callsites.insert(recv, CallSiteInfo { target: mh, mutable: true });
            } else {
                return Err(self.bsm_error(format!(
                    "{}.<init>({desc}) 的参数既不是方法句柄也不是 MethodType",
                    class.replace('/', ".")
                )));
            }
            return Ok(Slot::I(0));
        }
        match name {
            "getTarget" | "dynamicInvoker" => {
                let t = self.indy().callsites.get(&recv).map(|i| i.target);
                match t {
                    Some(t) => Ok(Slot::I(t)),
                    None => Err(self.bsm_error(format!(
                        "CallSite.getTarget(): 句柄 {recv} 不是本 VM 的调用点"
                    ))),
                }
            }
            "setTarget" => {
                let target = args.first().copied().unwrap_or(Slot::I(0)).as_i32();
                if !self.is_mh(target) {
                    return Err(self.wrong_type("CallSite.setTarget: 参数不是方法句柄"));
                }
                let mutable = self.indy().callsites.get(&recv).map(|i| i.mutable);
                match mutable {
                    Some(true) => {
                        if let Some(i) = self.indy().callsites.get_mut(&recv) {
                            i.target = target;
                        }
                        Ok(Slot::I(0))
                    }
                    Some(false) => Err(self.java_exception(
                        "java/lang/UnsupportedOperationException",
                        "ConstantCallSite.setTarget()".to_string(),
                    )),
                    None => Err(self.bsm_error(format!(
                        "CallSite.setTarget(): 句柄 {recv} 不是本 VM 的调用点"
                    ))),
                }
            }
            "type" => {
                let mh = self.indy().callsites.get(&recv).map(|i| i.target);
                match mh.and_then(|m| self.mh_info(m)).map(|i| i.mtype) {
                    Some(t) => Ok(Slot::I(self.mtype_obj(&t))),
                    None => Err(self.bsm_error("CallSite.type(): 拿不到目标句柄")),
                }
            }
            "toString" => Ok(Slot::I(self.str_new(class.replace('/', ".")))),
            other => Err(self.bsm_error(format!(
                "VM 未实现的 {}.{other}{desc}",
                class.replace('/', ".")
            ))),
        }
    }

    // ---------- 统计 ----------

    /// `-Dvmless.indy.stats` / `VMLESS_INDY_STATS=1` 时打印的一行统计
    pub(super) fn indy_stats_lines(&self) -> Vec<String> {
        vec![
            format!(
                "[indy] 调用点解析 {} 次，执行 {} 次，lambda 实例 {} 个，缓存命中 {} 次",
                self.indy().indy_resolved,
                self.indy().indy_calls,
                self.indy().lambda_seq,
                self.indy().indy_calls.saturating_sub(self.indy().indy_resolved)
            ),
        ]
    }
}

/// 常量池 `NameAndType` → `(名字, 描述符)`
fn nat_of(cf: &crate::ClassFile, index: u16) -> Result<(String, String), VmError> {
    match cf.constant_pool.get(index as usize).and_then(|e| e.as_ref()) {
        Some(CpEntry::NameAndType(n, d)) => Ok((
            String::from_utf8_lossy(cf.utf8(*n).unwrap_or_default()).into_owned(),
            String::from_utf8_lossy(cf.utf8(*d).unwrap_or_default()).into_owned(),
        )),
        other => err(format!("索引 {index} 不是 NameAndType: {other:?}")),
    }
}

/// 把拼接配方解析成「段」序列。
///
/// 规范里的三个特殊字符：
/// - `\u0001` → 下一个**动态参数**（按顺序取，类型取自调用点描述符）；
/// - `\u0002` → 下一个**静态常量**（`makeConcatWithConstants` 静态参数里配方之后那些）；
/// - 其它字符 → 原样输出（所以「全是字面量」的配方等价于常量折叠，直接拼出来）。
pub(super) fn parse_recipe(
    recipe: &str,
    params: &[String],
    consts: &[TypedSlot],
) -> Result<Vec<Chunk>, String> {
    let mut chunks = Vec::new();
    let mut lit = String::new();
    let mut arg = 0usize;
    let mut konst = 0usize;
    for ch in recipe.chars() {
        match ch {
            '\u{0001}' => {
                if !lit.is_empty() {
                    chunks.push(Chunk::Lit(std::mem::take(&mut lit)));
                }
                let ty = params.get(arg).cloned().ok_or_else(|| {
                    format!(
                        "拼接配方里的第 {} 个动态参数没有对应的调用点参数（描述符里只有 {} 个）",
                        arg + 1,
                        params.len()
                    )
                })?;
                chunks.push(Chunk::Arg(arg, ty));
                arg += 1;
            }
            '\u{0002}' => {
                if !lit.is_empty() {
                    chunks.push(Chunk::Lit(std::mem::take(&mut lit)));
                }
                let ty = consts.get(konst).map(|c| c.0.clone()).ok_or_else(|| {
                    format!(
                        "拼接配方引用了第 {} 个静态常量，但只给了 {} 个",
                        konst + 1,
                        consts.len()
                    )
                })?;
                chunks.push(Chunk::Const(konst, ty));
                konst += 1;
            }
            c => lit.push(c),
        }
    }
    if !lit.is_empty() {
        chunks.push(Chunk::Lit(lit));
    }
    if arg != params.len() {
        return Err(format!(
            "拼接配方用了 {arg} 个动态参数，但调用点描述符有 {} 个",
            params.len()
        ));
    }
    Ok(chunks)
}

/// 描述符 → `MethodType.toString()` 里的类型名。
///
/// JDK 的实现用的是 `Class.getSimpleName()`（`java.lang.String` → `String`）：
/// `MethodType.methodType(int.class, String.class)` 打出来是 `(String)int`。
/// 数组按 `元素名[]`，内部类取 `$` 之后那段。
fn java_type_name(desc: &str) -> String {
    match desc {
        "I" => "int".into(),
        "J" => "long".into(),
        "F" => "float".into(),
        "D" => "double".into(),
        "Z" => "boolean".into(),
        "C" => "char".into(),
        "B" => "byte".into(),
        "S" => "short".into(),
        "V" => "void".into(),
        d if d.starts_with('[') => format!("{}[]", java_type_name(&d[1..])),
        d if d.starts_with('L') && d.ends_with(';') => {
            let inner = &d[1..d.len() - 1];
            let simple = inner.rsplit('/').next().unwrap_or(inner);
            let simple = match simple.rsplit_once('$') {
                Some((_, tail)) if !tail.is_empty() && !tail.chars().all(|c| c.is_ascii_digit()) => tail,
                _ => simple,
            };
            simple.to_string()
        }
        d => d.to_string(),
    }
}

/// 描述符 → 内部类名（`Ljava/lang/Integer;` → `java/lang/Integer`；其它原样）
fn class_of_desc(d: &str) -> &str {
    match d.strip_prefix('L').and_then(|x| x.strip_suffix(';')) {
        Some(inner) => inner,
        None => d,
    }
}

/// 基本类型的**宽化**序（JLS 5.1.2）：`byte→short→int→long→float→double`
fn prim_rank(t: &str) -> u8 {
    match t {
        "B" => 1,
        "S" | "C" => 2,
        "I" => 3,
        "J" => 4,
        "F" => 5,
        "D" => 6,
        _ => 0, // Z（boolean）只能转成自己
    }
}

/// `MethodHandle.asType` 允许的转换（规范：同一类型 / 宽化 / 装箱 / 拆箱 / 引用转换）
pub(super) fn can_convert(from: &str, to: &str) -> bool {
    if from == to {
        return true;
    }
    match (is_prim(from), is_prim(to)) {
        // 基本类型之间：只允许宽化（Z 只到 Z）
        (true, true) => {
            let (a, b) = (prim_rank(from), prim_rank(to));
            a != 0 && b != 0 && a < b
        }
        // 装箱：目标必须是包装类，或它的一个超类型（Object/Number/Comparable/…）
        (true, false) => {
            jdk::wrapper_prim(class_of_desc(to)).is_some()
                || matches!(
                    to,
                    "Ljava/lang/Object;"
                        | "Ljava/lang/Number;"
                        | "Ljava/lang/Comparable;"
                        | "Ljava/io/Serializable;"
                        | "Ljava/lang/constant/Constable;"
                )
        }
        // 拆箱：来源必须是包装类或它的一个超类型
        (false, true) => {
            jdk::wrapper_prim(class_of_desc(from)).is_some()
                || matches!(
                    from,
                    "Ljava/lang/Object;"
                        | "Ljava/lang/Number;"
                        | "Ljava/lang/Comparable;"
                        | "Ljava/io/Serializable;"
                )
        }
        // 引用之间：VM 不做强转，交给运行期（JVM 侧是 checkcast）
        (false, false) => true,
    }
}

/// 把方法描述符拆成 `(参数串, 返回类型)`（参数串不带括号）
fn split_desc(desc: &str) -> (String, String) {
    match desc.find(')') {
        Some(close) => (desc[1..close].to_string(), desc[close + 1..].to_string()),
        None => (String::new(), desc.to_string()),
    }
}

/// 直接方法句柄的**类型**（`MethodHandle.type()`）。
///
/// 三个容易写错的地方，这里一次说清：
/// - `REF_invokeVirtual/Interface/Special` 的句柄**把接收者算作第一个参数**：
///   `String::length` 的句柄类型是 `(Ljava/lang/String;)I`，不是 `()I`；
/// - `REF_newInvokeSpecial` 的返回类型是**类本身**，不是构造器描述符里的 `V`；
/// - `REF_getField/getStatic/putField/putStatic` 的 CP 描述符是**字段类型**（不是方法描述符）。
pub(super) fn handle_type(ref_kind: u8, class: &str, desc: &str) -> String {
    let (params, ret) = split_desc(desc);
    match ref_kind {
        5 | 7 | 9 => format!("(L{class};{params}){ret}"),
        8 => format!("({params})L{class};"),
        1 => format!("(L{class};){desc}"),
        2 => format!("(){desc}"),
        3 => format!("(L{class};{desc})V"),
        4 => format!("({desc})V"),
        _ => format!("({params}){ret}"),
    }
}

fn wrapper_prim_of(class: &str) -> &'static str {
    match jdk::wrapper_prim(class) {
        Some(p) => p,
        None => "I",
    }
}

/// 基本类型之间的转换（`asType` 的数值部分）
fn prim_convert(v: Slot, from: &str, to: &str) -> Slot {
    match (from, to) {
        ("J", "I") => Slot::I(v.as_i64() as i32),
        ("J", "J") => Slot::L(v.as_i64()),
        ("F", "I") => Slot::I(v.as_f32() as i32),
        ("F", "J") => Slot::L(v.as_f32() as i64),
        ("F", "F") => Slot::Fl(v.as_f32()),
        ("F", "D") => Slot::F(v.as_f32() as f64),
        ("D", "I") => Slot::I(v.as_f64() as i32),
        ("D", "J") => Slot::L(v.as_f64() as i64),
        ("D", "F") => Slot::Fl(v.as_f64() as f32),
        ("D", "D") => Slot::F(v.as_f64()),
        ("I", "I") | ("I", "B") | ("I", "C") | ("I", "S") | ("I", "Z") => Slot::I(v.as_i32()),
        ("I", "J") => Slot::L(v.as_i32() as i64),
        ("I", "F") => Slot::Fl(v.as_i32() as f32),
        ("I", "D") => Slot::F(v.as_i32() as f64),
        (_, "J") => Slot::L(v.as_i64()),
        (_, "D") => Slot::F(v.as_f64()),
        (_, "F") => Slot::Fl(v.as_f32()),
        _ => Slot::I(v.as_i32()),
    }
}

/// `Vm` 里 `invokedynamic` 用到的 side table 与缓存的**类型清单**（字段本身定义在 `mod.rs`）。
///
/// 为什么不塞进对象字段：句柄/调用点的语义是 VM 的，不是 Java 对象的；side table
/// 让 `getClass()`/`instanceof` 还能照常看到 `java.lang.invoke.MethodHandle` 这个类。
/// 这里只放文档与类型别名，真正的字段在 `Vm` 上（与其它堆表保持同一处）。
pub(super) type BsmTable = Arc<Vec<crate::classfile::attr::BootstrapMethod>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_descriptors() {
        assert_eq!(param_descs("(II)I"), vec!["I", "I"]);
        assert_eq!(param_descs("([Ljava/lang/String;)V"), vec!["[Ljava/lang/String;"]);
        assert_eq!(param_descs("(JD)D"), vec!["J", "D"]);
        assert_eq!(param_descs("()V"), Vec::<String>::new());
        assert_eq!(ret_desc("(I)Ljava/lang/String;"), "Ljava/lang/String;");
        assert_eq!(ret_desc("(I)V"), "V");
        assert_eq!(ret_desc("()[I"), "[I");
        assert_eq!(prefix_desc("I", "(D)J"), "(ID)J");
    }

    #[test]
    fn converts_primitives() {
        assert_eq!(prim_convert(Slot::I(3), "I", "J"), Slot::L(3));
        assert_eq!(prim_convert(Slot::L(-3), "J", "I"), Slot::I(-3));
        assert_eq!(prim_convert(Slot::F(2.9), "D", "I"), Slot::I(2));
        assert_eq!(prim_convert(Slot::I(2), "I", "D"), Slot::F(2.0));
    }

    /// 直接句柄的类型：接收者算第一个参数、构造器返回类本身、字段是字段类型
    #[test]
    fn handle_types_include_receiver() {
        // REF_invokeVirtual：`String::length` 是 `(String)I`，不是 `()I`
        assert_eq!(handle_type(5, "java/lang/String", "()I"), "(Ljava/lang/String;)I");
        // REF_invokeStatic：原样
        assert_eq!(handle_type(6, "I7", "(II)I"), "(II)I");
        // REF_newInvokeSpecial：返回类本身
        assert_eq!(handle_type(8, "I4", "(I)V"), "(I)LI4;");
        // 字段句柄：CP 里是字段类型
        assert_eq!(handle_type(1, "C", "I"), "(LC;)I");
        assert_eq!(handle_type(2, "C", "I"), "()I");
        assert_eq!(handle_type(3, "C", "I"), "(LC;I)V");
        assert_eq!(handle_type(4, "C", "I"), "(I)V");
    }

    #[test]
    fn parses_concat_recipes() {
        let params = vec!["I".to_string(), "Ljava/lang/String;".to_string()];
        let consts = vec![("I".to_string(), Slot::I(7))];
        // 字面量 + 动态参数 + 常量，顺序不能乱
        assert_eq!(
            parse_recipe("a\u{1}b\u{2}c", &params[..1], &consts).unwrap(),
            vec![
                Chunk::Lit("a".into()),
                Chunk::Arg(0, "I".into()),
                Chunk::Lit("b".into()),
                Chunk::Const(0, "I".into()),
                Chunk::Lit("c".into()),
            ]
        );
        // 两个动态参数按顺序取
        assert_eq!(
            parse_recipe("\u{1}=\u{1}", &params, &[]).unwrap(),
            vec![
                Chunk::Arg(0, "I".into()),
                Chunk::Lit("=".into()),
                Chunk::Arg(1, "Ljava/lang/String;".into()),
            ]
        );
        // 纯字面量（等价于常量折叠）
        assert_eq!(parse_recipe("a1b", &[], &[]).unwrap(), vec![Chunk::Lit("a1b".into())]);
        // 参数比配方少 / 常量比配方少：都必须报错（不能悄悄输出错字符串）
        assert!(parse_recipe("\u{1}\u{1}", &params[..1], &[]).is_err());
        assert!(parse_recipe("\u{2}", &[], &[]).is_err());
        // 配方比参数多也不算错（多余参数由 JVM 侧保证不会发生）；但少了必须报
        assert!(parse_recipe("", &params, &[]).is_err());
    }

    /// `asType` 允许的转换：宽化/装箱/拆箱/引用，**窄化不允许**
    #[test]
    fn as_type_conversions() {
        assert!(can_convert("I", "J")); // 宽化
        assert!(can_convert("I", "D"));
        assert!(can_convert("J", "D"));
        assert!(!can_convert("J", "I")); // 窄化：asType 不支持
        assert!(!can_convert("D", "I"));
        assert!(!can_convert("Z", "I")); // boolean 只能是自己
        assert!(can_convert("I", "Ljava/lang/Object;")); // 装箱
        assert!(can_convert("I", "Ljava/lang/Integer;"));
        assert!(!can_convert("I", "Ljava/lang/String;"));
        assert!(can_convert("Ljava/lang/Object;", "I")); // 拆箱
        assert!(can_convert("Ljava/lang/Number;", "J"));
        assert!(can_convert("Ljava/lang/String;", "Ljava/lang/Object;")); // 引用
    }

    #[test]
    fn method_type_to_string_uses_simple_names() {        assert_eq!(java_type_name("I"), "int");
        assert_eq!(java_type_name("Ljava/lang/String;"), "String");
        assert_eq!(java_type_name("[Ljava/lang/String;"), "String[]");
        assert_eq!(java_type_name("[I"), "int[]");
        assert_eq!(java_type_name("LI9$Inner;"), "Inner");
        assert_eq!(java_type_name("V"), "void");
    }
}
