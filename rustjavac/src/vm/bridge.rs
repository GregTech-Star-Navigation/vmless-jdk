//! 解释器 ↔ JIT 的**方法级**调用桥。
//!
//! 两套运行时各有自己的堆与静态字段表，所以桥只允许**基本类型**跨越边界：
//! 参数与返回值里出现引用（`L…;` / `[…]`）的方法不参与桥接。
//!
//! 两个方向：
//!
//! * **解释器 → JIT**：`install_jit` 装一张「纯数值方法」表，`execute` 进门先查它 ——
//!   命中就直接调机器码。没命中照常解释（「能编的走 JIT，编不了的走解释器」）。
//! * **JIT → 解释器**：`call_interpreted` 供 JIT 侧生成的解释器桩用。
//!
//! 表里能出现哪些方法由 `jit::pure` 判定 —— 那是**语义安全**判据，不只是「能编译」：
//! 字节码里不许碰对象/数组/字段/监视器，也不许调 `java.*`，且调用图的传递闭包都得干净。

use std::cell::RefCell;

use super::err;
use super::slot::Slot;
use super::Vm;
use super::VmError;
use crate::{ClassFile, CpEntry};

/// JIT 侧登记的一个可调用入口（机器码 + 帧规格）
#[derive(Clone, Copy)]
pub struct JitEntry {
    /// `extern "C" fn(*mut i64) -> 返回值` 的入口地址
    pub code: usize,
    pub max_locals: usize,
    /// 返回类型：`V` / `I` / `J` / `F` / `D`（描述符里的那个字符）
    pub ret: u8,
}

pub type JitTable = Vec<(String, String, String, JitEntry)>;

thread_local! {
    /// JIT 侧逃进来的解释器（懒建：只有真的遇到编不了的方法才建）
    static INTERP: RefCell<Option<Vm>> = const { RefCell::new(None) };
    /// JIT 侧登记过的「只能解释执行」的方法：(类, 名, 描述符)
    static INTERP_METHODS: RefCell<Vec<(String, String, String)>> = const { RefCell::new(Vec::new()) };
    /// 同一个方法可能有多个调用点：编号按方法去重（统计里「解释 M 个」是**方法的个数**）
    static INTERP_IDS: RefCell<std::collections::HashMap<(String, String, String), u32>> =
        RefCell::new(std::collections::HashMap::new());
}

/// JIT 方法表：**进程级**的（解释器会开多个线程，表必须在所有线程可见）。
///
/// 用 `OnceLock` 而不是 `Mutex`：表只在启动时装一次，之后读路径没有锁。
static JIT_TABLE: std::sync::OnceLock<JitTable> = std::sync::OnceLock::new();

/// 装表（重复调用时忽略第一次之后的：一个进程只跑一个入口）
pub fn install_jit(table: JitTable) {
    let _ = JIT_TABLE.set(table);
}

/// 已装的方法个数（诊断用）
pub fn jit_table_len() -> usize {
    JIT_TABLE.get().map(|t| t.len()).unwrap_or(0)
}

/// 解释器进方法时的第一道查问：命中 JIT 就调机器码，`None` 表示照常解释。
///
/// 这是解释器最热的入口，所以：先判空（全解释运行时只有一次借用），
/// 再按**字节**比较类名/方法名（不分配），最后把 `JitEntry` 复制出来再放开借用
/// —— 机器码里可能再回调解释器，借用不能跨调用。
pub(super) fn try_call_jit(
    cf: &ClassFile,
    method: usize,
    desc: &str,
    args: &[Slot],
    receiver: bool,
) -> Option<Result<Slot, VmError>> {
    if receiver {
        return None;
    }
    let t = JIT_TABLE.get()?;
    if t.is_empty() {
        return None;
    }
    let cname = class_bytes(cf);
    let mname = cf.utf8(cf.methods.get(method)?.name_index).unwrap_or_default();
    let entry = *t
        .iter()
        .find(|(c, n, d, _)| c.as_bytes() == cname && n.as_bytes() == mname && d == desc)
        .map(|(_, _, _, e)| e)?;
    Some(call_entry(&entry, desc, args))
}

