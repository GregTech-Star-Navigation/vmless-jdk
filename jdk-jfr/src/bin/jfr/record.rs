//! `jfr record` 的实现（被 `bin/jfr.rs` 用 `#[path]` 引进来）。
//!
//! 两条路径：
//!
//! | 模式 | 事件来源 | 用途 |
//! | --- | --- | --- |
//! | `--demo`（默认） | 内置事件模型 + 合成数据 | 自检「我们写的 .jfr 真 jfr 读得动」 |
//! | `--pid <pid>` | vmless-attach：`Thread.print` 采样 + perfdata 的 GC 计数 | 给 VMless 的 VM 做真录制 |
//!
//! 两条路走的是**同一个** [`vmless_jfr::recorder::Recorder`] 和同一套写出逻辑，
//! 差别只在事件从哪来。

use std::io::Write;

use vmless_jfr::recorder::{ClassInfo, FrameInfo, Recorder, ThreadInfo};

/// 生成一批内置事件（不依赖任何外部进程）。
///
/// 覆盖全部 5 个内置事件类型，字段填的都是**真实语义**的值（栈帧、类、线程 id），
/// 这样真 `jfr print` 打出来的东西能看出对不对，而不是一堆 0。
pub fn record_demo(rec: &mut Recorder, count: usize) -> Result<(), String> {
    let main = ThreadInfo {
        java_name: "main".into(),
        java_id: 1,
        os_name: "main".into(),
        os_id: std::process::id() as i64,
        virtual_thread: false,
    };
    let worker = ThreadInfo {
        java_name: "worker-0".into(),
        java_id: 42,
        os_name: "worker-0".into(),
        os_id: std::process::id() as i64 + 1,
        virtual_thread: false,
    };
    let frames: Vec<FrameInfo> = vec![
        FrameInfo {
            class_name: "demo.Busy".into(),
            method_name: "work".into(),
            descriptor: "(JLjava/lang/String;)V".into(),
            line: 42,
            bci: 7,
            modifiers: 2,
            frame_type: "Interpreted".into(),
        },
        FrameInfo {
            class_name: "java.lang.Thread".into(),
            method_name: "run".into(),
            descriptor: "()V".into(),
            line: 1474,
            bci: 19,
            modifiers: 1,
            frame_type: "JIT compiled".into(),
        },
    ];
    let loader = ClassInfo {
        name: "app".into(),
        loader_name: Some("app".into()),
        loader_id: 2,
        ..Default::default()
    };

    rec.thread_start(&main, &worker, &frames)
        .map_err(|e| e.to_string())?;
    rec.class_load(
        &ClassInfo {
            name: "demo.Busy".into(),
            modifiers: 33,
            hidden: false,
            loader_name: Some("app".into()),
            loader_id: 2,
        },
        Some(&loader),
        None,
        &frames,
    )
    .map_err(|e| e.to_string())?;

    let vm_thread = ThreadInfo {
        java_name: "VM Thread".into(),
        java_id: 2,
        os_name: "VM Thread".into(),
        os_id: std::process::id() as i64 + 2,
        virtual_thread: false,
    };
    let base = vmless_jfr::recorder::now_nanos();
    for i in 0..count {
        let ts = base + (i as i64) * 1_000_000;
        if i % 10 == 0 {
            rec.gc_phase_pause(&vm_thread, (i / 10) as i64, "Young Pause", ts, 4_038_000)
                .map_err(|e| e.to_string())?;
        }
        if i % 25 == 7 {
            rec.java_monitor_enter(
                &worker,
                &ClassInfo {
                    name: "demo.Busy".into(),
                    modifiers: 33,
                    ..Default::default()
                },
                Some(&main),
                0x7f00_1234_5000,
                ts,
                1_500_000,
                &frames,
            )
            .map_err(|e| e.to_string())?;
        }
        rec.execution_sample(&worker, "STATE_RUNNABLE", &frames)
            .map_err(|e| e.to_string())?;
    }
    rec.thread_end(&worker).map_err(|e| e.to_string())?;
    Ok(())
}

