//! 方法级字节码验证器（JVMS 4.10.1）。
//!
//! 没有这一层的时候，**我们自己的编译器产出的非法 class 会被我们自己的 VM 照跑**：
//! `R4.class` 的操作数栈需求超过 `max_stack`（HotSpot：`VerifyError: Operand stack
//! overflow @49: i2l`），解释器和 JIT 两条腿都算出了「正确」的答案 —— 错的字节码
//! 碰巧算对了，换一份输入就崩。所以这里是三条验收腿里前两条的**当场报警探针**。
//!
//! ## 做什么
//!
//! 进入方法体之前跑一次，按 `(ClassFile, 方法下标)` 缓存（只在第一次执行时算）：
//!
//! 1. 指令解码（含 `wide` / `tableswitch` / `lookupswitch`）；跳转目标必须是指令边界
//! 2. 栈高度与类型的数据流：每条指令的净栈效应（`long`/`double` 占 2 槽），
//!    `max_stack` / `max_locals` 越界即拒
//! 3. 局部变量与操作数栈的**类型汇合**：按前驱求最小上界（`Top` 与任何类型汇合得
//!    `Top`；不同引用类型汇合得共同父类）
//! 4. 类型规则：`xload`/`xstore` 与槽位类型、`invoke*` 的接收者与参数与描述符、
//!    `getfield` 的接收者非 `Top`、`aaload`/`aastore` 的元素类型、`new` → `<init>` 之前不许用
//! 5. `StackMapTable` 存在时核对：声明的帧必须是数据流算出来的状态的**上界**
//!    （JVMS 4.10.1.2 `frameIsAssignable` 的方向，不是相等 —— javac 有时声明得比
//!    实际流更宽）
//!
//! ## 不做什么（宁可放过，不可误杀）
//!
//! - `jsr`/`ret`/`jsr_w`：遇到过就整方法跳过（JDK 6 之后没有编译器再产这个）
//! - **子类型加载**：`A` 能否赋给 `B` 只在两边的类层次都能解析时给结论，解析不到就
//!   当「未知 → 放过」。独立模式（`rustjava`）下 `java.*` 没有 class 文件，
//!   `String <: CharSequence` 这种根本没法判，硬判会误杀合法代码。
//! - 不查「`new` 的是不是接口/抽象类」（那是运行时的 `InstantiationError`，不是验证错误）
//!
//! 调试开关：`VMLESS_VERIFY=0` 关掉校验（只为定位「验证器误杀」，别当默认值）；
//! `VMLESS_VERIFY_DEBUG=1` 把每个方法的验证结论打到 stderr。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::{ClassFile, CpEntry, Member};

use super::Vm;

// ---------------------------------------------------------------- 类型格

/// 验证用类型。`long`/`double` 在槽位里占两个：第二个固定是 `Top`。
#[derive(Clone, PartialEq, Eq, Debug)]
enum Ty {
    /// 不可用槽位（未赋值 / cat-2 的后半 / 汇合失败）
    Top,
    Int,
    Float,
    Long,
    Double,
    /// `null` 字面量：可赋给任何引用类型
    Null,
    /// 引用类型。类名是 `java/lang/String` 这种内部形式；数组是 `[I` / `[Ljava/lang/String;`
    Ref(Arc<str>),
    /// `new` 之后、`<init>` 之前（带 `new` 的 bci）
    Uninit(u16),
    /// 构造器里的 `this`，调父类构造器之前
    UninitThis,
}

impl Ty {
    fn cat2(&self) -> bool {
        matches!(self, Ty::Long | Ty::Double)
    }
    fn is_ref(&self) -> bool {
        matches!(self, Ty::Ref(_) | Ty::Null)
    }
    fn is_uninit(&self) -> bool {
        matches!(self, Ty::Uninit(_) | Ty::UninitThis)
    }
    fn show(&self) -> String {
        match self {
            Ty::Top => "Top".into(),
            Ty::Int => "int".into(),
            Ty::Float => "float".into(),
            Ty::Long => "long".into(),
            Ty::Double => "double".into(),
            Ty::Null => "null".into(),
            Ty::Ref(s) => s.replace('/', "."),
            Ty::Uninit(b) => format!("uninitialized(@{b})"),
            Ty::UninitThis => "uninitializedThis".into(),
        }
    }
}

fn ty_ref(name: &str) -> Ty {
    Ty::Ref(Arc::from(name))
}

/// 一组槽位：局部变量表或操作数栈。`stack.len()` 就是占用的槽数。
#[derive(Clone, PartialEq, Debug)]
struct St {
    locals: Vec<Ty>,
    stack: Vec<Ty>,
}

impl St {
    fn push(&mut self, t: Ty) {
        if t.cat2() {
            self.stack.push(t);
            self.stack.push(Ty::Top);
        } else {
            self.stack.push(t);
        }
    }

    /// 弹一个**值**：返回 `(类型, 是否 cat-2)`。栈上的 `Top` 只可能是 cat-2 的后半。
    fn pop_val(&mut self) -> Result<(Ty, bool), String> {
        match self.stack.pop() {
            None => Err("操作数栈下溢".into()),
            Some(Ty::Top) => match self.stack.pop() {
                Some(t @ (Ty::Long | Ty::Double)) => Ok((t, true)),
                Some(other) => Err(format!("操作数栈损坏：{} 后面跟着 Top", other.show())),
                None => Err("操作数栈下溢（cat-2 只弹到一半）".into()),
            },
            Some(t) => Ok((t, false)),
        }
    }

    fn push_val(&mut self, t: Ty, cat2: bool) {
        if cat2 {
            self.stack.push(t);
            self.stack.push(Ty::Top);
        } else {
            self.stack.push(t);
        }
    }

    /// `<init>` 调用之后：把全部 `which` 换成正式类型
    fn replace_uninit(&mut self, which: &Ty, class: &Arc<str>) {
        let repl = Ty::Ref(Arc::clone(class));
        for t in self.locals.iter_mut().chain(self.stack.iter_mut()) {
            if t == which {
                *t = repl.clone();
            }
        }
    }
}

// ---------------------------------------------------------------- Code 属性

struct Exc {
    start: usize,
    end: usize,
    handler: usize,
    /// `None` = catch-all（`finally` 的 any handler）
    ty: Option<Arc<str>>,
}

struct CodeAttr<'a> {
    max_stack: usize,
    max_locals: usize,
    code: &'a [u8],
    exc: Vec<Exc>,
    /// `StackMapTable` 的原始 info 字节（不含属性头）
    frames: Option<&'a [u8]>,
}

fn be2(b: &[u8], i: usize) -> Option<usize> {
    Some(u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]) as usize)
}

fn be4(b: &[u8], i: usize) -> Option<usize> {
    Some(u32::from_be_bytes([*b.get(i)?, *b.get(i + 1)?, *b.get(i + 2)?, *b.get(i + 3)?]) as usize)
}

/// 常量池里某个名字的 Utf8 下标（属性名比较用）
fn utf8_index(cf: &ClassFile, name: &str) -> Option<u16> {
    cf.utf8_index(name.as_bytes())
}

fn code_attr<'a>(cf: &'a ClassFile, m: &'a Member) -> Option<CodeAttr<'a>> {
    let code_name = utf8_index(cf, "Code")?;
    let smt_name = utf8_index(cf, "StackMapTable");
    for a in &m.attributes {
        if a.name_index != code_name || a.info.len() < 8 {
            continue;
        }
        let info: &'a [u8] = &a.info;
        let max_stack = be2(info, 0)?;
        let max_locals = be2(info, 2)?;
        let code_len = be4(info, 4)?;
        if 8 + code_len > info.len() {
            return Some(CodeAttr {
                max_stack,
                max_locals,
                code: &info[8..],
                exc: Vec::new(),
                frames: None,
            });
        }
        let code = &info[8..8 + code_len];
        let mut q = 8 + code_len;
        let mut exc = Vec::new();
        if let Some(n) = be2(info, q) {
            q += 2;
            for _ in 0..n {
                let (Some(s), Some(e), Some(h), Some(t)) = (
                    be2(info, q),
                    be2(info, q + 2),
                    be2(info, q + 4),
                    be2(info, q + 6),
                ) else {
                    break;
                };
                let mut ent = Exc { start: s, end: e, handler: h, ty: None };
                if t != 0 {
                    if let Some(nm) = class_at(cf, t as u16) {
                        ent.ty = Some(nm);
                    }
                }
                exc.push(ent);
                q += 8;
            }
        }
        // Code 自己的属性表：找 StackMapTable
        let mut frames = None;
        if let (Some(n), Some(smt)) = (be2(info, q), smt_name) {
            q += 2;
            for _ in 0..n {
                let (Some(ni), Some(len)) = (be2(info, q), be4(info, q + 2)) else { break };
                let start = q + 6;
                if start + len > info.len() {
                    break;
                }
                if ni == smt as usize {
                    frames = Some(&info[start..start + len]);
                }
                q = start + len;
            }
        }
        return Some(CodeAttr { max_stack, max_locals, code, exc, frames });
    }
    None
}

// ---------------------------------------------------------------- 常量池辅助

fn class_at(cf: &ClassFile, idx: u16) -> Option<Arc<str>> {
    match cf.constant_pool.get(idx as usize).and_then(|e| e.as_ref()) {
        Some(CpEntry::Class(n)) => {
            Some(Arc::from(String::from_utf8_lossy(cf.utf8(*n)?).into_owned()))
        }
        _ => None,
    }
}

/// `Fieldref` / `Methodref` / `InterfaceMethodref` → `(类名, 名字, 描述符)`
fn member_at(cf: &ClassFile, idx: u16) -> Option<(Arc<str>, String, String)> {
    let (c, nt) = match cf.constant_pool.get(idx as usize).and_then(|e| e.as_ref())? {
        CpEntry::FieldRef(c, nt) | CpEntry::MethodRef(c, nt) | CpEntry::InterfaceMethodRef(c, nt) => {
            (*c, *nt)
        }
        _ => return None,
    };
    let class = class_at(cf, c)?;
    let (n, d) = match cf.constant_pool.get(nt as usize).and_then(|e| e.as_ref())? {
        CpEntry::NameAndType(n, d) => (*n, *d),
        _ => return None,
    };
    Some((
        class,
        String::from_utf8_lossy(cf.utf8(n)?).into_owned(),
        String::from_utf8_lossy(cf.utf8(d)?).into_owned(),
    ))
}