/// 按描述符把解释器槽位拍成 i64，调机器码，再把返回值还原成槽位。
///
/// 这就是两个方向共用的 ABI：`(i64* locals) -> 返回值`，float/double 走位模式。
fn call_entry(e: &JitEntry, desc: &str, args: &[Slot]) -> Result<Slot, VmError> {
    let slots = crate::jit::param_slots(desc);
    let need = slots.iter().copied().max().map(|m| m + 2).unwrap_or(2);
    let mut locals = vec![0i64; e.max_locals.max(need).max(args.len() + 1)];
    for (i, a) in args.iter().enumerate() {
        let s = slots.get(i).copied().unwrap_or(i);
        if s < locals.len() {
            locals[s] = slot_bits(*a);
        }
    }
    let p = locals.as_mut_ptr();
    let raw: i64 = unsafe {
        match e.ret {
            b'J' => {
                let f: unsafe extern "C" fn(*mut i64) -> i64 = std::mem::transmute(e.code);
                f(p)
            }
            b'D' => {
                let f: unsafe extern "C" fn(*mut i64) -> f64 = std::mem::transmute(e.code);
                f(p).to_bits() as i64
            }
            b'F' => {
                let f: unsafe extern "C" fn(*mut i64) -> f32 = std::mem::transmute(e.code);
                f(p).to_bits() as i64
            }
            b'V' => {
                let f: unsafe extern "C" fn(*mut i64) -> i32 = std::mem::transmute(e.code);
                f(p);
                0
            }
            _ => {
                let f: unsafe extern "C" fn(*mut i64) -> i32 = std::mem::transmute(e.code);
                f(p) as i64
            }
        }
    };
    Ok(bits_slot(raw, e.ret))
}

fn class_bytes(cf: &ClassFile) -> &[u8] {
    match cf.constant_pool.get(cf.this_class as usize).and_then(|e| e.as_ref()) {
        Some(CpEntry::Class(n)) => cf.utf8(*n).unwrap_or_default(),
        _ => &[],
    }
}

/// 槽位 → 机器码 ABI 里的 i64（float/double 走位模式，int 符号扩展）
fn slot_bits(v: Slot) -> i64 {
    match v {
        Slot::I(x) => x as i64,
        Slot::L(x) => x,
        Slot::Fl(x) => x.to_bits() as i64,
        Slot::F(x) => x.to_bits() as i64,
    }
}

/// ABI 的 i64 → 槽位（按返回类型解码）
fn bits_slot(raw: i64, ret: u8) -> Slot {
    match ret {
        b'J' => Slot::L(raw),
        b'D' => Slot::F(f64::from_bits(raw as u64)),
        b'F' => Slot::Fl(f32::from_bits(raw as u32)),
        b'V' => Slot::I(0),
        _ => Slot::I(raw as i32),
    }
}

// ---------- JIT → 解释器（生成代码里的解释器桩走这里）----------

/// JIT 侧登记一个只能解释执行的方法，返回它的稳定编号
pub fn register_interp(class: &str, name: &str, desc: &str) -> u32 {
    let key = (class.to_string(), name.to_string(), desc.to_string());
    if let Some(id) = INTERP_IDS.with(|m| m.borrow().get(&key).copied()) {
        return id;
    }
    let id = INTERP_METHODS.with(|v| {
        let mut v = v.borrow_mut();
        v.push(key.clone());
        (v.len() - 1) as u32
    });
    INTERP_IDS.with(|m| m.borrow_mut().insert(key, id));
    id
}

/// 解释器**必须**在任何机器码跑起来之前建好（由 `run_jit` 调）。
///
/// 为什么不允许在回调里懒建：`Vm::open` 要加载整个 java.base（~95MB + 数秒）。
/// 一旦它落在生成代码的调用路径上，出任何岔子都会变成「每次调用重建一个解释器」——
/// 工作集按 90MB/s 往上涨直到把机器吃光（这条路径 2026-11-17 实测吃过 18.8GB）。
/// 所以这里只做**一次性**建立，回调里查不到就直接报错退出，绝不分配。
pub fn open_interp(dir: &str, main_class: &str) -> Result<(), VmError> {
    if INTERP.with(|c| c.borrow().is_some()) {
        return Ok(());
    }
    let mut vm = Vm::open(dir, main_class)?;
    // 进 VM：`run_main` 会做的三件事，这里也要做 —— 否则 `execute` 在没有线程登记
    // 的情况下跑（`cur_tid()` = 0），监视器/hand-off 用的是非法 tid，会卡死。
    super::thread::set_main_tid();
    vm.sh.safepoint.register();
    vm.thread_register(1, 0, "jit-caller", false);
    INTERP.with(|c| *c.borrow_mut() = Some(vm));
    Ok(())
}

