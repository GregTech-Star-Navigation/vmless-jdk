//! 线程栈与死锁检测。
//!
//! 输出格式**逐字符对齐 HotSpot 的 `jstack`**（见 `tests/format_align.rs`）：
//!
//! ```text
//! "main" #1 prio=5 os_prio=0 cpu=123.45ms elapsed=12.34s tid=0x0000000000000001 nid=0x1 runnable [0x0000000000000000]
//!    java.lang.Thread.State: RUNNABLE
//! 	at com.example.Main.main(Main.java:10)
//! 	- locked <0x00000000000000ff> (a java.lang.Object)
//!
//! Locked ownable synchronizers:
//! 	- None
//! ```
//!
//! 死锁检测是在 **VM 上报的锁图** 上做的：`waiting_on`（等谁）+ `locks`（持有谁）。
//! 找环就是 DFS，环上一圈线程就是 HotSpot 报的那一组。

use std::collections::HashMap;

/// 一个 monitor / 锁。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LockRef {
    pub id: u64,
    pub class: String,
}

impl LockRef {
    pub fn new(id: u64, class: impl Into<String>) -> LockRef {
        LockRef { id, class: class.into() }
    }
    /// HotSpot 的 `<0x...>` 形式。
    pub fn hex(&self) -> String {
        format!("<0x{:016x}>", self.id)
    }
}

/// 一个栈帧。
#[derive(Debug, Clone)]
pub struct Frame {
    pub class: String,
    pub method: String,
    pub file: Option<String>,
    pub line: Option<i32>,
    pub native: bool,
    /// 在这个帧里持有了哪些锁
    pub locked: Vec<LockRef>,
}

impl Frame {
    pub fn java(class: impl Into<String>, method: impl Into<String>, file: impl Into<String>, line: i32) -> Frame {
        Frame {
            class: class.into(),
            method: method.into(),
            file: Some(file.into()),
            line: Some(line),
            native: false,
            locked: Vec::new(),
        }
    }
    pub fn native_m(class: impl Into<String>, method: impl Into<String>) -> Frame {
        Frame {
            class: class.into(),
            method: method.into(),
            file: None,
            line: None,
            native: true,
            locked: Vec::new(),
        }
    }
    /// `at com.example.Main.main(Main.java:10)` 里的位置部分。
    pub fn location(&self) -> String {
        if self.native {
            // 真 jstack 的版式是 `at java.lang.Thread.sleep(Native Method)`
            return format!("{}.{}(Native Method)", self.class, self.method);
        }
        // HotSpot 的版式是 `类.方法(文件:行)`
        let qualified = format!("{}.{}", self.class, self.method);
        match (&self.file, self.line) {
            (Some(f), Some(l)) if l > 0 => format!("{}({}:{})", qualified, f, l),
            (Some(f), _) => format!("{}({})", qualified, f),
            _ => format!("{}(Unknown Source)", qualified),
        }
    }
}

/// 一个 Java 线程。
#[derive(Debug, Clone)]
pub struct JThread {
    pub name: String,
    /// HotSpot 叫它 "tid"（其实是 #N 的序号）；`jstack` 里 `#1` 就是它。
    pub java_id: u64,
    pub daemon: bool,
    pub priority: i32,
    pub os_priority: i32,
    /// `java.lang.Thread.State`
    pub state: String,
    pub frames: Vec<Frame>,
    /// 正在等哪把锁
    pub waiting_on: Option<LockRef>,
    /// 持有的锁
    pub locks: Vec<LockRef>,
    /// HotSpot 的 `tid=0x...`
    pub tid: u64,
    /// HotSpot 的 `nid=0x...`（原生线程 id）
    pub nid: u64,
    pub cpu_ns: Option<u64>,
    pub elapsed_ns: Option<u64>,
    /// `[0x...]` 栈基址；没有就输出 `[0x0000000000000000]`
    pub stack_base: u64,
}

impl JThread {
    pub fn new(name: impl Into<String>, java_id: u64, state: impl Into<String>) -> JThread {
        JThread {
            name: name.into(),
            java_id,
            daemon: false,
            priority: 5,
            os_priority: 0,
            state: state.into(),
            frames: Vec::new(),
            waiting_on: None,
            locks: Vec::new(),
            tid: java_id,
            nid: java_id,
            cpu_ns: None,
            elapsed_ns: None,
            stack_base: 0,
        }
    }
}

/// 一次线程快照。
#[derive(Debug, Clone, Default)]
pub struct ThreadDump {
    pub threads: Vec<JThread>,
}

