//! 录制器：`Recorder::start(config)` → 环形缓冲 → `dump(path)` 写合法 chunk。
//!
//! # 内置事件（字段顺序与真 JFR 的定义**逐字段一致**）
//!
//! | 事件 | 字段 |
//! | --- | --- |
//! | `jdk.ExecutionSample` | startTime, sampledThread, stackTrace, state |
//! | `jdk.GCPhasePause` | startTime, duration, eventThread, gcId, name |
//! | `jdk.JavaMonitorEnter` | startTime, duration, eventThread, stackTrace, monitorClass, previousOwner, address |
//! | `jdk.ThreadStart` | startTime, eventThread, stackTrace, thread, parentThread |
//! | `jdk.ThreadEnd` | startTime, eventThread, thread |
//! | `jdk.ClassLoad` | startTime, duration, eventThread, stackTrace, loadedClass, definingClassLoader, initiatingClassLoader |
//!
//! 字段顺序不是随便定的：`startTime` 必须是第 0 个（读侧 `objectAt(0)` 直接拿它当
//! ticks），有 `duration` 的必须是第 1 个（`EventParser.startIndex = 2`）。
//!
//! # 类型 id
//!
//! JFR 的 type id 是写侧自选的（真文件的 id 是 237、239 这种小整数，不是哈希），
//! 只要元数据、常量池、事件三处一致就行。这里取固定编号，方便测试断言。

use std::collections::VecDeque;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::format::event::EventRecord;
use crate::format::Result;
use crate::model::{AnnotationDef, FieldDef, Metadata, TypeDef, Value, SUPER_ANNOTATION, SUPER_EVENT};
use crate::writer::ChunkSpec;

// —— 基础类型 id ——
pub const T_LONG: i64 = 1;
pub const T_INT: i64 = 2;
pub const T_BOOLEAN: i64 = 3;
pub const T_STRING: i64 = 4;
pub const T_FLOAT: i64 = 5;
pub const T_DOUBLE: i64 = 6;
pub const T_SHORT: i64 = 7;
pub const T_BYTE: i64 = 8;
pub const T_CHAR: i64 = 9;

// —— 注解类型 id ——
const A_LABEL: i64 = 100;
const A_DESCRIPTION: i64 = 101;
const A_TIMESTAMP: i64 = 102;
const A_TIMESPAN: i64 = 103;
const A_CATEGORY: i64 = 104;
const A_UNSIGNED: i64 = 105;
const A_MEMORY_ADDRESS: i64 = 106;
const A_STACK_TRACE: i64 = 107;
const A_CONTENT_TYPE: i64 = 108;

// —— 结构体类型 id ——
pub const T_THREAD: i64 = 200;
pub const T_CLASS_LOADER: i64 = 201;
pub const T_CLASS: i64 = 202;
pub const T_METHOD: i64 = 203;
pub const T_STACK_FRAME: i64 = 204;
pub const T_STACK_TRACE: i64 = 205;

// —— 事件类型 id ——
pub const E_EXECUTION_SAMPLE: i64 = 300;
pub const E_GC_PHASE_PAUSE: i64 = 301;
pub const E_JAVA_MONITOR_ENTER: i64 = 302;
pub const E_THREAD_START: i64 = 303;
pub const E_THREAD_END: i64 = 304;
pub const E_CLASS_LOAD: i64 = 305;

fn ann(type_id: i64, value: &str) -> AnnotationDef {
    AnnotationDef {
        type_id,
        type_name: String::new(),
        attrs: vec![("value".to_string(), value.to_string())],
    }
}

fn ann_multi(type_id: i64, values: &[&str]) -> AnnotationDef {
    AnnotationDef {
        type_id,
        type_name: String::new(),
        attrs: values
            .iter()
            .enumerate()
            .map(|(i, v)| (format!("value-{}", i), v.to_string()))
            .collect(),
    }
}

fn plain_ann(type_id: i64) -> AnnotationDef {
    AnnotationDef {
        type_id,
        type_name: String::new(),
        attrs: Vec::new(),
    }
}

fn field(name: &str, type_id: i64, cp: bool, anns: Vec<AnnotationDef>) -> FieldDef {
    FieldDef {
        name: name.to_string(),
        type_id,
        dimension: 0,
        constant_pool: cp,
        annotations: anns,
        is_setting: false,
    }
}