/// 登记过多少个「只能解释执行」的方法（`run_jit` 据此决定要不要建解释器）
pub fn interp_targets() -> usize {
    INTERP_METHODS.with(|v| v.borrow().len())
}

/// 解释器建好了没有（诊断用）
pub fn interp_ready() -> bool {
    INTERP.with(|c| c.borrow().is_some())
}

/// JIT 调用一个只能解释执行的方法。`args` 已按描述符拍成 i64。
pub unsafe fn call_interpreted_ptr(id: u32, vals: *const i64) -> i64 {
    let (class, name, desc) = match INTERP_METHODS.with(|v| v.borrow().get(id as usize).cloned()) {
        Some(x) => x,
        None => {
            eprintln!("JIT 回退失败：解释器方法编号 {id} 没登记");
            std::process::exit(1);
        }
    };
    let n = crate::jit::param_descs(&desc).len();
    let args: Vec<i64> = (0..n)
        .map(|i| unsafe { std::ptr::read_unaligned(vals.add(i)) })
        .collect();
    match with_interp(|vm| vm.call_primitive(&class, &name, &desc, &args)) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("JIT 回退到解释器时失败：{class}.{name}{desc}：{}", e.msg);
            std::process::exit(1);
        }
    }
}

/// 只**借用**已经建好的解释器；没有就报错退出（不在这里分配 —— 见 `open_interp`）
fn with_interp<R>(f: impl FnOnce(&mut Vm) -> R) -> R {
    INTERP.with(|cell| {
        let mut g = cell.borrow_mut();
        match g.as_mut() {
            Some(vm) => f(vm),
            None => {
                eprintln!("JIT 回退失败：解释器没有预先建好（vm::open_interp 未调用）");
                std::process::exit(1);
            }
        }
    })
}

impl Vm {
    /// 按 `(类名, 方法名, 描述符)` 执行一个**参数与返回值都是基本类型**的方法。
    ///
    /// 参数/返回值都是 i64 位模式（int 取低 32 位、float/double 走位模式），
    /// 这是 JIT 的 `(i64* locals) -> 返回值` ABI 在解释器这边的等价物。
    /// 引用类型一律拒绝：两个运行时各有自己的堆，跨边界没有意义。
    pub fn call_primitive(
        &mut self,
        class: &str,
        name: &str,
        desc: &str,
        args: &[i64],
    ) -> Result<i64, VmError> {
        if has_ref(desc) {
            return err(format!("{class}.{name}{desc} 含引用类型，不能跨 JIT/解释器边界"));
        }
        let (ci, mi) = match self.store.methods.get(&(class.to_string(), name.to_string(), desc.to_string())) {
            Some(v) => *v,
            None => return err(format!("解释器里找不到 {class}.{name}{desc}")),
        };
        let params = crate::jit::param_descs(desc);
        let mut slots = Vec::with_capacity(args.len());
        for (i, a) in args.iter().enumerate() {
            let p = params.get(i).map(|s| s.as_str()).unwrap_or("I");
            slots.push(bits_slot_typed(*a, p));
        }
        let d = desc.to_string();
        let r = self.execute(ci, mi, &d, &slots, false)?;
        Ok(match r {
            Slot::I(v) => v as i64,
            Slot::L(v) => v,
            Slot::Fl(v) => v.to_bits() as i64,
            Slot::F(v) => v.to_bits() as i64,
        })
    }
}

/// ABI 的 i64 → 槽位（按参数描述符解码）
fn bits_slot_typed(raw: i64, desc: &str) -> Slot {
    match desc.as_bytes().first().copied() {
        Some(b'J') => Slot::L(raw),
        Some(b'F') => Slot::Fl(f32::from_bits(raw as u32)),
        Some(b'D') => Slot::F(f64::from_bits(raw as u64)),
        _ => Slot::I(raw as i32),
    }
}

/// 描述符里有没有引用类型（参数或返回值）
pub fn has_ref(desc: &str) -> bool {
    desc.bytes().any(|b| b == b'L' || b == b'[')
}