/// 描述符 → 类型（返回 `(类型, 消耗的字节数)`）
fn parse_ty(desc: &[u8], i: usize) -> Option<(Ty, usize)> {
    let c = *desc.get(i)?;
    Some(match c {
        b'B' | b'C' | b'I' | b'S' | b'Z' => (Ty::Int, 1),
        b'F' => (Ty::Float, 1),
        b'J' => (Ty::Long, 1),
        b'D' => (Ty::Double, 1),
        b'L' => {
            let end = desc[i..].iter().position(|&b| b == b';')? + i;
            (ty_ref(&String::from_utf8_lossy(&desc[i + 1..end])), end + 1 - i)
        }
        b'[' => {
            let mut j = i;
            while *desc.get(j)? == b'[' {
                j += 1;
            }
            let (_, used) = parse_ty(desc, j)?;
            (ty_ref(&String::from_utf8_lossy(&desc[i..j + used])), j + used - i)
        }
        _ => return None,
    })
}

/// 方法描述符 `(II)I` → (参数类型, 返回类型)
fn parse_method_desc(desc: &str) -> Option<(Vec<Ty>, Option<Ty>)> {
    let b = desc.as_bytes();
    if b.first()? != &b'(' {
        return None;
    }
    let mut i = 1;
    let mut args = Vec::new();
    while *b.get(i)? != b')' {
        let (t, used) = parse_ty(b, i)?;
        args.push(t);
        i += used;
    }
    i += 1;
    if i >= b.len() {
        return None;
    }
    if b[i] == b'V' {
        Some((args, None))
    } else {
        let (t, _) = parse_ty(b, i)?;
        Some((args, Some(t)))
    }
}

// ---------------------------------------------------------------- 指令长度

/// 定长指令的长度（`tableswitch` / `lookupswitch` / `wide` 单独处理）
fn fixed_len(op: u8) -> Option<usize> {
    Some(match op {
        0x00..=0x0f => 1, // nop … dconst_1
        0x10 => 2,        // bipush
        0x11 => 3,        // sipush
        0x12 => 2,        // ldc
        0x13 | 0x14 => 3, // ldc_w / ldc2_w
        0x15..=0x19 => 2, // iload … aload
        0x1a..=0x35 => 1, // xload_n / xaload
        0x36..=0x3a => 2, // istore … astore
        0x3b..=0x83 => 1, // xstore_n / 算术 / 转换
        0x84 => 3,        // iinc
        0x85..=0x98 => 1, // 转换 / 比较
        0x99..=0xa8 => 3, // if* / goto / jsr
        0xa9 => 2,        // ret
        0xac..=0xb1 => 1, // xreturn
        0xb2..=0xb8 => 3, // getstatic … invokestatic
        0xb9 | 0xba => 5, // invokeinterface / invokedynamic
        0xbb => 3,        // new
        0xbc => 2,        // newarray
        0xbd => 3,        // anewarray
        0xbe | 0xbf => 1, // arraylength / athrow
        0xc0 | 0xc1 => 3, // checkcast / instanceof
        0xc2 | 0xc3 => 1, // monitorenter / monitorexit
        0xc5 => 4,        // multianewarray
        0xc6 | 0xc7 => 3, // ifnull / ifnonnull
        0xc8 | 0xc9 => 5, // goto_w / jsr_w
        _ => return None,
    })
}

fn i16_at(code: &[u8], i: usize) -> Option<i32> {
    Some(i16::from_be_bytes([*code.get(i)?, *code.get(i + 1)?]) as i32)
}

fn i32_at(code: &[u8], i: usize) -> Option<i32> {
    Some(i32::from_be_bytes([
        *code.get(i)?,
        *code.get(i + 1)?,
        *code.get(i + 2)?,
        *code.get(i + 3)?,
    ]))
}

/// `pc` 处指令的长度；`pc` 必须是一个指令起点
fn insn_len(code: &[u8], pc: usize) -> Result<usize, String> {
    let op = *code.get(pc).ok_or_else(|| format!("@{pc}: 越界"))?;
    match op {
        0xaa => {
            // tableswitch: opcode + 0..3 填充 + default(4) + low(4) + high(4) + (high-low+1)*4
            let mut q = pc + 1;
            while q % 4 != 0 {
                q += 1;
            }
            let low = i32_at(code, q + 4).ok_or_else(|| format!("@{pc}: tableswitch 截断"))?;
            let high = i32_at(code, q + 8).ok_or_else(|| format!("@{pc}: tableswitch 截断"))?;
            let n = (high as i64 - low as i64 + 1).max(0) as usize;
            Ok(q + 12 + n * 4 - pc)
        }
        0xab => {
            let mut q = pc + 1;
            while q % 4 != 0 {
                q += 1;
            }
            let npairs = i32_at(code, q + 4).ok_or_else(|| format!("@{pc}: lookupswitch 截断"))?;
            let n = npairs.max(0) as usize;
            Ok(q + 8 + n * 8 - pc)
        }
        0xc4 => {
            // wide <op> index(u2) [const(u2)，仅 iinc]
            let sub = *code.get(pc + 1).ok_or_else(|| format!("@{pc}: wide 截断"))?;
            Ok(if sub == 0x84 { 6 } else { 4 })
        }
        0xfe | 0xff => Err(format!("@{pc}: 保留指令 0x{op:02x}")),
        0xca => Err(format!("@{pc}: breakpoint 不是合法字节码")),
        _ => fixed_len(op).ok_or_else(|| format!("@{pc}: 未知/保留操作码 0x{op:02x}")),
    }
}

fn opname(op: u8) -> String {
    match op {
        0x15 => "iload".into(),
        0x16 => "lload".into(),
        0x17 => "fload".into(),
        0x18 => "dload".into(),
        0x19 => "aload".into(),
        0x36 => "istore".into(),
        0x37 => "lstore".into(),
        0x38 => "fstore".into(),
        0x39 => "dstore".into(),
        0x3a => "astore".into(),
        0x59 => "dup".into(),
        0x5a => "dup_x1".into(),
        0x5b => "dup_x2".into(),
        0x5c => "dup2".into(),
        0x5d => "dup2_x1".into(),
        0x5e => "dup2_x2".into(),
        0x5f => "swap".into(),
        0x84 => "iinc".into(),
        0xa7 => "goto".into(),
        0xa8 => "jsr".into(),
        0xa9 => "ret".into(),
        0xaa => "tableswitch".into(),
        0xab => "lookupswitch".into(),
        0xb2 => "getstatic".into(),
        0xb3 => "putstatic".into(),
        0xb4 => "getfield".into(),
        0xb5 => "putfield".into(),
        0xb6 => "invokevirtual".into(),
        0xb7 => "invokespecial".into(),
        0xb8 => "invokestatic".into(),
        0xb9 => "invokeinterface".into(),
        0xba => "invokedynamic".into(),
        0xbb => "new".into(),
        0xbc => "newarray".into(),
        0xbd => "anewarray".into(),
        0xbe => "arraylength".into(),
        0xbf => "athrow".into(),
        0xc0 => "checkcast".into(),
        0xc1 => "instanceof".into(),
        0xc2 => "monitorenter".into(),
        0xc3 => "monitorexit".into(),
        0xc5 => "multianewarray".into(),
        0xc6 => "ifnull".into(),
        0xc7 => "ifnonnull".into(),
        0xc8 => "goto_w".into(),
        0xc9 => "jsr_w".into(),
        _ => format!("op 0x{op:02x}"),
    }
}

// ---------------------------------------------------------------- 入口与缓存

type Key = (usize, usize);

/// 已验证的方法 → 钉住那份 `ClassFile`，避免指针被复用后错认（与 `method_refs` 同理）
static OK: OnceLock<Mutex<HashMap<Key, Arc<ClassFile>>>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<Key, Arc<ClassFile>>> {
    OK.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 最近一次验证成功的方法（线程本地，**热路径**用）。
///
/// `execute` 每次调用都会问一次「这个方法验过吗」。真的每次都去抢全局表的锁 + 哈希，
/// 在「16M 次虚调用」这种极端调用密集的负载上能量出 2~7% 的退化。这里只存一个
/// `(ClassFile 指针, 方法下标)`：同一方法被反复调用时命中率极高，命中的成本是
/// 一次 thread-local 读 + 一次比较。
///
/// 指针不会失效：全局表里那份 `Arc<ClassFile>` 是**永久的**，地址不会被别的类复用。
thread_local! {
    static LAST: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, usize::MAX)) };
}

fn cached(cf: &Arc<ClassFile>, method: usize) -> bool {
    let ptr = Arc::as_ptr(cf) as usize;
    if LAST.with(|c| c.get()) == (ptr, method) {
        return true;
    }
    let key = (ptr, method);
    let ok = match cache().lock() {
        Ok(g) => matches!(g.get(&key), Some(c) if Arc::ptr_eq(c, cf)),
        Err(_) => false,
    };
    if ok {
        LAST.with(|c| c.set((ptr, method)));
    }
    ok
}

fn mark_ok(cf: &Arc<ClassFile>, method: usize) {
    let key = (Arc::as_ptr(cf) as usize, method);
    if let Ok(mut g) = cache().lock() {
        g.insert(key, Arc::clone(cf));
    }
    LAST.with(|c| c.set(key));
}