fn array_field(name: &str, type_id: i64, cp: bool, anns: Vec<AnnotationDef>) -> FieldDef {
    FieldDef {
        dimension: 1,
        ..field(name, type_id, cp, anns)
    }
}

/// 内置的事件/类型定义（写侧的唯一真相源）。
pub fn builtin_metadata() -> Metadata {
    let mut types: Vec<TypeDef> = Vec::new();

    let prim = |id: i64, name: &str| TypeDef {
        id,
        name: name.to_string(),
        ..Default::default()
    };
    types.push(prim(T_LONG, "long"));
    types.push(prim(T_INT, "int"));
    types.push(prim(T_BOOLEAN, "boolean"));
    types.push(prim(T_STRING, "java.lang.String"));
    types.push(prim(T_FLOAT, "float"));
    types.push(prim(T_DOUBLE, "double"));
    types.push(prim(T_SHORT, "short"));
    types.push(prim(T_BYTE, "byte"));
    types.push(prim(T_CHAR, "char"));

    // 注解类型：值类型必须定义，否则真 jfr 解析元数据会报 Type not defined
    let annotation = |id: i64, name: &str, fields: Vec<FieldDef>| TypeDef {
        id,
        name: name.to_string(),
        super_type: Some(SUPER_ANNOTATION.to_string()),
        fields,
        ..Default::default()
    };
    // 内容类型的注解类型本身要带 `@ContentType`（见 A_CONTENT_TYPE 的注释）
    let content_annotation = |id: i64, name: &str, fields: Vec<FieldDef>| TypeDef {
        annotations: vec![plain_ann(A_CONTENT_TYPE)],
        ..annotation(id, name, fields)
    };
    types.push(annotation(
        A_LABEL,
        "jdk.jfr.Label",
        vec![field("value", T_STRING, false, vec![])],
    ));
    types.push(annotation(
        A_DESCRIPTION,
        "jdk.jfr.Description",
        vec![field("value", T_STRING, false, vec![])],
    ));
    types.push(content_annotation(
        A_TIMESTAMP,
        "jdk.jfr.Timestamp",
        vec![field("value", T_STRING, false, vec![])],
    ));
    types.push(content_annotation(
        A_TIMESPAN,
        "jdk.jfr.Timespan",
        vec![field("value", T_STRING, false, vec![])],
    ));
    types.push(annotation(
        A_CATEGORY,
        "jdk.jfr.Category",
        vec![array_field("value", T_STRING, false, vec![])],
    ));
    types.push(annotation(A_UNSIGNED, "jdk.jfr.Unsigned", vec![]));
    // `@ContentType` 是「标记注解的注解」：被它标记的注解类型会被
    // `ValueDescriptor.getContentType()` 认成内容类型。
    // 少了它，真 jfr 拿到 `@Timestamp` 字段时不会走时间格式化，
    // 直接把 `OffsetDateTime.toString()` 打出来 —— 也就是一串 ISO-8601。
    types.push(annotation(A_CONTENT_TYPE, "jdk.jfr.ContentType", vec![]));

    types.push(content_annotation(A_MEMORY_ADDRESS, "jdk.jfr.MemoryAddress", vec![]));
    types.push(annotation(
        A_STACK_TRACE,
        "jdk.jfr.StackTrace",
        vec![field("value", T_BOOLEAN, false, vec![])],
    ));

    // 结构体：名字一个都不能改（见 model.rs 顶部的表）
    types.push(TypeDef {
        id: T_THREAD,
        name: crate::model::TYPE_THREAD.to_string(),
        fields: vec![
            field("osName", T_STRING, false, vec![ann(A_LABEL, "OS Thread Name")]),
            field("osThreadId", T_LONG, false, vec![ann(A_LABEL, "OS Thread Id")]),
            field("javaName", T_STRING, false, vec![ann(A_LABEL, "Java Thread Name")]),
            field("javaThreadId", T_LONG, false, vec![ann(A_LABEL, "Java Thread Id")]),
            field("virtual", T_BOOLEAN, false, vec![ann(A_LABEL, "Virtual Thread")]),
        ],
        ..Default::default()
    });
    types.push(TypeDef {
        id: T_CLASS_LOADER,
        name: crate::model::TYPE_CLASS_LOADER.to_string(),
        fields: vec![
            field("type", T_CLASS, true, vec![ann(A_LABEL, "Type")]),
            field("name", T_STRING, false, vec![ann(A_LABEL, "Name")]),
            field("id", T_LONG, false, vec![ann(A_LABEL, "Id")]),
        ],
        ..Default::default()
    });
    types.push(TypeDef {
        id: T_CLASS,
        name: crate::model::TYPE_CLASS.to_string(),
        fields: vec![
            field("classLoader", T_CLASS_LOADER, true, vec![ann(A_LABEL, "Class Loader")]),
            field("name", T_STRING, false, vec![ann(A_LABEL, "Name")]),
            field("modifiers", T_INT, false, vec![ann(A_LABEL, "Access Modifiers")]),
            field("hidden", T_BOOLEAN, false, vec![ann(A_LABEL, "Hidden")]),
        ],
        ..Default::default()
    });
    types.push(TypeDef {
        id: T_METHOD,
        name: crate::model::TYPE_METHOD.to_string(),
        fields: vec![
            field("type", T_CLASS, true, vec![ann(A_LABEL, "Type")]),
            field("name", T_STRING, false, vec![ann(A_LABEL, "Name")]),
            field("descriptor", T_STRING, false, vec![ann(A_LABEL, "Descriptor")]),
            field("modifiers", T_INT, false, vec![ann(A_LABEL, "Access Modifiers")]),
            field("hidden", T_BOOLEAN, false, vec![ann(A_LABEL, "Hidden")]),
        ],
        ..Default::default()
    });
    types.push(TypeDef {
        id: T_STACK_FRAME,
        name: crate::model::TYPE_STACK_FRAME.to_string(),
        fields: vec![
            field("method", T_METHOD, true, vec![ann(A_LABEL, "Java Method")]),
            field("lineNumber", T_INT, false, vec![ann(A_LABEL, "Line Number")]),
            field("bytecodeIndex", T_INT, false, vec![ann(A_LABEL, "Bytecode Index")]),
            field("type", T_STRING, false, vec![ann(A_LABEL, "Frame Type")]),
        ],
        ..Default::default()
    });
    types.push(TypeDef {
        id: T_STACK_TRACE,
        name: crate::model::TYPE_STACK_TRACE.to_string(),
        fields: vec![
            field("truncated", T_BOOLEAN, false, vec![ann(A_LABEL, "Truncated")]),
            // frames **不能**进常量池：真文件里它是内联结构体数组（`cp=false`）。
            // 进了池之后元素会被 `ObjectFactory` 包成 `RecordedFrame`，而
            // `RecordedObject.structifyArray` 期望拿到裸 `Object[]` 再自己包一层，
            // 于是真 jfr 直接抛 ClassCastException（`RecordedFrame cannot be cast to [Ljava.lang.Object;`）。
            array_field("frames", T_STACK_FRAME, false, vec![ann(A_LABEL, "Stack Frames")]),
        ],
        ..Default::default()
    });

    // 事件
    let start_time = || field("startTime", T_LONG, false, vec![ann(A_LABEL, "Start Time"), ann(A_TIMESTAMP, "TICKS")]);
    let duration = || field("duration", T_LONG, false, vec![ann(A_LABEL, "Duration"), ann(A_TIMESPAN, "TICKS")]);
    let event_thread = || {
        field(
            "eventThread",
            T_THREAD,
            true,
            vec![
                ann(A_LABEL, "Event Thread"),
                ann(A_DESCRIPTION, "Thread in which event was committed in"),
            ],
        )
    };
    let stack_trace = || {
        field(
            "stackTrace",
            T_STACK_TRACE,
            true,
            vec![
                ann(A_LABEL, "Stack Trace"),
                ann(A_DESCRIPTION, "Stack Trace starting from the method the event was committed in"),
            ],
        )
    };
    let event = |id: i64, name: &str, label: &str, cats: &[&str], fields: Vec<FieldDef>| -> TypeDef {
        TypeDef {
            id,
            name: name.to_string(),
            super_type: Some(SUPER_EVENT.to_string()),
            annotations: vec![
                ann_multi(A_CATEGORY, cats),
                ann(A_LABEL, label),
                // `(true)`：真 JFR 的事件都带这个，缺了真 jfr 会显示 false
                ann(A_STACK_TRACE, "true"),
            ],
            fields,
            ..Default::default()
        }
    };

    types.push(event(
        E_EXECUTION_SAMPLE,
        "jdk.ExecutionSample",
        "Execution Sample",
        &["Java Virtual Machine", "Profiling"],
        vec![
            start_time(),
            field("sampledThread", T_THREAD, true, vec![ann(A_LABEL, "Thread")]),
            stack_trace(),
            field("state", T_STRING, false, vec![ann(A_LABEL, "Thread State")]),
        ],
    ));
    types.push(event(
        E_GC_PHASE_PAUSE,
        "jdk.GCPhasePause",
        "GC Phase Pause",
        &["Java Virtual Machine", "GC"],
        vec![
            start_time(),
            duration(),
            event_thread(),
            field("gcId", T_INT, false, vec![plain_ann(A_UNSIGNED), ann(A_LABEL, "GC Identifier")]),
            field("name", T_STRING, false, vec![ann(A_LABEL, "Name")]),
        ],
    ));
    types.push(event(
        E_JAVA_MONITOR_ENTER,
        "jdk.JavaMonitorEnter",
        "Java Monitor Enter",
        &["Java Virtual Machine", "Synchronization"],
        vec![
            start_time(),
            duration(),
            event_thread(),
            stack_trace(),
            field("monitorClass", T_CLASS, true, vec![ann(A_LABEL, "Monitor Class")]),
            field("previousOwner", T_THREAD, true, vec![ann(A_LABEL, "Previous Monitor Owner")]),
            field(
                "address",
                T_LONG,
                false,
                vec![plain_ann(A_UNSIGNED), plain_ann(A_MEMORY_ADDRESS), ann(A_LABEL, "Monitor Address")],
            ),
        ],
    ));
    types.push(event(
        E_THREAD_START,
        "jdk.ThreadStart",
        "Java Thread Start",
        &["Java Virtual Machine", "Thread"],
        vec![
            start_time(),
            event_thread(),
            stack_trace(),
            field("thread", T_THREAD, true, vec![ann(A_LABEL, "New Java Thread")]),
            field("parentThread", T_THREAD, true, vec![ann(A_LABEL, "Parent Java Thread")]),
        ],
    ));
    types.push(event(
        E_THREAD_END,
        "jdk.ThreadEnd",
        "Java Thread End",
        &["Java Virtual Machine", "Thread"],
        vec![
            start_time(),
            event_thread(),
            field("thread", T_THREAD, true, vec![ann(A_LABEL, "Java Thread")]),
        ],
    ));
    types.push(event(
        E_CLASS_LOAD,
        "jdk.ClassLoad",
        "Class Load",
        &["Java Virtual Machine", "Java Application"],
        vec![
            start_time(),
            duration(),
            event_thread(),
            stack_trace(),
            field("loadedClass", T_CLASS, true, vec![ann(A_LABEL, "Loaded Class")]),
            field("definingClassLoader", T_CLASS_LOADER, true, vec![ann(A_LABEL, "Defining Class Loader")]),
            field("initiatingClassLoader", T_CLASS_LOADER, true, vec![ann(A_LABEL, "Initiating Class Loader")]),
        ],
    ));

    let gmt = local_gmt_offset_ms();
    Metadata::build(types, gmt, 0, "zh_CN")
}