/// 从 vmless-attach 采样一个真 VM。
///
/// 抓的是两样现在就能拿到的东西：
/// - `Thread.print` 的栈 → `jdk.ExecutionSample`（每线程一条）
/// - perfdata 里 GC 的调用次数 → `jdk.GCPhasePause`
///
/// 采样间隔 20ms（50Hz），与真 JFR 的默认采样率同一量级。
pub fn record_from_vm(rec: &mut Recorder, pid: u32, count: usize) -> Result<(), String> {
    let mut c = vmless_attach::Client::attach(pid, 2000).map_err(|e| e.to_string())?;
    for i in 0..count {
        let dump = c.command("Thread.print").map_err(|e| e.to_string())?;
        for (thread, frames) in parse_thread_dump(&dump) {
            if thread.java_id <= 0 {
                continue;
            }
            rec.execution_sample(&thread, "STATE_RUNNABLE", &frames)
                .map_err(|e| e.to_string())?;
        }
        if i % 5 == 0 {
            if let Ok(pd) = vmless_attach::tool::read_perf(pid) {
                let gc_count: i64 = pd.get_i64("sun.gc.collector.0.invocations").unwrap_or(0);
                rec.gc_phase_pause(
                    &ThreadInfo { java_name: "VM Thread".into(), java_id: 2, os_name: "VM Thread".into(), os_id: 2, virtual_thread: false },
                    gc_count,
                    "Young Pause",
                    vmless_jfr::recorder::now_nanos(),
                    1_000_000,
                )
                .map_err(|e| e.to_string())?;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Ok(())
}

/// 把 `Thread.print` 的输出解析成 `(线程, 栈帧)`。
///
/// 认的行形态：
/// - `"worker-1" #45 prio=5 ...` —— 线程头（名字 + `#javaThreadId`）
/// - `\tat demo.Busy.work(Busy.java:42)` —— 栈帧
/// - `java.lang.Thread.State: RUNNABLE` 之类的状态行忽略
pub fn parse_thread_dump(text: &str) -> Vec<(ThreadInfo, Vec<FrameInfo>)> {
    let mut out: Vec<(ThreadInfo, Vec<FrameInfo>)> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix('"') {
            if let Some((name, after)) = rest.split_once('"') {
                let java_id = after
                    .trim_start()
                    .strip_prefix('#')
                    .map(|s| {
                        s.chars()
                            .take_while(|c| c.is_ascii_digit())
                            .collect::<String>()
                    })
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0);
                out.push((
                    ThreadInfo {
                        java_name: name.to_string(),
                        java_id,
                        os_name: name.to_string(),
                        os_id: java_id,
                        virtual_thread: false,
                    },
                    Vec::new(),
                ));
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("at ") {
            if let Some(last) = out.last_mut() {
                if let Some(f) = parse_frame(rest) {
                    last.1.push(f);
                }
            }
        }
    }
    out
}

fn parse_frame(s: &str) -> Option<FrameInfo> {
    let (head, tail) = s.split_once('(')?;
    let line = tail
        .trim_end_matches(')')
        .rsplit(':')
        .next()
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(-1);
    let (class, method) = match head.rsplit_once('.') {
        Some((c, m)) => (c.to_string(), m.to_string()),
        None => (head.to_string(), String::new()),
    };
    Some(FrameInfo {
        class_name: class,
        method_name: method,
        descriptor: "()V".into(),
        line,
        bci: 0,
        modifiers: 0,
        frame_type: "Interpreted".into(),
    })
}

/// `jfr record` 的入口（由 `bin/jfr.rs` 调用）。
pub fn run(args: &[String]) -> i32 {
    let mut file: Option<String> = None;
    let mut pid: Option<u32> = None;
    let mut max_events = 200usize;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--demo" => {}
            "--pid" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<u32>().ok()) {
                    Some(p) => pid = Some(p),
                    None => return fail("--pid 需要一个十进制进程号"),
                }
            }
            "--events" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<usize>().ok()) {
                    Some(n) => max_events = n,
                    None => return fail("--events 需要一个数字"),
                }
            }
            other if other.starts_with("--") => {
                return fail(&format!("未知选项 {}", other));
            }
            other => file = Some(other.to_string()),
        }
        i += 1;
    }
    let file = match file {
        Some(f) => f,
        None => return fail("record 需要一个输出文件名（例如 out.jfr）"),
    };
    let mut rec = Recorder::start(vmless_jfr::recorder::RecorderConfig {
        max_events,
        start_nanos: None,
    });
    let r = match pid {
        Some(p) => record_from_vm(&mut rec, p, max_events),
        None => record_demo(&mut rec, max_events),
    };
    if let Err(e) = r {
        return fail(&e);
    }
    match rec.dump(&file) {
        Ok(n) => {
            println!(
                "写出 {}（{} 字节，{} 条事件{}）",
                file,
                n,
                rec.event_count(),
                if rec.dropped_count() > 0 {
                    format!("，环形缓冲丢弃 {} 条", rec.dropped_count())
                } else {
                    String::new()
                }
            );
            0
        }
        Err(e) => fail(&format!("写 {} 失败: {}", file, e)),
    }
}

fn fail(msg: &str) -> i32 {
    let _ = writeln!(std::io::stderr(), "错误: {}", msg);
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_dump_parsing() {
        let text = "\"main\" #3 prio=5 os_prio=0 cpu=12.34ms elapsed=1.23s tid=0x1 nid=0x2 runnable\n   java.lang.Thread.State: RUNNABLE\n\tat demo.Busy.work(Busy.java:42)\n\tat java.lang.Thread.run(Thread.java:1474)\n\n\"worker-0\" #44 prio=5\n   java.lang.Thread.State: WAITING\n\tat demo.Busy.lambda$main$0(Busy.java:9)\n";
        let v = parse_thread_dump(text);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].0.java_name, "main");
        assert_eq!(v[0].0.java_id, 3);
        assert_eq!(v[0].1.len(), 2);
        assert_eq!(v[0].1[0].class_name, "demo.Busy");
        assert_eq!(v[0].1[0].method_name, "work");
        assert_eq!(v[0].1[0].line, 42);
        assert_eq!(v[1].0.java_id, 44);
        assert_eq!(v[1].1.len(), 1);
        assert_eq!(v[1].1[0].line, 9);
    }
}