/// 这个方法验证过了吗；没验证过就现在验证一次（结果缓存）。
///
/// 返回 `Err(原因)` 表示字节码非法 —— 调用方负责抛 `java.lang.VerifyError`。
pub(super) fn ensure_verified(vm: &Vm, cf: &Arc<ClassFile>, method: usize) -> Result<(), String> {
    if cached(cf, method) {
        return Ok(());
    }
    let Some(m) = cf.methods.get(method) else { return Ok(()) };
    // 抽象 / native 方法没有字节码
    if m.access_flags & 0x0400 != 0 || m.access_flags & 0x0100 != 0 {
        mark_ok(cf, method);
        return Ok(());
    }
    // 只有第一次（缓存未命中）才读环境变量：热路径上不能有一次 getenv
    let off = std::env::var_os("VMLESS_VERIFY").map(|v| v == "0").unwrap_or(false);
    if off {
        mark_ok(cf, method);
        return Ok(());
    }
    let desc = String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default()).into_owned();
    let name = String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default()).into_owned();
    let owner = cf.this_class_name().unwrap_or_default();
    match verify_method(Some(vm), cf, m, &desc, &name) {
        Ok(outcome) => {
            if std::env::var_os("VMLESS_VERIFY_DEBUG").is_some() {
                match &outcome {
                    Outcome::Checked => eprintln!("[verify] OK  {owner}.{name}{desc}"),
                    Outcome::Skipped(w) => eprintln!("[verify] SKIP {owner}.{name}{desc}: {w}"),
                }
            }
            mark_ok(cf, method);
            Ok(())
        }
        Err(why) => {
            if std::env::var_os("VMLESS_VERIFY_DEBUG").is_some() {
                eprintln!("[verify] FAIL {owner}.{name}{desc}: {why}");
            }
            Err(format!("{owner}.{name}{desc}: {why}"))
        }
    }
}

/// 整个类一次验完 —— 给**不经过 `Vm::execute`** 的入口用（`run_jit` 直接编机器码）。
///
/// 没有 `Vm` 就没有类层次：`String <: CharSequence` 这类判定一律「未知 → 放过」，
/// 但栈高 / 槽位类型 / 跳转边界这些**不依赖层次**的规则照样生效，
/// `R4`（`max_stack` 不够）就是死在这里。
pub fn verify_class(cf: &ClassFile) -> Result<(), String> {
    for (i, m) in cf.methods.iter().enumerate() {
        if m.access_flags & 0x0400 != 0 || m.access_flags & 0x0100 != 0 {
            continue;
        }
        let desc = String::from_utf8_lossy(cf.utf8(m.descriptor_index).unwrap_or_default()).into_owned();
        let name = String::from_utf8_lossy(cf.utf8(m.name_index).unwrap_or_default()).into_owned();
        let owner = cf.this_class_name().unwrap_or_default();
        if let Err(why) = verify_method(None, cf, m, &desc, &name) {
            return Err(format!("{owner}.{name}{desc}: {why}"));
        }
        let _ = i;
    }
    Ok(())
}

/// 验证结论：真正验过 / 因为不支持而跳过
enum Outcome {
    Checked,
    Skipped(&'static str),
}

fn verify_method(
    vm: Option<&Vm>,
    cf: &ClassFile,
    m: &Member,
    desc: &str,
    name: &str,
) -> Result<Outcome, String> {
    let ca = code_attr(cf, m).ok_or_else(|| "没有 Code 属性".to_string())?;
    if ca.code.is_empty() {
        return Ok(Outcome::Skipped("空方法体"));
    }
    // 指令起点表（末尾多一格，方便判「跳转目标 == code_len」）
    let n = ca.code.len();
    let mut starts = vec![false; n + 1];
    let mut p = 0usize;
    while p < n {
        starts[p] = true;
        let len = insn_len(ca.code, p)?;
        if len == 0 || p + len > n {
            return Err(format!("@{p}: 指令长度 {len} 越过 code 末尾"));
        }
        // jsr/ret：整方法跳过（不受支持，但也不想误杀）
        match ca.code[p] {
            0xa8 | 0xa9 | 0xc9 => return Ok(Outcome::Skipped("jsr/ret")),
            _ => {}
        }
        p += len;
    }
    if p != n {
        return Err(format!("@{p}: 指令在 code 末尾被截断"));
    }
    let v = Vf {
        vm,
        cf,
        code: ca.code,
        starts,
        max_stack: ca.max_stack,
        max_locals: ca.max_locals,
        is_static: m.access_flags & 0x0008 != 0,
        is_ctor: name == "<init>",
    };
    v.run(&ca, desc)?;
    Ok(Outcome::Checked)
}

// ---------------------------------------------------------------- 验证器主体

struct Vf<'a> {
    /// JIT 侧（`run_jit` 从来不建 Vm）为 `None`：此时类层次一律「未知」，只做不依赖
    /// 层次的检查（栈高、槽位类型、跳转边界…）。解释器侧总是 `Some`。
    vm: Option<&'a Vm>,
    cf: &'a ClassFile,
    code: &'a [u8],
    starts: Vec<bool>,
    max_stack: usize,
    max_locals: usize,
    is_static: bool,
    is_ctor: bool,
}

impl<'a> Vf<'a> {
    fn run(&self, ca: &CodeAttr<'_>, desc: &str) -> Result<(), String> {
        // 注意：`max_locals == 0` 是**合法**的（空 `<clinit>` 只有一条 `return`）——
        // 需要在下面按实际用到的槽数判越界，不能在这里一刀切。
        // ---- 入口状态：this + 参数 ----
        // `params` 是**声明用的**局部变量列表（只有 this + 参数，不补到 max_locals）：
        // StackMapTable 的 same/append/chop 帧都是从这份列表上增删的。
        let (args, _ret) = parse_method_desc(desc).ok_or_else(|| format!("描述符解析失败: {desc}"))?;
        let mut init = St { locals: vec![Ty::Top; self.max_locals], stack: Vec::new() };
        let mut params: Vec<Ty> = Vec::with_capacity(self.max_locals);
        let mut slot = 0usize;
        if !self.is_static {
            let this = self.cf.this_class_name().unwrap_or_default();
            let t = if self.is_ctor { Ty::UninitThis } else { ty_ref(&this) };
            self.set_local(&mut init.locals, slot, &t)?;
            params.push(t);
            slot += 1;
        }
        for a in &args {
            self.set_local(&mut init.locals, slot, a)?;
            params.push(a.clone());
            if a.cat2() {
                params.push(Ty::Top);
            }
            slot += if a.cat2() { 2 } else { 1 };
        }
        if slot > self.max_locals {
            return Err(format!(
                "参数需要 {slot} 个局部变量槽，超过 max_locals={}",
                self.max_locals
            ));
        }

        // ---- 异常表偏移校验 ----
        for e in &ca.exc {
            for t in [e.start, e.handler] {
                if t >= self.code.len() || !self.starts[t] {
                    return Err(format!("异常表里的偏移 {t} 不是指令边界"));
                }
            }
            if e.end > self.code.len() {
                return Err(format!("异常表 end={} 越过 code 末尾", e.end));
            }
        }

        // ---- 数据流 ----
        let mut states: Vec<Option<St>> = vec![None; self.code.len()];
        states[0] = Some(init.clone());
        let mut work: Vec<usize> = vec![0];
        let mut steps = 0usize;
        while let Some(pc) = work.pop() {
            steps += 1;
            if steps > 4_000_000 {
                return Err("数据流不收敛（可能的非法控制流）".into());
            }
            let pre = states[pc].clone().ok_or_else(|| format!("@{pc}: 内部错误（无状态）"))?;
            let mut st = pre.clone();
            let succ = self.transfer(pc, &mut st)?;
            // 异常边：这条指令在某个 try 范围内时，handler 收到 (pre.locals, [异常类型])
            for e in &ca.exc {
                if pc >= e.start && pc < e.end {
                    let mut hs = St { locals: pre.locals.clone(), stack: Vec::new() };
                    hs.stack.push(match &e.ty {
                        Some(t) => Ty::Ref(Arc::clone(t)),
                        None => ty_ref("java/lang/Throwable"),
                    });
                    self.feed(&mut states, &mut work, e.handler, hs)?;
                }
            }
            for t in succ {
                self.feed(&mut states, &mut work, t, st.clone())?;
            }
        }

        // ---- StackMapTable 核对 ----
        if let Some(fr) = ca.frames {
            self.check_frames(fr, &states, &params)?;
        }
        Ok(())
    }

    fn set_local(&self, locals: &mut [Ty], slot: usize, t: &Ty) -> Result<(), String> {
        let need = if t.cat2() { 2 } else { 1 };
        if slot + need > locals.len() {
            return Err(format!("局部变量下标 {slot} 超出 max_locals={}", self.max_locals));
        }
        locals[slot] = t.clone();
        if need == 2 {
            locals[slot + 1] = Ty::Top;
        }
        Ok(())
    }

    fn local(&self, locals: &[Ty], i: usize) -> Result<Ty, String> {
        if i >= self.max_locals {
            return Err(format!("局部变量下标 {i} 超出 max_locals={}", self.max_locals));
        }
        Ok(locals[i].clone())
    }

    /// 把一个前驱状态并进 `pc`
    fn feed(
        &self,
        states: &mut [Option<St>],
        work: &mut Vec<usize>,
        pc: usize,
        incoming: St,
    ) -> Result<(), String> {
        if pc >= self.code.len() {
            return Err(format!("跳转目标 {pc} 越过 code 末尾"));
        }
        if incoming.stack.len() > self.max_stack {
            return Err(format!(
                "@{pc}: 操作数栈需要 {} 槽，超过 max_stack={}",
                incoming.stack.len(),
                self.max_stack
            ));
        }
        match &states[pc] {
            None => {
                states[pc] = Some(incoming);
                work.push(pc);
            }
            Some(old) => {
                if old.stack.len() != incoming.stack.len() {
                    return Err(format!(
                        "@{pc}: 汇合点的操作数栈高度不一致（{} vs {}）",
                        old.stack.len(),
                        incoming.stack.len()
                    ));
                }
                let locals = self.merge_locals(&old.locals, &incoming.locals);
                let mut stack = Vec::with_capacity(old.stack.len());
                for (a, b) in old.stack.iter().zip(incoming.stack.iter()) {
                    stack.push(self.lub(a, b));
                }
                if locals != old.locals || stack != old.stack {
                    states[pc] = Some(St { locals, stack });
                    work.push(pc);
                }
            }
        }
        Ok(())
    }

