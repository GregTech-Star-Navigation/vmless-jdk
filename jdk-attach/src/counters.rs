//! 计数器定义与快照。
//!
//! **名字全部照抄 HotSpot**（用真 JDK 25 的 perfdata 文件逐条核对过 191 个计数器），
//! 因为 `jstat` 按这些名字取数 —— 名字对了，真 jstat 读我们的 VM 就能算出正确的列。
//!
//! 单位约定（与 HotSpot 一致）：
//! - 时间类计数器写**纳秒**，`sun.os.hrt.frequency = 1e9`，所以 `jstat` 除一下就是秒。
//! - 容量类计数器写**字节**，`jstat -gc` 自己除 1024 显示成 KB。
//!
//! 值不是估算：`capacity/used/committed` 来自 VM 的堆管理器，`YGC/YGCT` 来自 GC 记账，
//! `loaded/bytes` 来自类加载器，`compiled/time` 来自 JIT。VM 侧只需实现 [`VmProbe`]。

use crate::perf::{PerfValue, Sample, Units, Var};

/// 一次采样里的一块空间（eden / survivor / old / metaspace …）。
#[derive(Debug, Clone, Default)]
pub struct SpaceStat {
    /// HotSpot 里的空间名，例如 `Eden Space`
    pub name: String,
    pub capacity: i64,
    pub used: i64,
    pub max_capacity: i64,
    pub init_capacity: i64,
}

impl SpaceStat {
    pub fn new(name: impl Into<String>, capacity: i64, used: i64, max: i64, init: i64) -> SpaceStat {
        SpaceStat { name: name.into(), capacity, used, max_capacity: max, init_capacity: init }
    }
}

/// 一个分代（generation 0 = young，generation 1 = old）。
#[derive(Debug, Clone, Default)]
pub struct GenStat {
    pub spaces: Vec<SpaceStat>,
}

/// 一个 GC 收集器的记账。
#[derive(Debug, Clone, Default)]
pub struct GcStat {
    /// 例如 `G1 Young Generation`
    pub name: String,
    pub invocations: i64,
    /// 累计耗时（纳秒）
    pub time_ns: i64,
    pub last_entry_ns: i64,
    pub last_exit_ns: i64,
}

/// TLAB 统计（可选，`jstat -gc` 不用它，但 `PerfCounter.print` 会列）。
#[derive(Debug, Clone, Default)]
pub struct TlabStat {
    pub alloc_threads: i64,
    pub fills: i64,
    pub max_fills: i64,
    pub alloc: i64,
    pub gc_waste: i64,
    pub max_gc_waste: i64,
    pub refill_waste: i64,
    pub max_refill_waste: i64,
    pub slow_alloc: i64,
    pub max_slow_alloc: i64,
}

/// 一次完整的计数器快照 —— **VM 只需要填这个结构**。
#[derive(Debug, Clone, Default)]
pub struct PerfSnapshot {
    pub uptime_ms: u64,
    // 线程
    pub threads_started: i64,
    pub threads_live: i64,
    pub threads_daemon: i64,
    pub threads_peak: i64,
    // 类
    pub classes_loaded: i64,
    pub classes_unloaded: i64,
    /// 共享类（CDS）—— 我们没有，填 0 即可（但必须存在，见上）
    pub classes_shared_loaded: i64,
    pub class_bytes_shared: i64,
    pub class_bytes: i64,
    pub class_bytes_unloaded: i64,
    pub method_bytes: i64,
    pub class_time_ns: i64,
    pub class_init_time_ns: i64,
    pub class_verify_time_ns: i64,
    pub class_link_time_ns: i64,
    pub classes_initialized: i64,
    pub classes_linked: i64,
    pub classes_verified: i64,
    // JIT
    pub jit_compiled: i64,
    pub jit_failed: i64,
    pub jit_invalid: i64,
    pub jit_time_ns: i64,
    pub jit_failed_type: i64,
    pub jit_failed_method: String,
    // safepoint / 时间
    pub safepoints: i64,
    pub safepoint_time_ns: i64,
    pub safepoint_sync_time_ns: i64,
    pub application_time_ns: i64,
    // 内存
    pub generations: Vec<GenStat>,
    pub collectors: Vec<GcStat>,
    pub metaspace: SpaceStat,
    pub compressed_class: SpaceStat,
    pub tlab: Option<TlabStat>,
    /// VM 自己的额外计数器（名字必须唯一；出现一次后一直要在）
    pub extra: Vec<(String, PerfValue, Units, Var)>,
}