/// 本机时区偏移（毫秒）。
///
/// 无依赖前提下不做完整 tz 数据库解析：Windows 上读 `GetTimeZoneInformation`，
/// 其它平台退回 0（UTC）。这个值只影响 `print` 里时间的显示偏移，
/// 写进元数据的 `gmtOffset` 后，读侧用的也是它 —— 自洽。
fn local_gmt_offset_ms() -> i64 {
    #[cfg(windows)]
    {
        #[repr(C)]
        struct SystemTime {
            w_year: u16,
            w_month: u16,
            w_day_of_week: u16,
            w_day: u16,
            w_hour: u16,
            w_minute: u16,
            w_second: u16,
            w_milliseconds: u16,
        }
        #[repr(C)]
        struct TimeZoneInformation {
            bias: i32,
            standard_name: [u16; 32],
            standard_date: SystemTime,
            standard_bias: i32,
            daylight_name: [u16; 32],
            daylight_date: SystemTime,
            daylight_bias: i32,
        }
        extern "system" {
            fn GetTimeZoneInformation(tzi: *mut TimeZoneInformation) -> u32;
        }
        unsafe {
            let mut tzi: TimeZoneInformation = std::mem::zeroed();
            // 返回 0xFFFFFFFF 表示失败
            let r = GetTimeZoneInformation(&mut tzi);
            if r == u32::MAX {
                return 0;
            }
            // Bias 是「UTC = local + bias」的分钟数，所以要取反
            let mut minutes = -(tzi.bias as i64);
            if r == 2 {
                // TIME_ZONE_ID_DAYLIGHT
                minutes -= tzi.daylight_bias as i64;
            }
            minutes * 60_000
        }
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// 墙钟纳秒。
pub fn now_nanos() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i64,
        Err(_) => 0,
    }
}