    fn merge_locals(&self, a: &[Ty], b: &[Ty]) -> Vec<Ty> {
        let n = a.len().max(b.len());
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let x = a.get(i).cloned().unwrap_or(Ty::Top);
            let y = b.get(i).cloned().unwrap_or(Ty::Top);
            out.push(self.lub(&x, &y));
        }
        out
    }

    /// 最小上界（汇合用）
    fn lub(&self, a: &Ty, b: &Ty) -> Ty {
        if a == b {
            return a.clone();
        }
        match (a, b) {
            (Ty::Null, r) | (r, Ty::Null) if r.is_ref() => r.clone(),
            (Ty::Ref(x), Ty::Ref(y)) => Ty::Ref(self.lub_ref(x, y)),
            _ => Ty::Top,
        }
    }

    fn lub_ref(&self, x: &Arc<str>, y: &Arc<str>) -> Arc<str> {
        if x == y {
            return Arc::clone(x);
        }
        let (xa, ya) = (x.starts_with('['), y.starts_with('['));
        if xa && ya {
            let (ex, ey) = (elem_of(x), elem_of(y));
            if ex.len() == 1 || ey.len() == 1 {
                return Arc::from("java/lang/Object");
            }
            let e = self.lub_ref(&Arc::from(ex), &Arc::from(ey));
            return Arc::from(format!("[{}", norm_arr_elem(&e)).as_str());
        }
        if xa != ya {
            return Arc::from("java/lang/Object");
        }
        if !self.known(x) || !self.known(y) {
            return Arc::from("java/lang/Object");
        }
        let Some(vm) = self.vm else { return Arc::from("java/lang/Object") };
        // 1) 共同父类。**先跳过 Object**：若 y 是接口，共同父类只会是 Object，
        //    而真正的上界往往是某个共同接口（`HashMap.EntrySet` 与 `Set` 的并
        //    是 `Set` 而不是 `Object`）—— 早退到 Object 会让后面所有帧核对误杀。
        let mut cur: String = (**x).to_string();
        loop {
            if &*cur != "java/lang/Object" && vm.is_subclass(y, &cur) {
                return Arc::from(cur.as_str());
            }
            match vm.store.super_of(&cur).filter(|s| !s.is_empty()) {
                Some(s) => cur = s,
                None => break,
            }
        }
        // 2) 共同接口
        for i in vm.store.iface_closure(x) {
            if vm.is_subclass(y, &i) {
                return Arc::from(i.as_str());
            }
        }
        Arc::from("java/lang/Object")
    }

    fn known(&self, n: &str) -> bool {
        n == "java/lang/Object"
            || super::class::builtin_super(n).is_some()
            || self.vm.map_or(false, |vm| vm.store.has_class(n))
    }

    /// `from` 能否赋给 `to`。`None` = 类层次没加载，判不了（调用方当放过）。
    fn assignable(&self, from: &Ty, to: &Ty) -> Option<bool> {
        if from == to {
            return Some(true);
        }
        match (from, to) {
            (Ty::Top, _) | (_, Ty::Top) => Some(false),
            (Ty::Null, Ty::Ref(_)) => Some(true),
            (Ty::Null, _) | (_, Ty::Null) => Some(false),
            (Ty::Ref(a), Ty::Ref(b)) => self.class_assignable(a, b),
            _ => Some(false),
        }
    }

    fn class_assignable(&self, from: &Arc<str>, to: &Arc<str>) -> Option<bool> {
        if from == to {
            return Some(true);
        }
        if &**to == "java/lang/Object" {
            return Some(true);
        }
        // `Object` 是**最宽的上界**，也是 `lub_ref` 在「类层次没加载」时的退化结果。
        // 对它向下的赋值判定必须放过，否则会误杀合法的 java.base 代码：
        // 实测 `LocaleProviderAdapter.forType` 的多重 catch
        // （`NoSuchMethodException | InvocationTargetException | ClassNotFoundException | …`）
        // 因为那几个异常类没被加载、LUB 退化成 Object，就被判成
        // 「Object 不能赋给 Throwable」——那是验证器错，不是字节码错。
        // 代价是漏检「真的是 Object 却当 String 用」，但那条 HotSpot 会抓，
        // 而这个误杀的代价是**整个 java.base 跑不起来**。
        if &**from == "java/lang/Object" {
            return None;
        }
        let (fa, ta) = (from.starts_with('['), to.starts_with('['));
        match (fa, ta) {
            (true, true) => {
                let (ex, ey) = (elem_of(from), elem_of(to));
                if ex.len() == 1 || ey.len() == 1 {
                    return Some(ex == ey);
                }
                self.class_assignable(&Arc::from(ex), &Arc::from(ey))
            }
            (true, false) => Some(matches!(&**to, "java/lang/Cloneable" | "java/io/Serializable")),
            (false, true) => Some(false),
            (false, false) => {
                if !self.known(from) || !self.known(to) {
                    return None; // 层次未知：放过（独立模式下 java.* 没有 class 文件）
                }
                self.vm.map(|vm| vm.is_subclass(from, to))
            }
        }
    }

    fn check_assign(&self, pc: usize, v: &Ty, want: &Ty) -> Result<(), String> {
        match self.assignable(v, want) {
            Some(true) | None => Ok(()),
            Some(false) => Err(format!("@{pc}: 类型 {} 不能赋给 {}", v.show(), want.show())),
        }
    }

    /// 帧核对里「算不算不一致」的判据。
    ///
    /// `assignable(数据流, 声明)` 为假时还要再反着看一眼：如果**声明比数据流更具体**
    /// （`assignable(声明, 数据流)` 成立），那是我这边汇合得不够准（接口上界之类），
    /// 不是字节码的错 —— 这一层放过，真正的错会在用到该值的指令上暴露。
    /// 两条都不成立（例如 `Top` vs 具体类型）才是真不一致。
    fn frame_mismatch(&self, got: &Ty, want: &Ty) -> bool {
        if !matches!(self.assignable(got, want), Some(false)) {
            return false;
        }
        matches!(self.assignable(want, got), Some(false))
    }
    // ---- 单条指令的取值/存值 ----

    fn do_load(&self, st: &mut St, pc: usize, op: u8, idx: usize) -> Result<(), String> {
        let t = self.local(&st.locals, idx)?;
        let want = match op {
            0x15 => "int",
            0x16 => "long",
            0x17 => "float",
            0x18 => "double",
            _ => "引用",
        };
        let ok = match op {
            0x15 => t == Ty::Int,
            0x16 => t == Ty::Long,
            0x17 => t == Ty::Float,
            0x18 => t == Ty::Double,
            // aload：引用或**未初始化对象**都合法（未初始化的引用可以搬来搬去）
            _ => t.is_ref() || t.is_uninit(),
        };
        if !ok {
            return Err(format!(
                "@{pc}: {} 读到的局部变量 {idx} 是 {}（期望 {}）",
                opname(op),
                t.show(),
                want
            ));
        }
        st.push(t);
        Ok(())
    }

    fn do_store(&self, st: &mut St, pc: usize, op: u8, idx: usize) -> Result<(), String> {
        let (v, cat2) = st.pop_val().map_err(|e| format!("@{pc}: {} {e}", opname(op)))?;
        let ok = match op {
            0x36 => v == Ty::Int,
            0x37 => v == Ty::Long,
            0x38 => v == Ty::Float,
            0x39 => v == Ty::Double,
            _ => v.is_ref() || v.is_uninit(),
        };
        if !ok {
            return Err(format!(
                "@{pc}: {} 存的值是 {}（期望 {}）",
                opname(op),
                v.show(),
                match op {
                    0x36 => "int",
                    0x37 => "long",
                    0x38 => "float",
                    0x39 => "double",
                    _ => "引用",
                }
            ));
        }
        let _ = cat2;
        self.set_local(&mut st.locals, idx, &v)
    }

    // ---- 转移函数 ----

    /// 弹一个值（**允许 cat-2**：long/double 在栈上就占两格，`lmul` 弹到 long 是正常的）。
    /// 只做类型判定，不管槽宽 —— 槽宽不合法的情形由具体指令的规则抓。
    fn pop_any(&self, st: &mut St, pc: usize, op: u8) -> Result<Ty, String> {
        match st.pop_val() {
            Ok((t, _)) => Ok(t),
            Err(e) => Err(format!("@{pc}: {} {e}", opname(op))),
        }
    }

    fn check_target(&self, pc: usize, t: usize) -> Result<usize, String> {
        if t >= self.code.len() {
            return Err(format!("@{pc}: 跳转目标 {t} 越过 code 末尾"));
        }
        if !self.starts[t] {
            return Err(format!("@{pc}: 跳转目标 {t} 不是指令边界"));
        }
        Ok(t)
    }

    fn ldc_ty(&self, idx: u16) -> Result<Ty, String> {
        match self.cf.constant_pool.get(idx as usize).and_then(|e| e.as_ref()) {
            Some(CpEntry::Integer(_)) => Ok(Ty::Int),
            Some(CpEntry::Float(_)) => Ok(Ty::Float),
            Some(CpEntry::Long(_)) => Ok(Ty::Long),
            Some(CpEntry::Double(_)) => Ok(Ty::Double),
            Some(CpEntry::String(_)) => Ok(ty_ref("java/lang/String")),
            Some(CpEntry::Class(_)) => Ok(ty_ref("java/lang/Class")),
            Some(CpEntry::MethodHandle(..)) => Ok(ty_ref("java/lang/invoke/MethodHandle")),
            Some(CpEntry::MethodType(_)) => Ok(ty_ref("java/lang/invoke/MethodType")),
            Some(CpEntry::Dynamic(_, nt)) => {
                let d = match self.cf.constant_pool.get(*nt as usize).and_then(|e| e.as_ref()) {
                    Some(CpEntry::NameAndType(_, d)) => *d,
                    _ => return Err(format!("ldc {idx}: Dynamic 的 NameAndType 非法")),
                };
                let s = String::from_utf8_lossy(self.cf.utf8(d).unwrap_or_default()).into_owned();
                parse_ty(s.as_bytes(), 0).map(|(t, _)| t).ok_or_else(|| format!("ldc {idx}: 描述符 {s}"))
            }
            _ => Err(format!("ldc {idx}: 常量类型不支持")),
        }
    }

    /// 执行一条指令：就地改 `st`，返回后继 pc（不含 fall-through）
    fn transfer(&self, pc: usize, st: &mut St) -> Result<Vec<usize>, String> {
        let op = self.code[pc];
        let len = insn_len(self.code, pc)?;
        let next = pc + len;
        let mut succ: Vec<usize> = Vec::new();

        match op {
            // ---- 常量 ----
            0x00 => {}
            0x01 => st.push(Ty::Null),
            0x02..=0x08 => st.push(Ty::Int),
            0x09 | 0x0a => st.push(Ty::Long),
            0x0b..=0x0d => st.push(Ty::Float),
            0x0e | 0x0f => st.push(Ty::Double),
            0x10 | 0x11 => st.push(Ty::Int),
            0x12 => {
                // ldc 的操作数是**1 字节**的常量池下标（ldc_w / ldc2_w 才是 2 字节）
                let t = self.ldc_ty(*self.code.get(pc + 1).unwrap_or(&0) as u16)?;
                st.push(t);
            }
            0x13 => {
                let t = self.ldc_ty(be2(self.code, pc + 1).unwrap_or(0) as u16)?;
                st.push(t);
            }
            0x14 => {
                let t = self.ldc_ty(be2(self.code, pc + 1).unwrap_or(0) as u16)?;
                if !t.cat2() {
                    return Err(format!("@{pc}: ldc2_w 指向非 cat-2 常量（{}）", t.show()));
                }
                st.push(t);
            }
            // ---- 局部变量 load ----
            0x15..=0x19 => {
                let idx = *self.code.get(pc + 1).unwrap_or(&0) as usize;
                self.do_load(st, pc, op, idx)?;
            }
            0x1a..=0x1d => self.do_load(st, pc, 0x15, (op - 0x1a) as usize)?,
            0x1e..=0x21 => self.do_load(st, pc, 0x16, (op - 0x1e) as usize)?,
            0x22..=0x25 => self.do_load(st, pc, 0x17, (op - 0x22) as usize)?,
            0x26..=0x29 => self.do_load(st, pc, 0x18, (op - 0x26) as usize)?,
            0x2a..=0x2d => self.do_load(st, pc, 0x19, (op - 0x2a) as usize)?,
            // ---- 局部变量 store ----
            0x36..=0x3a => {
                let idx = *self.code.get(pc + 1).unwrap_or(&0) as usize;
                self.do_store(st, pc, op, idx)?;
            }
            0x3b..=0x3e => self.do_store(st, pc, 0x36, (op - 0x3b) as usize)?,
            0x3f..=0x42 => self.do_store(st, pc, 0x37, (op - 0x3f) as usize)?,
            0x43..=0x46 => self.do_store(st, pc, 0x38, (op - 0x43) as usize)?,
            0x47..=0x4a => self.do_store(st, pc, 0x39, (op - 0x47) as usize)?,
            0x4b..=0x4e => self.do_store(st, pc, 0x3a, (op - 0x4b) as usize)?,
            // ---- 数组 load ----
            0x2e..=0x35 => {
                let idx = self.pop_any(st, pc, op)?;
                if idx != Ty::Int {
                    return Err(format!("@{pc}: {} 的下标是 {}", opname(op), idx.show()));
                }
                let arr = self.pop_any(st, pc, op)?;
                let elem = self.array_elem(pc, op, &arr)?;
                match op {
                    0x2f => st.push(Ty::Long),
                    0x30 => st.push(Ty::Float),
                    0x31 => st.push(Ty::Double),
                    0x32 => st.push(ty_ref(&elem)),
                    _ => st.push(Ty::Int),
                }
            }
            // ---- 数组 store ----
            0x4f..=0x56 => {
                let val = self.pop_any(st, pc, op)?;
                let idx = self.pop_any(st, pc, op)?;
                if idx != Ty::Int {
                    return Err(format!("@{pc}: {} 的下标是 {}", opname(op), idx.show()));
                }
                let arr = self.pop_any(st, pc, op)?;
                let elem = self.array_elem(pc, op, &arr)?;
                let ok = match op {
                    0x4f => val == Ty::Int,
                    0x50 => val == Ty::Long,
                    0x51 => val == Ty::Float,
                    0x52 => val == Ty::Double,
                    0x53 => val.is_ref(),
                    0x54 => val == Ty::Int,
                    0x55 => val == Ty::Int,
                    0x56 => val == Ty::Int,
                    _ => false,
                };
                if !ok {
                    return Err(format!(
                        "@{pc}: {} 存的值是 {}（数组元素是 {}）",
                        opname(op),
                        val.show(),
                        elem
                    ));
                }
            }
            // ---- 栈操作 ----
            0x57 => {
                let (v, c) = st.pop_val().map_err(|e| format!("@{pc}: pop {e}"))?;
                if c {
                    return Err(format!("@{pc}: pop 弹到 cat-2 值 {}", v.show()));
                }
            }
            0x58 => {
                st.pop_val().map_err(|e| format!("@{pc}: pop2 {e}"))?;
            }
            0x59 => {
                let (v1, c1) = st.pop_val().map_err(|e| format!("@{pc}: dup {e}"))?;
                if c1 {
                    return Err(format!("@{pc}: dup 弹到 cat-2 值 {}", v1.show()));
                }
                st.push_val(v1.clone(), false);
                st.push_val(v1, false);
            }
            0x5a => {
                let (v1, c1) = st.pop_val().map_err(|e| format!("@{pc}: dup_x1 {e}"))?;
                let (v2, c2) = st.pop_val().map_err(|e| format!("@{pc}: dup_x1 {e}"))?;
                if c1 || c2 {
                    return Err(format!("@{pc}: dup_x1 只能用于两个 cat-1 值"));
                }
                st.push_val(v1.clone(), false);
                st.push_val(v2, false);
                st.push_val(v1, false);
            }
            0x5b => {
                let (v1, c1) = st.pop_val().map_err(|e| format!("@{pc}: dup_x2 {e}"))?;
                if c1 {
                    return Err(format!("@{pc}: dup_x2 第一个值是 cat-2 {}", v1.show()));
                }
                let (v2, c2) = st.pop_val().map_err(|e| format!("@{pc}: dup_x2 {e}"))?;
                if c2 {
                    // form2: [v2(cat2), v1] → v1, v2, v1
                    st.push_val(v1.clone(), false);
                    st.push_val(v2, true);
                    st.push_val(v1, false);
                } else {
                    let (v3, c3) = st.pop_val().map_err(|e| format!("@{pc}: dup_x2 {e}"))?;
                    if c3 {
                        return Err(format!("@{pc}: dup_x2 第三个值是 cat-2 {}", v3.show()));
                    }
                    // form1: [v3, v2, v1] → v1, v3, v2, v1
                    st.push_val(v1.clone(), false);
                    st.push_val(v3, false);
                    st.push_val(v2, false);
                    st.push_val(v1, false);
                }
            }
            0x5c => {
                let (v1, c1) = st.pop_val().map_err(|e| format!("@{pc}: dup2 {e}"))?;
                if c1 {
                    st.push_val(v1.clone(), true);
                    st.push_val(v1, true);
                } else {
                    let (v2, c2) = st.pop_val().map_err(|e| format!("@{pc}: dup2 {e}"))?;
                    if c2 {
                        return Err(format!("@{pc}: dup2 的第二个值是 cat-2 {}", v2.show()));
                    }
                    st.push_val(v2.clone(), false);
                    st.push_val(v1.clone(), false);
                    st.push_val(v2, false);
                    st.push_val(v1, false);
                }
            }
            0x5d => {
                let (v1, c1) = st.pop_val().map_err(|e| format!("@{pc}: dup2_x1 {e}"))?;
                let (v2, c2) = st.pop_val().map_err(|e| format!("@{pc}: dup2_x1 {e}"))?;
                if c2 {
                    return Err(format!("@{pc}: dup2_x1 的第二个值是 cat-2 {}", v2.show()));
                }
                if c1 {
                    // form2: [v2(cat1), v1(cat2)] → v1, v2, v1
                    st.push_val(v1.clone(), true);
                    st.push_val(v2, false);
                    st.push_val(v1, true);
                } else {
                    let (v3, c3) = st.pop_val().map_err(|e| format!("@{pc}: dup2_x1 {e}"))?;
                    if c3 {
                        return Err(format!("@{pc}: dup2_x1 的第三个值是 cat-2 {}", v3.show()));
                    }
                    // form1: [v3, v2, v1] → v2, v1, v3, v2, v1
                    st.push_val(v2.clone(), false);
                    st.push_val(v1.clone(), false);
                    st.push_val(v3, false);
                    st.push_val(v2, false);
                    st.push_val(v1, false);
                }
            }
            0x5e => {
                let (v1, c1) = st.pop_val().map_err(|e| format!("@{pc}: dup2_x2 {e}"))?;
                if c1 {
                    // form4: [v2(cat2), v1(cat2)] → v1, v2, v1
                    let (v2, c2) = st.pop_val().map_err(|e| format!("@{pc}: dup2_x2 {e}"))?;
                    if !c2 {
                        return Err(format!("@{pc}: dup2_x2 form4 的第二个值是 cat-1 {}", v2.show()));
                    }
                    st.push_val(v1.clone(), true);
                    st.push_val(v2, true);
                    st.push_val(v1, true);
                } else {
                    let (v2, c2) = st.pop_val().map_err(|e| format!("@{pc}: dup2_x2 {e}"))?;
                    if c2 {
                        // form3: [v3(cat1), v2(cat2), v1(cat1)] → v1, v3, v2, v1
                        let (v3, c3) = st.pop_val().map_err(|e| format!("@{pc}: dup2_x2 {e}"))?;
                        if c3 {
                            return Err(format!("@{pc}: dup2_x2 form3 的第三个值是 cat-2 {}", v3.show()));
                        }
                        st.push_val(v1.clone(), false);
                        st.push_val(v3, false);
                        st.push_val(v2, true);
                        st.push_val(v1, false);
                    } else {
                        let (v3, c3) = st.pop_val().map_err(|e| format!("@{pc}: dup2_x2 {e}"))?;
                        if c3 {
                            // form2: [v3(cat2), v2(cat1), v1(cat1)] → v2, v1, v3, v2, v1
                            st.push_val(v2.clone(), false);
                            st.push_val(v1.clone(), false);
                            st.push_val(v3, true);
                            st.push_val(v2, false);
                            st.push_val(v1, false);
                        } else {
                            let (v4, c4) = st.pop_val().map_err(|e| format!("@{pc}: dup2_x2 {e}"))?;
                            if c4 {
                                return Err(format!(
                                    "@{pc}: dup2_x2 form1 的第四个值是 cat-2 {}",
                                    v4.show()
                                ));
                            }
                            // form1: [v4, v3, v2, v1] → v2, v1, v4, v3, v2, v1
                            st.push_val(v2.clone(), false);
                            st.push_val(v1.clone(), false);
                            st.push_val(v4, false);
                            st.push_val(v3, false);
                            st.push_val(v2, false);
                            st.push_val(v1, false);
                        }
                    }
                }
            }
            0x5f => {
                let (v1, c1) = st.pop_val().map_err(|e| format!("@{pc}: swap {e}"))?;
                let (v2, c2) = st.pop_val().map_err(|e| format!("@{pc}: swap {e}"))?;
                if c1 || c2 {
                    return Err("@{pc}: swap 只能用于两个 cat-1 值".replace("{pc}", &pc.to_string()));
                }
                st.push_val(v1, false);
                st.push_val(v2, false);
            }
            // ---- 算术 ----
            // JVM 的操作码按「运算」分组、再按类型（i/l/f/d）排列：
            // 0x60 iadd ladd fadd dadd … 0x6c idiv ldiv fdiv ddiv，0x70 irem lrem frem drem。
            0x60..=0x73 => {
                let ty = match (op - 0x60) % 4 {
                    0 => Ty::Int,
                    1 => Ty::Long,
                    2 => Ty::Float,
                    _ => Ty::Double,
                };
                let b = self.pop_any(st, pc, op)?;
                let a = self.pop_any(st, pc, op)?;
                if a != ty || b != ty {
                    return Err(format!(
                        "@{pc}: {} 需要两个 {}，拿到 {} / {}",
                        opname(op),
                        ty.show(),
                        a.show(),
                        b.show()
                    ));
                }
                st.push(ty);
            }
            // 0x74..=0x77 neg（一元）
            0x74..=0x77 => {
                let ty = match op - 0x74 {
                    0 => Ty::Int,
                    1 => Ty::Long,
                    2 => Ty::Float,
                    _ => Ty::Double,
                };
                let v = self.pop_any(st, pc, op)?;
                if v != ty {
                    return Err(format!("@{pc}: {} 需要 {}，拿到 {}", opname(op), ty.show(), v.show()));
                }
                st.push(ty);
            }
            // 0x78..=0x7d 位移：long << int
            0x78..=0x7d => {
                let wide = matches!(op, 0x79 | 0x7b | 0x7d);
                let ty = if wide { Ty::Long } else { Ty::Int };
                let n = self.pop_any(st, pc, op)?;
                if n != Ty::Int {
                    return Err(format!("@{pc}: {} 的位移量是 {}", opname(op), n.show()));
                }
                let v = self.pop_any(st, pc, op)?;
                if v != ty {
                    return Err(format!("@{pc}: {} 需要 {}，拿到 {}", opname(op), ty.show(), v.show()));
                }
                st.push(ty);
            }
            // 0x7e..=0x83 位运算（i/l 交替）
            0x7e..=0x83 => {
                let ty = if (op - 0x7e) % 2 == 1 { Ty::Long } else { Ty::Int };
                let b = self.pop_any(st, pc, op)?;
                let a = self.pop_any(st, pc, op)?;
                if a != ty || b != ty {
                    return Err(format!(
                        "@{pc}: {} 需要两个 {}，拿到 {} / {}",
                        opname(op),
                        ty.show(),
                        a.show(),
                        b.show()
                    ));
                }
                st.push(ty);
            }
            0x84 => {
                let idx = *self.code.get(pc + 1).unwrap_or(&0) as usize;
                let cur = self.local(&st.locals, idx)?;
                if cur != Ty::Int {
                    return Err(format!("@{pc}: iinc 的目标是 {}（期望 int）", cur.show()));
                }
            }
            // ---- 数值转换 ----
            0x85..=0x93 => {
                let (from, to) = match op {
                    0x85 => (Ty::Int, Ty::Long),
                    0x86 => (Ty::Int, Ty::Float),
                    0x87 => (Ty::Int, Ty::Double),
                    0x88 => (Ty::Long, Ty::Int),
                    0x89 => (Ty::Long, Ty::Float),
                    0x8a => (Ty::Long, Ty::Double),
                    0x8b => (Ty::Float, Ty::Int),
                    0x8c => (Ty::Float, Ty::Long),
                    0x8d => (Ty::Float, Ty::Double),
                    0x8e => (Ty::Double, Ty::Int),
                    0x8f => (Ty::Double, Ty::Long),
                    0x90 => (Ty::Double, Ty::Float),
                    _ => (Ty::Int, Ty::Int), // i2b / i2c / i2s
                };
                let v = self.pop_any(st, pc, op)?;
                if v != from {
                    return Err(format!("@{pc}: {} 需要 {}，拿到 {}", opname(op), from.show(), v.show()));
                }
                st.push(to);
            }
            // ---- 比较 ----
            0x94..=0x98 => {
                let want = match op {
                    0x94 => Ty::Long,
                    0x95 | 0x96 => Ty::Float,
                    _ => Ty::Double,
                };
                let b = self.pop_any(st, pc, op)?;
                let a = self.pop_any(st, pc, op)?;
                if a != want || b != want {
                    return Err(format!(
                        "@{pc}: {} 需要两个 {}，拿到 {} / {}",
                        opname(op),
                        want.show(),
                        a.show(),
                        b.show()
                    ));
                }
                st.push(Ty::Int);
            }
            // ---- 条件跳转 ----
            0x99..=0x9e | 0xc6 | 0xc7 => {
                let v = self.pop_any(st, pc, op)?;
                if op >= 0xc6 {
                    if !v.is_ref() {
                        return Err(format!("@{pc}: {} 需要引用，拿到 {}", opname(op), v.show()));
                    }
                } else if v != Ty::Int {
                    return Err(format!("@{pc}: {} 需要 int，拿到 {}", opname(op), v.show()));
                }
                let off = i16_at(self.code, pc + 1).ok_or_else(|| format!("@{pc}: 跳转偏移截断"))?;
                succ.push(self.check_target(pc, (pc as i64 + off as i64) as usize)?);
            }
            0x9f..=0xa6 => {
                let b = self.pop_any(st, pc, op)?;
                let a = self.pop_any(st, pc, op)?;
                if op <= 0xa4 {
                    if a != Ty::Int || b != Ty::Int {
                        return Err(format!(
                            "@{pc}: {} 需要两个 int，拿到 {} / {}",
                            opname(op),
                            a.show(),
                            b.show()
                        ));
                    }
                } else if !a.is_ref() || !b.is_ref() {
                    return Err(format!(
                        "@{pc}: {} 需要两个引用，拿到 {} / {}",
                        opname(op),
                        a.show(),
                        b.show()
                    ));
                }
                let off = i16_at(self.code, pc + 1).ok_or_else(|| format!("@{pc}: 跳转偏移截断"))?;
                succ.push(self.check_target(pc, (pc as i64 + off as i64) as usize)?);
            }
            0xa7 => {
                let off = i16_at(self.code, pc + 1).ok_or_else(|| format!("@{pc}: goto 偏移截断"))?;
                succ.push(self.check_target(pc, (pc as i64 + off as i64) as usize)?);
            }
            0xc8 => {
                let off = i32_at(self.code, pc + 1).ok_or_else(|| format!("@{pc}: goto_w 偏移截断"))?;
                succ.push(self.check_target(pc, (pc as i64 + off as i64) as usize)?);
            }
            0xaa | 0xab => {
                let v = self.pop_any(st, pc, op)?;
                if v != Ty::Int {
                    return Err(format!("@{pc}: {} 需要 int 键值，拿到 {}", opname(op), v.show()));
                }
                let mut q = pc + 1;
                while q % 4 != 0 {
                    q += 1;
                }
                let default = i32_at(self.code, q).ok_or_else(|| format!("@{pc}: switch 截断"))?;
                succ.push(self.check_target(pc, (pc as i64 + default as i64) as usize)?);
                if op == 0xaa {
                    let low = i32_at(self.code, q + 4).ok_or_else(|| format!("@{pc}: switch 截断"))?;
                    let high = i32_at(self.code, q + 8).ok_or_else(|| format!("@{pc}: switch 截断"))?;
                    let n = (high as i64 - low as i64 + 1).max(0) as usize;
                    for k in 0..n {
                        let off = i32_at(self.code, q + 12 + k * 4)
                            .ok_or_else(|| format!("@{pc}: switch 表截断"))?;
                        succ.push(self.check_target(pc, (pc as i64 + off as i64) as usize)?);
                    }
                } else {
                    let npairs = i32_at(self.code, q + 4).ok_or_else(|| format!("@{pc}: switch 截断"))?;
                    for k in 0..npairs.max(0) as usize {
                        let off = i32_at(self.code, q + 12 + k * 8)
                            .ok_or_else(|| format!("@{pc}: switch 表截断"))?;
                        succ.push(self.check_target(pc, (pc as i64 + off as i64) as usize)?);
                    }
                }
            }
            // ---- 返回 ----
            0xac..=0xb0 => {
                let v = self.pop_any(st, pc, op)?;
                let ok = match op {
                    0xac => v == Ty::Int,
                    0xad => v == Ty::Long,
                    0xae => v == Ty::Float,
                    0xaf => v == Ty::Double,
                    _ => v.is_ref(),
                };
                if !ok {
                    return Err(format!("@{pc}: {} 返回 {}（类型不符）", opname(op), v.show()));
                }
            }
            0xb1 => {}
            // ---- 字段 ----
            0xb2..=0xb5 => {
                let idx = be2(self.code, pc + 1).unwrap_or(0) as u16;
                let (owner, fname, fdesc) = member_at(self.cf, idx)
                    .ok_or_else(|| format!("@{pc}: 字段引用 {idx} 解析失败"))?;
                let ft = parse_ty(fdesc.as_bytes(), 0)
                    .map(|(t, _)| t)
                    .ok_or_else(|| format!("@{pc}: 字段描述符解析失败 {fdesc}"))?;
                match op {
                    0xb2 => st.push(ft),
                    0xb3 => {
                        let v = self.pop_any(st, pc, op)?;
                        self.check_assign(pc, &v, &ft)?;
                    }
                    0xb4 => {
                        let r = self.pop_any(st, pc, op)?;
                        self.check_receiver(pc, op, &r, &owner, &fname)?;
                        st.push(ft);
                    }
                    _ => {
                        let v = self.pop_any(st, pc, op)?;
                        let r = self.pop_any(st, pc, op)?;
                        // JVMS 4.10.1.9：构造器里给**本类**字段赋值时，接收者允许是
                        // `uninitializedThis`（`this.x = x;` 在 `super()` 之前是合法的）
                        let uninit_this_ok = matches!(r, Ty::UninitThis)
                            && self
                                .cf
                                .this_class_name()
                                .map(|c| c == *owner)
                                .unwrap_or(false);
                        if !uninit_this_ok {
                            self.check_receiver(pc, op, &r, &owner, &fname)?;
                        }
                        self.check_assign(pc, &v, &ft)?;
                    }
                }
            }
            // ---- 调用 ----
            0xb6..=0xb9 => {
                let idx = be2(self.code, pc + 1).unwrap_or(0) as u16;
                let (owner, mname, mdesc) = member_at(self.cf, idx)
                    .ok_or_else(|| format!("@{pc}: 方法引用 {idx} 解析失败"))?;
                let (args, ret) = parse_method_desc(&mdesc)
                    .ok_or_else(|| format!("@{pc}: 方法描述符解析失败 {mdesc}"))?;
                for (k, a) in args.iter().enumerate().rev() {
                    let v = self.pop_any(st, pc, op)?;
                    self.check_assign(pc, &v, a).map_err(|e| {
                        format!("{e}（{owner}.{mname}{mdesc} 的第 {} 个参数）", k + 1)
                    })?;
                }
                if op != 0xb8 {
                    let r = self.pop_any(st, pc, op)?;
                    if mname == "<init>" {
                        if !r.is_uninit() {
                            return Err(format!(
                                "@{pc}: invokespecial <init> 的接收者是 {}（期望未初始化的对象）",
                                r.show()
                            ));
                        }
                        // 未初始化对象在构造器里叫 uninitializedThis，此时类型取自当前类
                        let cls = match r {
                            Ty::UninitThis => {
                                Arc::from(self.cf.this_class_name().unwrap_or_default().as_str())
                            }
                            _ => owner.clone(),
                        };
                        st.replace_uninit(&r, &cls);
                    } else {
                        self.check_receiver(pc, op, &r, &owner, &mname)?;
                    }
                }
                if let Some(t) = ret {
                    st.push(t);
                }
            }
            0xba => {
                let idx = be2(self.code, pc + 1).unwrap_or(0) as u16;
                let nt = match self.cf.constant_pool.get(idx as usize).and_then(|e| e.as_ref()) {
                    Some(CpEntry::InvokeDynamic(_, nt)) => *nt,
                    _ => return Err(format!("@{pc}: invokedynamic {idx} 解析失败")),
                };
                let d = match self.cf.constant_pool.get(nt as usize).and_then(|e| e.as_ref()) {
                    Some(CpEntry::NameAndType(_, d)) => *d,
                    _ => return Err(format!("@{pc}: invokedynamic 的 NameAndType 解析失败")),
                };
                let d = String::from_utf8_lossy(self.cf.utf8(d).unwrap_or_default()).into_owned();
                let (args, ret) = parse_method_desc(&d)
                    .ok_or_else(|| format!("@{pc}: invokedynamic 描述符解析失败 {d}"))?;
                for a in args.iter().rev() {
                    let v = self.pop_any(st, pc, op)?;
                    self.check_assign(pc, &v, a)?;
                }
                if let Some(t) = ret {
                    st.push(t);
                }
            }
            // ---- 对象 ----
            0xbb => {
                // new：压未初始化对象；`<init>` 会把它换成正式类型
                st.push(Ty::Uninit(pc as u16));
            }
            0xbc => {
                let at = *self.code.get(pc + 1).unwrap_or(&0);
                let n = self.pop_any(st, pc, op)?;
                if n != Ty::Int {
                    return Err(format!("@{pc}: newarray 的长度是 {}", n.show()));
                }
                let elem = match at {
                    4 => "Z",
                    5 => "C",
                    6 => "F",
                    7 => "D",
                    8 => "B",
                    9 => "S",
                    10 => "I",
                    11 => "J",
                    _ => return Err(format!("@{pc}: newarray 的 atype={at} 非法")),
                };
                st.push(ty_ref(&format!("[{elem}")));
            }
            0xbd => {
                let idx = be2(self.code, pc + 1).unwrap_or(0) as u16;
                let class =
                    class_at(self.cf, idx).ok_or_else(|| format!("@{pc}: anewarray 的类索引非法"))?;
                let n = self.pop_any(st, pc, op)?;
                if n != Ty::Int {
                    return Err(format!("@{pc}: anewarray 的长度是 {}", n.show()));
                }
                let d = if class.starts_with('[') {
                    format!("[{class}")
                } else {
                    format!("[L{class};")
                };
                st.push(ty_ref(&d));
            }
            0xbe => {
                let a = self.pop_any(st, pc, op)?;
                match &a {
                    Ty::Ref(d) if d.starts_with('[') => {}
                    Ty::Ref(d) => return Err(format!("@{pc}: arraylength 作用在非数组 {d}")),
                    other => return Err(format!("@{pc}: arraylength 的接收者是 {}", other.show())),
                }
                st.push(Ty::Int);
            }
            0xbf => {
                let v = self.pop_any(st, pc, op)?;
                if !v.is_ref() {
                    return Err(format!("@{pc}: athrow 抛的是 {}", v.show()));
                }
            }
            0xc0 => {
                let v = self.pop_any(st, pc, op)?;
                if !v.is_ref() {
                    return Err(format!("@{pc}: checkcast 的对象是 {}", v.show()));
                }
                let idx = be2(self.code, pc + 1).unwrap_or(0) as u16;
                let class =
                    class_at(self.cf, idx).ok_or_else(|| format!("@{pc}: checkcast 的类索引非法"))?;
                st.push(ty_ref(&class));
            }
            0xc1 => {
                let v = self.pop_any(st, pc, op)?;
                if !v.is_ref() {
                    return Err(format!("@{pc}: instanceof 的对象是 {}", v.show()));
                }
                st.push(Ty::Int);
            }
            0xc2 | 0xc3 => {
                let v = self.pop_any(st, pc, op)?;
                if !v.is_ref() {
                    return Err(format!("@{pc}: {} 的对象是 {}", opname(op), v.show()));
                }
            }
            0xc5 => {
                let idx = be2(self.code, pc + 1).unwrap_or(0) as u16;
                let dims = *self.code.get(pc + 3).unwrap_or(&0) as usize;
                let class = class_at(self.cf, idx)
                    .ok_or_else(|| format!("@{pc}: multianewarray 的类索引非法"))?;
                for _ in 0..dims {
                    let v = self.pop_any(st, pc, op)?;
                    if v != Ty::Int {
                        return Err(format!("@{pc}: multianewarray 的维度是 {}", v.show()));
                    }
                }
                st.push(ty_ref(&class));
            }
            0xc4 => {
                let sub = *self.code.get(pc + 1).unwrap_or(&0);
                let idx = be2(self.code, pc + 2).unwrap_or(0);
                match sub {
                    0x15..=0x19 => self.do_load(st, pc, sub, idx)?,
                    0x36..=0x3a => self.do_store(st, pc, sub, idx)?,
                    0x84 => {
                        let cur = self.local(&st.locals, idx)?;
                        if cur != Ty::Int {
                            return Err(format!("@{pc}: wide iinc 的目标是 {}（期望 int）", cur.show()));
                        }
                    }
                    _ => return Err(format!("@{pc}: wide 后面跟了 0x{sub:02x}")),
                }
            }
            _ => return Err(format!("@{pc}: 未处理的指令 0x{op:02x}")),
        }

        // 末端检查：栈高度 / fall-through
        if st.stack.len() > self.max_stack {
            return Err(format!(
                "@{pc}: {} 之后操作数栈需要 {} 槽，超过 max_stack={}",
                opname(op),
                st.stack.len(),
                self.max_stack
            ));
        }
        let terminates = matches!(op, 0xac..=0xb1 | 0xbf | 0xa7 | 0xa8 | 0xc8 | 0xc9);
        if !terminates {
            succ.push(next);
        }
        Ok(succ)
    }

    /// 数组元素类型（顺带校验数组描述符）
    /// 数组元素类型。注意 `elem_of` 把 `Ljava/lang/String;` 归一到类名 `java/lang/String`，
    /// 基本类型元素仍是单字母（`I`/`J`/…），嵌套数组是 `[I` 这种描述符。
    fn array_elem(&self, pc: usize, op: u8, arr: &Ty) -> Result<String, String> {
        let d = match arr {
            Ty::Null => return Ok("Ljava/lang/Object;".into()),
            Ty::Ref(d) if d.starts_with('[') => d,
            other => {
                return Err(format!("@{pc}: {} 的数组引用是 {}", opname(op), other.show()));
            }
        };
        let elem = elem_of(d);
        let is_prim = elem.len() == 1 && "IJFDBCSZ".contains(&elem);
        let ok = match (op, elem.as_str()) {
            (0x2e | 0x4f, "I") => true,
            (0x2f | 0x50, "J") => true,
            (0x30 | 0x51, "F") => true,
            (0x31 | 0x52, "D") => true,
            (0x32 | 0x53, _) => !is_prim, // 引用元素（含嵌套数组）
            (0x33 | 0x54, "B") | (0x33 | 0x54, "Z") => true,
            (0x34 | 0x55, "C") => true,
            (0x35 | 0x56, "S") => true,
            _ => false,
        };
        if !ok {
            return Err(format!(
                "@{pc}: {} 的元素类型不匹配（数组是 {}）",
                opname(op),
                d
            ));
        }
        Ok(elem)
    }

    fn check_receiver(
        &self,
        pc: usize,
        op: u8,
        r: &Ty,
        owner: &Arc<str>,
        what: &str,
    ) -> Result<(), String> {
        if !r.is_ref() {
            return Err(format!(
                "@{pc}: {} 的接收者类型是 {}（{what}）",
                opname(op),
                r.show()
            ));
        }
        if let Ty::Ref(rc) = r {
            let sub = self.vm.map_or(true, |vm| vm.is_subclass(rc, owner));
            if self.known(rc) && self.known(owner) && !sub {
                return Err(format!(
                    "@{pc}: {} 的接收者 {} 不是 {} 的子类（{what}）",
                    opname(op),
                    rc,
                    owner
                ));
            }
        }
        Ok(())
    }

    // ---- StackMapTable ----

    /// 核对声明的帧：`数据流算出来的 <= 声明的`（JVMS 4.10.1.2 frameIsAssignable 方向）
    fn check_frames(
        &self,
        raw: &[u8],
        states: &[Option<St>],
        params: &[Ty],
    ) -> Result<(), String> {
        let n = be2(raw, 0).ok_or_else(|| "StackMapTable 截断".to_string())?;
        let mut q = 2usize;
        let mut prev: i64 = -1;
        // 声明侧的局部变量列表：从「this + 参数」开始增删（**不补到 max_locals**）
        let mut locals: Vec<Ty> = params.to_vec();
        for _ in 0..n {
            let ft = *raw.get(q).ok_or_else(|| "StackMapTable 截断".to_string())?;
            q += 1;
            let (delta, stack, new_locals): (i64, Vec<Ty>, Option<Vec<Ty>>) = match ft {
                0..=63 => (ft as i64, Vec::new(), None),
                64..=127 => {
                    let (t, used) = self.parse_vti(raw, q)?;
                    q += used;
                    (ft as i64 - 64, expand(t), None)
                }
                247 => {
                    let d = be2(raw, q).ok_or_else(|| "StackMapTable 截断".to_string())? as i64;
                    q += 2;
                    let (t, used) = self.parse_vti(raw, q)?;
                    q += used;
                    (d, expand(t), None)
                }
                248..=250 => {
                    let d = be2(raw, q).ok_or_else(|| "StackMapTable 截断".to_string())? as i64;
                    q += 2;
                    let k = 251 - ft as usize;
                    let mut l = locals.clone();
                    // chop 砍的是 k 个**值**：cat-2 值在列表里占两格（值 + Top）
                    for _ in 0..k {
                        let wide = matches!(l.len(), n if n >= 2)
                            && matches!(l.last(), Some(Ty::Top))
                            && matches!(l.get(l.len() - 2), Some(Ty::Long | Ty::Double));
                        l.pop();
                        if wide {
                            l.pop();
                        }
                    }
                    (d, Vec::new(), Some(l))
                }
                251 => {
                    let d = be2(raw, q).ok_or_else(|| "StackMapTable 截断".to_string())? as i64;
                    q += 2;
                    (d, Vec::new(), None)
                }
                252..=254 => {
                    let d = be2(raw, q).ok_or_else(|| "StackMapTable 截断".to_string())? as i64;
                    q += 2;
                    let k = ft as usize - 251;
                    let mut l = locals.clone();
                    for _ in 0..k {
                        let (t, used) = self.parse_vti(raw, q)?;
                        q += used;
                        l.extend(expand(t));
                    }
                    (d, Vec::new(), Some(l))
                }
                255 => {
                    let d = be2(raw, q).ok_or_else(|| "StackMapTable 截断".to_string())? as i64;
                    q += 2;
                    let nl = be2(raw, q).ok_or_else(|| "StackMapTable 截断".to_string())?;
                    q += 2;
                    let mut l: Vec<Ty> = Vec::with_capacity(nl);
                    for _ in 0..nl {
                        let (t, used) = self.parse_vti(raw, q)?;
                        q += used;
                        l.extend(expand(t));
                    }
                    let ns = be2(raw, q).ok_or_else(|| "StackMapTable 截断".to_string())?;
                    q += 2;
                    let mut s: Vec<Ty> = Vec::with_capacity(ns);
                    for _ in 0..ns {
                        let (t, used) = self.parse_vti(raw, q)?;
                        q += used;
                        s.extend(expand(t));
                    }
                    (d, s, Some(l))
                }
                _ => return Err(format!("StackMapTable: 非法 frame_type={ft}")),
            };
            let pc = prev + delta + 1;
            prev = pc;
            if let Some(l) = new_locals {
                locals = l;
            }
            if pc < 0 {
                return Err("StackMapTable: 帧偏移为负".into());
            }
            let pc = pc as usize;
            if pc >= self.code.len() || !self.starts[pc] {
                return Err(format!("StackMapTable: 帧偏移 {pc} 不是指令边界"));
            }
            if locals.len() > self.max_locals {
                return Err(format!(
                    "StackMapTable: 帧 @{pc} 声明 {} 个局部变量槽，超过 max_locals={}",
                    locals.len(),
                    self.max_locals
                ));
            }
            if stack.len() > self.max_stack {
                return Err(format!(
                    "StackMapTable: 帧 @{pc} 声明 {} 个栈槽，超过 max_stack={}",
                    stack.len(),
                    self.max_stack
                ));
            }
            // 与数据流算出来的状态核对（算不到就是死代码，跳过）
            let Some(st) = &states[pc] else { continue };
            if st.stack.len() != stack.len() {
                return Err(format!(
                    "StackMapTable: 帧 @{pc} 声明栈高 {}，数据流算出来是 {}",
                    stack.len(),
                    st.stack.len()
                ));
            }
            for (i, (got, want)) in st.stack.iter().zip(stack.iter()).enumerate() {
                if matches!(want, Ty::Top) {
                    continue;
                }
                if self.frame_mismatch(got, want) {
                    return Err(format!(
                        "StackMapTable: 帧 @{pc} 的栈槽 {i} 声明 {}，数据流是 {}",
                        want.show(),
                        got.show()
                    ));
                }
            }
            for i in 0..locals.len().max(st.locals.len()) {
                let want = locals.get(i).cloned().unwrap_or(Ty::Top);
                if matches!(want, Ty::Top) {
                    // 声明为 Top 的槽：数据流里有值也算过（javac 可能声明得更窄，
                    // 错的值会在用到它的指令上暴露，不在这一层误杀）
                    continue;
                }
                let got = st.locals.get(i).cloned().unwrap_or(Ty::Top);
                if self.frame_mismatch(&got, &want) {
                    return Err(format!(
                        "StackMapTable: 帧 @{pc} 的局部变量 {i} 声明 {}，数据流是 {}",
                        want.show(),
                        got.show()
                    ));
                }
            }
        }
        Ok(())
    }

    fn parse_vti(&self, raw: &[u8], q: usize) -> Result<(Ty, usize), String> {
        let tag = *raw.get(q).ok_or_else(|| "StackMapTable 截断".to_string())?;
        Ok(match tag {
            0 => (Ty::Top, 1),
            1 => (Ty::Int, 1),
            2 => (Ty::Float, 1),
            3 => (Ty::Double, 1),
            4 => (Ty::Long, 1),
            5 => (Ty::Null, 1),
            6 => (Ty::UninitThis, 1),
            7 => {
                let idx = be2(raw, q + 1).ok_or_else(|| "StackMapTable 截断".to_string())? as u16;
                let c = class_at(self.cf, idx)
                    .ok_or_else(|| format!("StackMapTable: Object_variable_info 的类索引 {idx} 非法"))?;
                (ty_ref(&c), 3)
            }
            8 => {
                let off = be2(raw, q + 1).ok_or_else(|| "StackMapTable 截断".to_string())? as u16;
                (Ty::Uninit(off), 3)
            }
            _ => return Err(format!("StackMapTable: 非法 verification_type_info tag={tag}")),
        })
    }
}

/// 帧里的一个 `verification_type_info` → 槽位（`long`/`double` 在槽位里占两格）
fn expand(t: Ty) -> Vec<Ty> {
    if t.cat2() {
        vec![t, Ty::Top]
    } else {
        vec![t]
    }
}

fn elem_of(d: &Arc<str>) -> String {
    let s: &str = &d[1..];
    if s.len() >= 2 && s.starts_with('L') && s.ends_with(';') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// 元素名 → 数组描述符里的元素写法（`java/lang/String` → `Ljava/lang/String;`）
fn norm_arr_elem(e: &str) -> String {
    if e.starts_with('[') || e.len() == 1 {
        e.to_string()
    } else {
        format!("L{e};")
    }
}