/// 只在启动时确定的字符串/常量计数器。
#[derive(Debug, Clone)]
pub struct VmStrings {
    /// `主类 参数...`（`sun.rt.javaCommand`，jps 靠它显示主类）
    pub java_command: String,
    /// 完整的 `-X` / `-D` / `-XX` 参数（`java.rt.vmArgs`，jps -v 靠它）
    pub vm_args: String,
    /// `-XX` 部分（`java.rt.vmFlags`）
    pub vm_flags: String,
    // 时间点（Unix 毫秒）
    pub created_ms: u64,
    pub vm_init_done_ms: u64,
    /// `jvmVersion` 编码：major<<24 | minor<<16 | micro<<8 | patch
    pub jvm_version: i64,
    pub jvm_capabilities: String,
    pub policy_name: String,
    pub system_properties: Vec<(String, String)>,
    // attach 端点的两个计数器（**不用单独的控制文件**，见 server.rs 的说明）
    pub attach_port: u16,
    pub attach_token: String,
}

impl VmStrings {
    pub fn samples(&self) -> Vec<Sample> {
        let mut v = Vec::with_capacity(32);
        let l = |n: &str, val: i64, u: Units, var: Var| Sample::long(n, val, u, var);
        v.push(l("sun.rt.createVmBeginTime", self.created_ms as i64, Units::None, Var::Variable));
        v.push(l("sun.rt.createVmEndTime", self.created_ms as i64 + 1, Units::None, Var::Variable));
        v.push(l("sun.rt.vmInitDoneTime", self.vm_init_done_ms as i64, Units::None, Var::Variable));
        v.push(l("sun.rt.jvmVersion", self.jvm_version, Units::None, Var::Constant));
        v.push(Sample::string("sun.rt.jvmCapabilities", self.jvm_capabilities.clone(), Var::Constant));
        v.push(Sample::string("sun.rt.javaCommand", self.java_command.clone(), Var::Constant));
        v.push(Sample::string("java.rt.vmArgs", self.vm_args.clone(), Var::Constant));
        v.push(Sample::string("java.rt.vmFlags", self.vm_flags.clone(), Var::Constant));
        v.push(Sample::string("sun.gc.policy.name", self.policy_name.clone(), Var::Constant));
        // attach 端点：真 jps/jstat 不认识这两个名字，会直接忽略；我们的工具靠它们连上来
        v.push(Sample::long("vmless.attach.port", self.attach_port as i64, Units::None, Var::Constant));
        v.push(Sample::string("vmless.attach.token", self.attach_token.clone(), Var::Constant));
        for (k, val) in &self.system_properties {
            // 只把短属性写进 perfdata（真 HotSpot 也只挑一部分写），避免文件被撑爆
            if val.len() <= 2048 {
                v.push(Sample::string(&format!("java.property.{}", k), val.clone(), Var::Constant));
            }
        }
        v
    }
}