/// 录制配置。
#[derive(Debug, Clone)]
pub struct RecorderConfig {
    /// 环形缓冲容量：超过就丢最旧的（与 JFR 的 maxSize 行为一致）。
    pub max_events: usize,
    /// 起点墙钟纳秒；`None` = 现在。
    pub start_nanos: Option<i64>,
}

impl Default for RecorderConfig {
    fn default() -> Self {
        RecorderConfig {
            max_events: 100_000,
            start_nanos: None,
        }
    }
}

/// 一个线程的标识。
#[derive(Debug, Clone, Default)]
pub struct ThreadInfo {
    pub java_name: String,
    pub java_id: i64,
    pub os_name: String,
    pub os_id: i64,
    pub virtual_thread: bool,
}

/// 一个栈帧。
#[derive(Debug, Clone, Default)]
pub struct FrameInfo {
    pub class_name: String,
    pub method_name: String,
    /// JVM 描述符，例如 `(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;`
    pub descriptor: String,
    pub line: i32,
    pub bci: i32,
    pub modifiers: i32,
    /// `Interpreted` / `JIT compiled` / `Inlined`
    pub frame_type: String,
}

/// 一个类（用于 ClassLoad / monitorClass 等字段）。
#[derive(Debug, Clone, Default)]
pub struct ClassInfo {
    pub name: String,
    pub modifiers: i32,
    pub hidden: bool,
    pub loader_name: Option<String>,
    pub loader_id: i64,
}