impl ThreadDump {
    /// 找 Java 级死锁：在 "等待 → 持有" 的图上找环。
    ///
    /// 返回每个环上的线程**下标**，按环内顺序。**不 panic**：自环（等自己持有的锁）
    /// 也算一环，HotSpot 也这么报。
    pub fn deadlocks(&self) -> Vec<Vec<usize>> {
        // 锁 → 持有它的线程下标
        let mut owners: HashMap<u64, usize> = HashMap::new();
        for (i, t) in self.threads.iter().enumerate() {
            for l in &t.locks {
                owners.insert(l.id, i);
            }
        }
        // 等锁的边：i -> owner
        let mut edges: Vec<Option<usize>> = vec![None; self.threads.len()];
        for (i, t) in self.threads.iter().enumerate() {
            if let Some(l) = &t.waiting_on {
                if let Some(&o) = owners.get(&l.id) {
                    edges[i] = Some(o);
                }
            }
        }
        let mut seen = vec![false; self.threads.len()];
        let mut cycles: Vec<Vec<usize>> = Vec::new();
        for start in 0..self.threads.len() {
            if seen[start] {
                continue;
            }
            // 顺着边走，记录路径
            let mut path: Vec<usize> = Vec::new();
            let mut pos: HashMap<usize, usize> = HashMap::new();
            let mut cur = start;
            loop {
                if let Some(&p) = pos.get(&cur) {
                    let mut cyc: Vec<usize> = path[p..].to_vec();
                    if cyc.len() > 1 || edges[cur] == Some(cur) {
                        cyc.sort_unstable();
                        if !cycles.iter().any(|c| c == &cyc) {
                            cycles.push(cyc);
                        }
                    }
                    break;
                }
                if seen[cur] {
                    break;
                }
                pos.insert(cur, path.len());
                path.push(cur);
                match edges[cur] {
                    Some(n) => cur = n,
                    None => break,
                }
            }
            for p in path {
                seen[p] = true;
            }
        }
        cycles
    }

    /// 渲染成 `jstack` 的文本。
    ///
    /// `with_locks` = `jstack -l`（多打印 ownable synchronizers），
    /// `with_deadlock` = `jstack -l` 的死锁段（HotSpot 只在 `-l` 时报死锁）。
    pub fn render(&self, vm_name: &str, vm_version: &str, with_locks: bool) -> String {
        let mut out = String::new();
        out.push_str(&format!("Full thread dump {} ({} mixed mode):\n\n", vm_name, vm_version));
        for t in &self.threads {
            out.push_str(&format!(
                "\"{}\"{} #{} prio={} os_prio={} cpu={} elapsed={} tid=0x{:016x} nid=0x{:x} {} [0x{:016x}]\n",
                t.name,
                if t.daemon { " daemon" } else { "" },
                t.java_id,
                t.priority,
                t.os_priority,
                fmt_duration(t.cpu_ns.unwrap_or(0)),
                fmt_duration(t.elapsed_ns.unwrap_or(0)),
                t.tid,
                t.nid,
                t.state.to_lowercase(),
                t.stack_base
            ));
            out.push_str(&format!("   java.lang.Thread.State: {}\n", t.state));
            for f in &t.frames {
                out.push_str(&format!("\tat {}\n", f.location()));
                for l in &f.locked {
                    out.push_str(&format!("\t- locked {} (a {})\n", l.hex(), l.class));
                }
            }
            if let Some(w) = &t.waiting_on {
                out.push_str(&format!("\t- waiting on {} (a {})\n", w.hex(), w.class));
            }
            if with_locks {
                out.push_str("\n   Locked ownable synchronizers:\n");
                let mut any = false;
                for l in &t.locks {
                    out.push_str(&format!("\t- {} (a {})\n", l.hex(), l.class));
                    any = true;
                }
                if !any {
                    out.push_str("\t- None\n");
                }
            }
            out.push('\n');
        }
        if with_locks {
            let cycles = self.deadlocks();
            if !cycles.is_empty() {
                out.push_str(&deadlock_text(&cycles, &self.threads));
            }
        }
        out
    }
}