impl PerfSnapshot {
    /// 数字类计数器 → perfdata 样本。
    pub fn samples(&self) -> Vec<Sample> {
        let mut v = Vec::with_capacity(160);
        let l = |n: String, val: i64, u: Units, var: Var| Sample::long(&n, val, u, var);

        // ---- 时间 / safepoint ----
        v.push(l("sun.os.hrt.frequency".into(), 1_000_000_000, Units::Hertz, Var::Constant));
        v.push(l("sun.rt.applicationTime".into(), self.application_time_ns, Units::Ticks, Var::Monotonic));
        v.push(l("sun.rt.safepoints".into(), self.safepoints, Units::Events, Var::Monotonic));
        v.push(l("sun.rt.safepointTime".into(), self.safepoint_time_ns, Units::Ticks, Var::Monotonic));
        v.push(l("sun.rt.safepointSyncTime".into(), self.safepoint_sync_time_ns, Units::Ticks, Var::Monotonic));

        // ---- 线程 ----
        v.push(l("java.threads.started".into(), self.threads_started, Units::Events, Var::Monotonic));
        v.push(l("java.threads.live".into(), self.threads_live, Units::None, Var::Variable));
        v.push(l("java.threads.livePeak".into(), self.threads_peak, Units::None, Var::Variable));
        v.push(l("java.threads.daemon".into(), self.threads_daemon, Units::None, Var::Variable));

        // ---- 类 ----
        v.push(l("java.cls.loadedClasses".into(), self.classes_loaded, Units::Events, Var::Monotonic));
        // jstat -class 的表达式是 `java.cls.loadedClasses + java.cls.sharedLoadedClasses`，
        // **任何一项缺失整列就变成 "-"**（实测）。所以 shared* 必须存在，没有就是 0。
        v.push(l("java.cls.sharedLoadedClasses".into(), self.classes_shared_loaded, Units::Events, Var::Monotonic));
        v.push(l("java.cls.sharedUnloadedClasses".into(), 0, Units::Events, Var::Monotonic));
        v.push(l("sun.cls.sharedLoadedBytes".into(), self.class_bytes_shared, Units::Bytes, Var::Monotonic));
        v.push(l("sun.cls.sharedUnloadedBytes".into(), 0, Units::Bytes, Var::Monotonic));
        v.push(l("java.cls.unloadedClasses".into(), self.classes_unloaded, Units::Events, Var::Monotonic));
        v.push(l("sun.cls.loadedBytes".into(), self.class_bytes, Units::Bytes, Var::Monotonic));
        v.push(l("sun.cls.unloadedBytes".into(), self.class_bytes_unloaded, Units::Bytes, Var::Monotonic));
        v.push(l("sun.cls.methodBytes".into(), self.method_bytes, Units::Bytes, Var::Variable));
        v.push(l("sun.cls.time".into(), self.class_time_ns, Units::Ticks, Var::Monotonic));
        v.push(l("sun.cls.classInitTime".into(), self.class_init_time_ns, Units::Ticks, Var::Monotonic));
        v.push(l("sun.cls.classVerifyTime".into(), self.class_verify_time_ns, Units::Ticks, Var::Monotonic));
        v.push(l("sun.cls.classLinkedTime".into(), self.class_link_time_ns, Units::Ticks, Var::Monotonic));
        v.push(l("sun.cls.initializedClasses".into(), self.classes_initialized, Units::Events, Var::Monotonic));
        v.push(l("sun.cls.linkedClasses".into(), self.classes_linked, Units::Events, Var::Monotonic));
        v.push(l("sun.cls.verifiedClasses".into(), self.classes_verified, Units::Events, Var::Monotonic));

        // ---- JIT ----
        v.push(l("java.ci.totalTime".into(), self.jit_time_ns, Units::Ticks, Var::Monotonic));
        // 名字必须是 jstat -compiler 用的那三个（见 JDK 里的 jstat option 规格）
        v.push(l("sun.ci.totalCompiles".into(), self.jit_compiled, Units::Events, Var::Monotonic));
        v.push(l("sun.ci.totalBailouts".into(), self.jit_failed, Units::Events, Var::Monotonic));
        v.push(l("sun.ci.totalInvalidates".into(), self.jit_invalid, Units::Events, Var::Monotonic));
        v.push(l("sun.ci.lastFailedType".into(), self.jit_failed_type, Units::None, Var::Variable));
        v.push(Sample::string("sun.ci.lastFailedMethod", self.jit_failed_method.clone(), Var::Variable));

        // ---- GC 收集器 ----
        v.push(l("sun.gc.policy.collectors".into(), self.collectors.len() as i64, Units::None, Var::Constant));
        v.push(l("sun.gc.policy.generations".into(), self.generations.len() as i64, Units::None, Var::Constant));
        for (i, c) in self.collectors.iter().enumerate() {
            v.push(Sample::string(&format!("sun.gc.collector.{}.name", i), c.name.clone(), Var::Constant));
            v.push(l(format!("sun.gc.collector.{}.invocations", i), c.invocations, Units::Events, Var::Monotonic));
            v.push(l(format!("sun.gc.collector.{}.time", i), c.time_ns, Units::Ticks, Var::Monotonic));
            v.push(l(format!("sun.gc.collector.{}.lastEntryTime", i), c.last_entry_ns, Units::Ticks, Var::Variable));
            v.push(l(format!("sun.gc.collector.{}.lastExitTime", i), c.last_exit_ns, Units::Ticks, Var::Variable));
        }

        // ---- 分代 / 空间 ----
        for (g, gen) in self.generations.iter().enumerate() {
            v.push(l(format!("sun.gc.generation.{}.spaces", g), gen.spaces.len() as i64, Units::None, Var::Constant));
            for (s, space) in gen.spaces.iter().enumerate() {
                let p = format!("sun.gc.generation.{}.space.{}", g, s);
                v.push(Sample::string(&format!("{}.name", p), space.name.clone(), Var::Constant));
                v.push(l(format!("{}.maxCapacity", p), space.max_capacity, Units::Bytes, Var::Constant));
                v.push(l(format!("{}.capacity", p), space.capacity, Units::Bytes, Var::Variable));
                v.push(l(format!("{}.used", p), space.used, Units::Bytes, Var::Variable));
                v.push(l(format!("{}.initCapacity", p), space.init_capacity, Units::Bytes, Var::Constant));
            }
        }

        // ---- metaspace / compressed class space ----
        for (prefix, sp) in [
            ("sun.gc.metaspace", &self.metaspace),
            ("sun.gc.compressedclassspace", &self.compressed_class),
        ] {
            v.push(l(format!("{}.minCapacity", prefix), sp.init_capacity, Units::Bytes, Var::Constant));
            v.push(l(format!("{}.maxCapacity", prefix), sp.max_capacity, Units::Bytes, Var::Variable));
            v.push(l(format!("{}.capacity", prefix), sp.capacity, Units::Bytes, Var::Variable));
            v.push(l(format!("{}.used", prefix), sp.used, Units::Bytes, Var::Variable));
        }

        // ---- TLAB ----
        if let Some(t) = &self.tlab {
            v.push(l("sun.gc.tlab.allocThreads".into(), t.alloc_threads, Units::None, Var::Variable));
            v.push(l("sun.gc.tlab.fills".into(), t.fills, Units::None, Var::Variable));
            v.push(l("sun.gc.tlab.maxFills".into(), t.max_fills, Units::None, Var::Variable));
            v.push(l("sun.gc.tlab.alloc".into(), t.alloc, Units::Bytes, Var::Variable));
            v.push(l("sun.gc.tlab.gcWaste".into(), t.gc_waste, Units::Bytes, Var::Variable));
            v.push(l("sun.gc.tlab.maxGcWaste".into(), t.max_gc_waste, Units::Bytes, Var::Variable));
            v.push(l("sun.gc.tlab.refillWaste".into(), t.refill_waste, Units::Bytes, Var::Variable));
            v.push(l("sun.gc.tlab.maxRefillWaste".into(), t.max_refill_waste, Units::Bytes, Var::Variable));
            v.push(l("sun.gc.tlab.slowAlloc".into(), t.slow_alloc, Units::None, Var::Variable));
            v.push(l("sun.gc.tlab.maxSlowAlloc".into(), t.max_slow_alloc, Units::None, Var::Variable));
        }

        for (name, value, units, var) in &self.extra {
            v.push(Sample {
                name: name.clone(),
                value: value.clone(),
                units: *units,
                var: *var,
                supported: true,
            });
        }
        v
    }