/// 录制器。
#[derive(Debug)]
pub struct Recorder {
    metadata: Metadata,
    cfg: RecorderConfig,
    ring: VecDeque<EventRecord>,
    total: u64,
    dropped: u64,
    start_nanos: i64,
    last_nanos: i64,
}

impl Recorder {
    /// 开始一次录制。
    pub fn start(cfg: RecorderConfig) -> Recorder {
        let metadata = builtin_metadata();
        let start_nanos = cfg.start_nanos.unwrap_or_else(now_nanos);
        Recorder {
            metadata,
            cfg,
            ring: VecDeque::new(),
            total: 0,
            dropped: 0,
            start_nanos,
            last_nanos: start_nanos,
        }
    }

    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    pub fn start_nanos(&self) -> i64 {
        self.start_nanos
    }

    pub fn last_nanos(&self) -> i64 {
        self.last_nanos
    }

    pub fn event_count(&self) -> u64 {
        self.total
    }

    pub fn dropped_count(&self) -> u64 {
        self.dropped
    }

    pub fn config(&self) -> &RecorderConfig {
        &self.cfg
    }

    /// 记录一个事件（开始时间 = 现在）。
    pub fn emit(&mut self, type_name: &str, values: &[(&str, Value)]) -> Result<()> {
        self.emit_at(type_name, None, None, values)
    }

    /// 记录一个事件（可指定开始时间与时长，纳秒）。
    pub fn emit_at(
        &mut self,
        type_name: &str,
        start_nanos: Option<i64>,
        duration_nanos: Option<i64>,
        values: &[(&str, Value)],
    ) -> Result<()> {
        let type_id = match self.metadata.type_id_by_name(type_name) {
            Some(id) => id,
            None => crate::bail!("没有这个事件类型：{}", type_name),
        };
        let t = match self.metadata.type_by_id(type_id) {
            Some(t) => t.clone(),
            None => crate::bail!("事件类型 {} 没有定义", type_name),
        };
        let start = start_nanos.unwrap_or_else(now_nanos);
        if start > self.last_nanos {
            self.last_nanos = start;
        }
        let dur = duration_nanos.unwrap_or(0).max(0);
        let start_ticks = start.saturating_sub(self.start_nanos);
        let mut vals = vec![Value::Null; t.fields.len()];
        if let Some(f0) = t.fields.first() {
            if f0.name != "startTime" {
                crate::bail!("事件 {} 的第一个字段不是 startTime", type_name);
            }
        }
        vals[0] = Value::Int(start_ticks);
        if t.has_duration() {
            vals[1] = Value::Int(dur);
        }
        for (i, f) in t.fields.iter().enumerate().skip(t.start_index()) {
            match values.iter().find(|(n, _)| *n == f.name) {
                Some((_, v)) => vals[i] = v.clone(),
                None => {
                    // 没给就填默认值：CP 字段填 null，其余填 0/false/""
                    vals[i] = default_value(&self.metadata, f);
                }
            }
        }
        // 给了类型里没有的字段：明确报错，不要静默丢
        for (n, _) in values {
            if t.field(n).is_none() {
                crate::bail!("事件 {} 没有字段 {}", type_name, n);
            }
        }
        let rec = EventRecord {
            type_id,
            start_ticks,
            duration_ticks: dur,
            values: vals,
        };
        self.total += 1;
        if self.ring.len() >= self.cfg.max_events.max(1) {
            self.ring.pop_front();
            self.dropped += 1;
        }
        self.ring.push_back(rec);
        Ok(())
    }