/// 死锁报告段。格式对齐 HotSpot：
/// `Found one Java-level deadlock:` + 每个线程一段 + `Found N deadlocks.`
pub fn deadlock_text(cycles: &[Vec<usize>], threads: &[JThread]) -> String {
    let mut out = String::new();
    match cycles.len() {
        0 => {}
        1 => out.push_str("Found one Java-level deadlock:\n"),
        n => out.push_str(&format!("Found {} Java-level deadlocks:\n", n)),
    }
    out.push_str("=============================\n");
    for cyc in cycles {
        for &i in cyc {
            let t = match threads.get(i) {
                Some(t) => t,
                None => continue,
            };
            out.push_str(&format!("\"{}\":\n", t.name));
            match &t.waiting_on {
                Some(w) => {
                    // 持有者必须**和检测时用的那条边一致**：优先选同环里的线程，
                    // 否则多个线程持有同一把锁时，报出来的名字会和环对不上。
                    let in_cycle = |i: usize| cyc.contains(&i);
                    let owner = threads
                        .iter()
                        .enumerate()
                        .filter(|(i, o)| in_cycle(*i) && o.locks.iter().any(|l| l.id == w.id))
                        .map(|(_, o)| o.name.clone())
                        .next()
                        .or_else(|| {
                            threads
                                .iter()
                                .find(|o| o.locks.iter().any(|l| l.id == w.id))
                                .map(|o| o.name.clone())
                        })
                        .unwrap_or_else(|| "<未知>".to_string());
                    out.push_str(&format!(
                        "  waiting to lock monitor 0x{:016x} (object 0x{:016x}, a {}),\n  which is held by \"{}\"\n",
                        w.id, w.id, w.class, owner
                    ));
                }
                None => out.push_str("  (没有等待中的锁 —— 图里不该出现，说明上报的锁信息不完整)\n"),
            }
        }
    }
    out.push('\n');
    if cycles.len() == 1 {
        out.push_str("Found 1 deadlock.\n\n");
    } else {
        out.push_str(&format!("Found {} deadlocks.\n\n", cycles.len()));
    }
    out
}

/// `123.45ms` / `12.34s` —— 与 jstack 的 `cpu=` / `elapsed=` 一致。
pub fn fmt_duration(ns: u64) -> String {
    let ms = ns as f64 / 1e6;
    if ms < 1000.0 {
        format!("{:.2}ms", ms)
    } else {
        format!("{:.2}s", ms / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(name: &str, holds: Vec<u64>, waits: Option<u64>) -> JThread {
        let mut t = JThread::new(name, 1, "BLOCKED");
        t.locks = holds.into_iter().map(|i| LockRef::new(i, "java.lang.Object")).collect();
        t.waiting_on = waits.map(|i| LockRef::new(i, "java.lang.Object"));
        t
    }

    #[test]
    fn no_deadlock_when_acyclic() {
        let d = ThreadDump { threads: vec![t("A", vec![1], None), t("B", vec![], Some(1))] };
        assert!(d.deadlocks().is_empty());
    }

    #[test]
    fn two_thread_cycle() {
        let d = ThreadDump { threads: vec![t("A", vec![1], Some(2)), t("B", vec![2], Some(1))] };
        let c = d.deadlocks();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0], vec![0, 1]);
    }

    #[test]
    fn self_deadlock() {
        let d = ThreadDump { threads: vec![t("A", vec![1], Some(1))] };
        assert_eq!(d.deadlocks(), vec![vec![0]]);
    }

    #[test]
    fn three_thread_cycle() {
        let d = ThreadDump {
            threads: vec![t("A", vec![1], Some(2)), t("B", vec![2], Some(3)), t("C", vec![3], Some(1))],
        };
        let c = d.deadlocks();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0], vec![0, 1, 2]);
    }

    #[test]
    fn render_contains_expected_lines() {
        let mut th = JThread::new("main", 1, "RUNNABLE");
        th.frames.push(Frame::java("com.example.Main", "main", "Main.java", 10));
        th.elapsed_ns = Some(1_234_000_000);
        th.cpu_ns = Some(123_450_000);
        th.tid = 1;
        th.nid = 1;
        let d = ThreadDump { threads: vec![th] };
        let s = d.render("VMless Server VM", "0.1.0", true);
        assert!(s.starts_with("Full thread dump VMless Server VM (0.1.0 mixed mode):"));
        assert!(s.contains("\"main\" #1 prio=5 os_prio=0 cpu=123.45ms elapsed=1.23s"));
        assert!(s.contains("java.lang.Thread.State: RUNNABLE"));
        assert!(s.contains("\tat com.example.Main.main(Main.java:10)"));
        assert!(s.contains("Locked ownable synchronizers:"));
        assert!(s.contains("\t- None"));
    }

    #[test]
    fn deadlock_text_format() {
        let d = ThreadDump { threads: vec![t("A", vec![1], Some(2)), t("B", vec![2], Some(1))] };
        let s = d.render("VM", "1", true);
        assert!(s.contains("Found one Java-level deadlock:"));
        assert!(s.contains("which is held by \"B\""));
        assert!(s.contains("Found 1 deadlock."));
    }
}