    /// `jstat -gc` 列要用的几个派生值，便于工具层与 VM 层共用同一套口径。
    ///
    /// 顺序：S0C S1C S0U S1U EC EU OC OU MC MU CCSC CCSU
    ///
    /// **空间顺序容易搞反**：HotSpot 里 `generation.0.space.0` 是 **Eden**，
    /// `.space.1` 是 Survivor 0，`.space.2` 是 Survivor 1（见 JDK 的 jstat option 规格）。
    pub fn gc_capacity_kb(&self) -> Vec<f64> {
        let kb = |v: i64| v as f64 / 1024.0;
        let g0 = self.generations.first();
        let space = |g: Option<&GenStat>, i: usize| -> SpaceStat {
            g.and_then(|g| g.spaces.get(i)).cloned().unwrap_or_default()
        };
        let s0 = space(g0, 1);
        let s1 = space(g0, 2);
        let ed = space(g0, 0);
        let od = space(self.generations.get(1), 0);
        vec![
            kb(s0.capacity),
            kb(s1.capacity),
            kb(s0.used),
            kb(s1.used),
            kb(ed.capacity),
            kb(ed.used),
            kb(od.capacity),
            kb(od.used),
            kb(self.metaspace.capacity),
            kb(self.metaspace.used),
            kb(self.compressed_class.capacity),
            kb(self.compressed_class.used),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> PerfSnapshot {
        PerfSnapshot {
            uptime_ms: 1000,
            threads_live: 3,
            generations: vec![
                GenStat {
                    // space.0 = Eden, space.1 = S0, space.2 = S1
                    spaces: vec![
                        SpaceStat::new("Eden Space", 8192, 4096, 16384, 0),
                        SpaceStat::new("Survivor Space", 1024, 256, 2048, 0),
                        SpaceStat::new("Survivor Space", 1024, 0, 2048, 0),
                    ],
                },
                GenStat { spaces: vec![SpaceStat::new("Old Space", 16384, 8192, 32768, 0)] },
            ],
            collectors: vec![GcStat { name: "Young".into(), invocations: 5, time_ns: 1_000_000, ..Default::default() }],
            metaspace: SpaceStat::new("Metaspace", 4096, 2048, 8192, 0),
            compressed_class: SpaceStat::new("Compressed Class Space", 1024, 512, 2048, 0),
            ..Default::default()
        }
    }

    #[test]
    fn samples_are_unique_and_named_like_hotspot() {
        let s = snap().samples();
        let names: Vec<&str> = s.iter().map(|x| x.name.as_str()).collect();
        assert!(names.contains(&"sun.gc.collector.0.invocations"));
        assert!(names.contains(&"sun.gc.generation.0.space.2.used"));
        assert!(names.contains(&"sun.os.hrt.frequency"));
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "计数器名字有重复");
    }

    #[test]
    fn gc_kb_columns() {
        let c = snap().gc_capacity_kb();
        assert_eq!(c.len(), 12);
        assert_eq!(c[4], 8.0); // EC = 8192 bytes / 1024
        assert_eq!(c[5], 4.0); // EU
        assert_eq!(c[6], 16.0); // OC
    }
}
