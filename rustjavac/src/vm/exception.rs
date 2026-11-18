//! 异常：抛出、异常表查找、未捕获异常的文本。

use std::collections::HashMap;
use std::sync::Mutex;

use super::shared::lk;
use super::stacktrace::FrameInfo;
use super::Vm;
use super::VmError;

/// 未捕获异常要打的栈：句柄 → **异常第一次被创建/抛出时**抓的帧快照。
///
/// 为什么必须在当场抓：栈到 `thread_finish` 时已经弹空了（异常一路 `?` 传上去，
/// 每个 `execute_body` 的 `FrameGuard` 都在返回时把自己的帧摘掉）。所以快照只能
/// 在异常**产生**的地方取一份存起来 —— 与 HotSpot 的 `fillInStackTrace` 同一个时机。
///
/// 两个采集点（都是「第一次赢」，后面的不动）：
/// 1. `stacktrace::throwable_fill_in` —— 真 `java.lang.Throwable` 走字节码构造；
/// 2. `Vm::java_exception` —— VM 自己造的异常（`Integer.parseInt` 这类走的是第 1 条，
///    VM 内建里抛的走这条）。
static EXC_FRAMES: Mutex<Option<HashMap<i32, Vec<FrameInfo>>>> = Mutex::new(None);

/// 句子柄上限。未捕获异常本身很少，但**被捕获**的异常也会在构造时记一笔，长时间跑的
/// 程序会把表撑大；超过就整张丢掉（丢的只是诊断用的栈，不影响任何语义）。
const EXC_FRAMES_MAX: usize = 512;

/// 记一份帧快照（**已有就不覆盖**：栈轨迹属于第一次创建它的那个时刻）
pub(super) fn note_frames(h: i32, frames: Vec<FrameInfo>) {
    if h == 0 {
        return;
    }
    let mut g = EXC_FRAMES.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(HashMap::new);
    if m.len() >= EXC_FRAMES_MAX {
        m.clear();
    }
    m.entry(h).or_insert(frames);
}

/// 取这个异常的快照（没有就是 None —— 调用方自己决定要不要打栈）
fn frames_of(h: i32) -> Option<Vec<FrameInfo>> {
    let g = EXC_FRAMES.lock().unwrap_or_else(|e| e.into_inner());
    g.as_ref().and_then(|m| m.get(&h).cloned())
}

/// 帧列表 → HotSpot 的栈文本（每帧一行，行首 `\n\tat `）
///
/// 三种括号形态与 `StackTraceElement.toString` 一致：
/// - 有文件有行号 → `(R2.java:12)`
/// - 有文件没行号 → `(R2.java)`
/// - 没有 `SourceFile` 属性 → `(Unknown Source)`
fn frames_text(frames: &[FrameInfo]) -> String {
    let mut s = String::new();
    for f in frames {
        s.push_str("\n\tat ");
        // 模块前缀只在 JDK 引导层的类上有（`java.base/java.lang.Integer.parseInt`）
        if let Some(m) = f.module {
            s.push_str(m);
            s.push('/');
        }
        s.push_str(&f.class.replace('/', "."));
        s.push('.');
        s.push_str(&f.method);
        match (f.source.as_deref(), f.line) {
            (Some(src), n) if n > 0 => {
                s.push('(');
                s.push_str(src);
                s.push(':');
                s.push_str(&n.to_string());
                s.push(')');
            }
            (Some(src), _) => {
                s.push('(');
                s.push_str(src);
                s.push(')');
            }
            (None, _) => s.push_str("(Unknown Source)"),
        }
    }
    s
}

/// 抛一个 Java 异常（参数是异常对象的引用句柄）
pub(super) fn thrown(h: i32) -> VmError {
    VmError { msg: format!("未捕获的 Java 异常（句柄 {h}）"), exc: Some(h) }
}

impl Vm {
    /// 异常对象 → 打印用的 `类名: message`（+ 栈）
    pub(super) fn exc_text(&self, handle: i32) -> String {
        let head = self.exc_head(handle);
        match frames_of(handle) {
            Some(frames) if !frames.is_empty() => head + &frames_text(&frames),
            _ => head,
        }
    }

    /// 只有 `类名: message` 那一行（不带栈）
    fn exc_head(&self, handle: i32) -> String {
        let class = self.type_name(handle);
        match lk(&self.sh.exc_msgs).get(&handle) {
            Some(m) => format!("{class}: {}", self.string_text(*m)),
            // 真字节码构造出来的异常：`Throwable.detailMessage` 字段里有消息
            None => match self.throwable_message(handle) {
                Some(m) => format!("{class}: {m}"),
                None => class,
            },
        }
    }

    /// 真 `java.lang.Throwable` 实例的 `detailMessage`（没有就说 None）
    fn throwable_message(&self, handle: i32) -> Option<String> {
        if !matches!(self.ref_kind(handle), crate::vm::heap::RefKind::Obj(ref c)
            if self.is_subclass(c, "java/lang/Throwable"))
        {
            return None;
        }
        let slots = self.sh.heap.snapshot(handle)?;
        let i = self.store.field_index("java/lang/Throwable", "detailMessage")?;
        let s = slots.get(i)?.as_i32();
        if s == 0 {
            None
        } else {
            Some(self.obj_to_string(s))
        }
    }

    /// 按顺序在异常表里找能接住这个异常的 handler（返回 handler 的字节码偏移）
    pub(super) fn find_handler(
        &self,
        cf: &crate::ClassFile,
        table: &[(u16, u16, u16, u16)],
        pc: usize,
        handle: i32,
    ) -> Result<Option<usize>, VmError> {
        for &(start, end, handler, catch_type) in table {
            if (start as usize) <= pc && pc < (end as usize) {
                if catch_type == 0 {
                    return Ok(Some(handler as usize)); // catch-all（finally）
                }
                let name = super::class::class_name_of(cf, catch_type)?;
                if self.is_instance(handle, &name) {
                    return Ok(Some(handler as usize));
                }
            }
        }
        Ok(None)
    }
}