    /// `jdk.ExecutionSample` 的便捷入口。
    pub fn execution_sample(
        &mut self,
        thread: &ThreadInfo,
        state: &str,
        frames: &[FrameInfo],
    ) -> Result<()> {
        let thread_v = thread_value(&self.metadata, thread);
        let trace = self.stack_trace_value(frames);
        self.emit(
            "jdk.ExecutionSample",
            &[
                ("sampledThread", thread_v),
                ("stackTrace", trace),
                ("state", Value::Str(state.to_string())),
            ],
        )
    }

    /// `jdk.GCPhasePause` 的便捷入口。
    /// `thread` 是发起这次 GC 的线程（真 JFR 里是 `VM Thread`）。
    /// 不传一个真实线程会让 `eventThread` 打印成 `"" (osThreadId = 0)` —— 虽然合法，
    /// 但一眼就知道是合成数据。
    pub fn gc_phase_pause(
        &mut self,
        thread: &ThreadInfo,
        gc_id: i64,
        name: &str,
        start_nanos: i64,
        duration_nanos: i64,
    ) -> Result<()> {
        let thread_v = thread_value(&self.metadata, thread);
        self.emit_at(
            "jdk.GCPhasePause",
            Some(start_nanos),
            Some(duration_nanos),
            &[
                ("eventThread", thread_v),
                ("gcId", Value::Int(gc_id)),
                ("name", Value::Str(name.to_string())),
            ],
        )
    }

    /// `jdk.JavaMonitorEnter` 的便捷入口。
    pub fn java_monitor_enter(
        &mut self,
        thread: &ThreadInfo,
        monitor_class: &ClassInfo,
        previous_owner: Option<&ThreadInfo>,
        address: i64,
        start_nanos: i64,
        duration_nanos: i64,
        frames: &[FrameInfo],
    ) -> Result<()> {
        let thread_v = thread_value(&self.metadata, thread);
        let class_v = class_value(&self.metadata, monitor_class);
        let prev = match previous_owner {
            Some(t) => thread_value(&self.metadata, t),
            None => Value::Null,
        };
        let trace = self.stack_trace_value(frames);
        self.emit_at(
            "jdk.JavaMonitorEnter",
            Some(start_nanos),
            Some(duration_nanos),
            &[
                ("eventThread", thread_v),
                ("stackTrace", trace),
                ("monitorClass", class_v),
                ("previousOwner", prev),
                ("address", Value::Int(address)),
            ],
        )
    }

    /// `jdk.ThreadStart` 的便捷入口。
    pub fn thread_start(&mut self, parent: &ThreadInfo, child: &ThreadInfo, frames: &[FrameInfo]) -> Result<()> {
        let parent_v = thread_value(&self.metadata, parent);
        let child_v = thread_value(&self.metadata, child);
        let trace = self.stack_trace_value(frames);
        self.emit(
            "jdk.ThreadStart",
            &[
                ("eventThread", parent_v.clone()),
                ("stackTrace", trace),
                ("thread", child_v),
                ("parentThread", parent_v),
            ],
        )
    }

    /// `jdk.ThreadEnd` 的便捷入口。
    pub fn thread_end(&mut self, thread: &ThreadInfo) -> Result<()> {
        let thread_v = thread_value(&self.metadata, thread);
        self.emit(
            "jdk.ThreadEnd",
            &[("eventThread", thread_v.clone()), ("thread", thread_v)],
        )
    }

    /// `jdk.ClassLoad` 的便捷入口。
    pub fn class_load(
        &mut self,
        loaded: &ClassInfo,
        defining: Option<&ClassInfo>,
        initiating: Option<&ClassInfo>,
        frames: &[FrameInfo],
    ) -> Result<()> {
        let trace = self.stack_trace_value(frames);
        self.emit(
            "jdk.ClassLoad",
            &[
                ("loadedClass", class_value(&self.metadata, loaded)),
                ("definingClassLoader", loader_value_of(&self.metadata, defining)),
                ("initiatingClassLoader", loader_value_of(&self.metadata, initiating)),
                ("stackTrace", trace),
            ],
        )
    }

    /// 由帧列表生成 `jdk.types.StackTrace` 值。
    pub fn stack_trace_value(&self, frames: &[FrameInfo]) -> Value {
        let frame_vals: Vec<Value> = frames
            .iter()
            .map(|f| {
                Value::Struct {
                    type_id: T_STACK_FRAME,
                    cp_id: 0,
                fields: vec![
                        method_value(&self.metadata, f),
                        Value::Int(f.line as i64),
                        Value::Int(f.bci as i64),
                        Value::Str(if f.frame_type.is_empty() {
                            "Interpreted".to_string()
                        } else {
                            f.frame_type.clone()
                        }),
                    ],
                }
            })
            .collect();
        Value::Struct {
            type_id: T_STACK_TRACE,
            cp_id: 0,
        fields: vec![Value::Bool(false), Value::Array(frame_vals)],
        }
    }

    /// 组装成 chunk（`dump` 之前可以先看一眼）。
    pub fn to_chunk(&self) -> ChunkSpec {
        ChunkSpec {
            metadata: self.metadata.clone(),
            events: self.ring.iter().cloned().collect(),
            start_nanos: self.start_nanos,
            duration_nanos: (self.last_nanos - self.start_nanos).max(0),
            metadata_id: 1,
        }
    }

    /// 序列化成字节。
    pub fn dump_bytes(&self) -> Result<Vec<u8>> {
        crate::writer::build(&self.to_chunk())
    }

    /// 落盘，返回字节数。
    pub fn dump(&self, path: impl AsRef<Path>) -> Result<u64> {
        crate::writer::write_file(path, &self.to_chunk())
    }

    /// 已录事件（按时间顺序）。
    pub fn events(&self) -> &VecDeque<EventRecord> {
        &self.ring
    }
}

fn default_value(meta: &Metadata, f: &FieldDef) -> Value {
    if f.constant_pool || f.dimension > 0 {
        return Value::Null;
    }
    let name = meta.type_name(f.type_id);
    match name.as_str() {
        "int" | "long" | "short" | "byte" | "char" => Value::Int(0),
        "boolean" => Value::Bool(false),
        "float" => Value::Float(0.0),
        "double" => Value::Double(0.0),
        "java.lang.String" => Value::Str(String::new()),
        _ => Value::Null,
    }
}

/// 构造 `java.lang.Thread` 值。
pub fn thread_value(_meta: &Metadata, t: &ThreadInfo) -> Value {
    Value::Struct {
        type_id: T_THREAD,
        cp_id: 0,
    fields: vec![
            Value::Str(t.os_name.clone()),
            Value::Int(t.os_id),
            Value::Str(t.java_name.clone()),
            Value::Int(t.java_id),
            Value::Bool(t.virtual_thread),
        ],
    }
}

/// 构造 `java.lang.Class` 值。
pub fn class_value(_meta: &Metadata, c: &ClassInfo) -> Value {
    let loader = match &c.loader_name {
        Some(name) => Value::Struct {
            type_id: T_CLASS_LOADER,
            cp_id: 0,
            fields: vec![
                Value::Null, // type（ClassLoader 自己的 Class，留空避免自引用）
                Value::Str(name.clone()),
                Value::Int(c.loader_id),
            ],
        },
        None => Value::Null,
    };
    Value::Struct {
        type_id: T_CLASS,
        cp_id: 0,
    fields: vec![
            loader,
            Value::Str(c.name.clone()),
            Value::Int(c.modifiers as i64),
            Value::Bool(c.hidden),
        ],
    }
}

/// 只取「类加载器」那一层（`ClassLoad` 的 defining/initiating 字段用它）。
fn loader_value_of(_meta: &Metadata, c: Option<&ClassInfo>) -> Value {
    match c {
        Some(c) => Value::Struct {
            type_id: T_CLASS_LOADER,
            cp_id: 0,
            fields: vec![
                Value::Null,
                Value::Str(c.loader_name.clone().unwrap_or_else(|| c.name.clone())),
                Value::Int(c.loader_id),
            ],
        },
        None => Value::Null,
    }
}

fn method_value(_meta: &Metadata, f: &FrameInfo) -> Value {
    Value::Struct {
        type_id: T_METHOD,
        cp_id: 0,
    fields: vec![
            Value::Struct {
                type_id: T_CLASS,
                cp_id: 0,
            fields: vec![
                    Value::Null, // classLoader：帧里的类不追 loader，保持 null
                    Value::Str(f.class_name.clone()),
                    Value::Int(f.modifiers as i64),
                    Value::Bool(false),
                ],
            },
            Value::Str(f.method_name.clone()),
            Value::Str(f.descriptor.clone()),
            Value::Int(f.modifiers as i64),
            Value::Bool(false),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_metadata_is_consistent() {
        let m = builtin_metadata();
        // 每个事件的第一个字段必须是 startTime
        for id in &m.event_type_ids {
            let t = m.type_by_id(*id).unwrap();
            assert_eq!(t.fields[0].name, "startTime", "{}", t.name);
            if t.fields.len() > 1 && t.fields[1].name == "duration" {
                assert_eq!(t.start_index(), 2);
            }
        }
        // 所有字段的类型必须定义
        for t in &m.types {
            for f in t.fields.iter().chain(t.settings.iter()) {
                assert!(m.type_by_id(f.type_id).is_some(), "{}.{} 类型缺失", t.name, f.name);
            }
            for a in &t.annotations {
                assert!(m.type_by_id(a.type_id).is_some(), "{} 注解类型缺失", t.name);
            }
        }
        // 关键类型名不能改
        assert!(m.type_id_by_name(crate::model::TYPE_THREAD).is_some());
        assert!(m.type_by_name(crate::model::TYPE_THREAD).unwrap().simple_name() == "Thread");
    }

    #[test]
    fn record_and_dump() {
        let mut r = Recorder::start(RecorderConfig {
            max_events: 10,
            start_nanos: Some(1_700_000_000_000_000_000),
        });
        let t = ThreadInfo {
            java_name: "main".into(),
            java_id: 1,
            os_name: "main".into(),
            os_id: 111,
            virtual_thread: false,
        };
        let frames = vec![FrameInfo {
            class_name: "demo.App".into(),
            method_name: "main".into(),
            descriptor: "([Ljava/lang/String;)V".into(),
            line: 12,
            bci: 4,
            modifiers: 9,
            frame_type: "Interpreted".into(),
        }];
        r.execution_sample(&t, "STATE_RUNNABLE", &frames).unwrap();
        r.gc_phase_pause(&t, 3, "Young Pause", 1_700_000_001_000_000_000, 4_038_000)
            .unwrap();
        r.thread_start(&t, &t, &frames).unwrap();
        r.thread_end(&t).unwrap();
        r.class_load(
            &ClassInfo {
                name: "demo.App".into(),
                loader_name: Some("app".into()),
                loader_id: 1,
                ..Default::default()
            },
            Some(&ClassInfo {
                name: "demo.App".into(),
                loader_name: Some("app".into()),
                loader_id: 1,
                ..Default::default()
            }),
            None,
            &frames,
        )
        .unwrap();
        assert_eq!(r.event_count(), 5);
        let bytes = r.dump_bytes().unwrap();
        let (types, events) = crate::writer::verify(&bytes).unwrap();
        assert!(types > 10);
        assert_eq!(events, 5);
    }

    #[test]
    fn ring_buffer_drops_oldest() {
        let mut r = Recorder::start(RecorderConfig {
            max_events: 3,
            start_nanos: Some(0),
        });
        let t = ThreadInfo::default();
        for _ in 0..10 {
            r.thread_end(&t).unwrap();
        }
        assert_eq!(r.event_count(), 10);
        assert_eq!(r.dropped_count(), 7);
        assert_eq!(r.events().len(), 3);
    }

    #[test]
    fn unknown_event_or_field_is_error() {
        let mut r = Recorder::start(RecorderConfig::default());
        assert!(r.emit("jdk.Nope", &[]).is_err());
        assert!(r.emit("jdk.ThreadEnd", &[("bogus", Value::Int(1))]).is_err());
    }
}
