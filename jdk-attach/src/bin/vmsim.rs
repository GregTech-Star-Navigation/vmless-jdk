//! `vmsim` —— 一个**实现了 [`VmProbe`] 的参考 VM**，用来证明 attach 内核确实能把一个
//! 真实进程导出成 `jps`/`jstat`/`jcmd`/`jmap`/`jstack` 能用的目标。
//!
//! 它不是一个空壳：堆是**真的对象表**（`jmap -histo` 逐个对象扫出来的实例数与字节数），
//! GC 是**真的标记-清扫**（`GC.run` 会真的回收、真的改 YGC/YGCT），线程是**真的 OS 线程**
//! （每个线程自己发布当前 Java 栈帧）。
//!
//! 真正需要 VM 那边接的时候，把这个文件里的 `SimVm` 换成 rustjavac 的 VM 即可 ——
//! 接口只有 `VmProbe` 那十几个方法。
//!
//! 用法：
//! ```text
//! vmsim [--main com.example.Main] [--arg x] [--threads 4] [--objects 2000]
//!       [--deadlock] [--gc-interval 1500] [--quiet]
//! ```

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vmless_attach::counters::{GcStat, GenStat, PerfSnapshot, SpaceStat, TlabStat};
use vmless_attach::heap::{ClassDef, ClassHisto, FieldDef, HeapModel, HeapObject, LoaderStat};
use vmless_attach::threads::{Frame, JThread, LockRef, ThreadDump};
use vmless_attach::{AttachOptions, ClassStat, VmIdentity, VmProbe};

// ---------------------------------------------------------------------------
// 类元数据
// ---------------------------------------------------------------------------

struct SimClass {
    name: &'static str,
    super_name: Option<&'static str>,
    /// 每个实例的字节数（按字段累加，8 字节对齐，模仿真实布局）
    size: u32,
    fields: Vec<(&'static str, char, u32)>,
    loader: usize,
    methods: u64,
}

fn classes() -> Vec<SimClass> {
    vec![
        SimClass { name: "java.lang.Object", super_name: None, size: 16, fields: vec![], loader: 0, methods: 14 },
        SimClass {
            name: "com.example.Point",
            super_name: Some("java.lang.Object"),
            size: 24,
            fields: vec![("x", 'I', 4), ("y", 'I', 4), ("tag", 'J', 8)],
            loader: 1,
            methods: 9,
        },
        SimClass {
            name: "com.example.Node",
            super_name: Some("java.lang.Object"),
            size: 32,
            fields: vec![("next", 'J', 8), ("key", 'I', 4), ("value", 'I', 4), ("hash", 'I', 4)],
            loader: 1,
            methods: 12,
        },
        SimClass {
            name: "java.lang.String",
            super_name: Some("java.lang.Object"),
            size: 24,
            fields: vec![("hash", 'I', 4), ("coder", 'B', 1)],
            loader: 0,
            methods: 84,
        },
        SimClass {
            name: "[B",
            super_name: Some("java.lang.Object"),
            size: 16,
            fields: vec![("length", 'I', 4)],
            loader: 0,
            methods: 0,
        },
        SimClass { name: "[I", super_name: Some("java.lang.Object"), size: 16, fields: vec![], loader: 0, methods: 0 },
        SimClass {
            name: "java.util.HashMap$Node",
            super_name: Some("java.lang.Object"),
            size: 32,
            fields: vec![("hash", 'I', 4), ("key", 'J', 8), ("value", 'J', 8)],
            loader: 0,
            methods: 6,
        },
        SimClass {
            name: "com.example.CacheEntry",
            super_name: Some("java.lang.Object"),
            size: 40,
            fields: vec![("key", 'J', 8), ("value", 'J', 8), ("next", 'J', 8), ("stamp", 'J', 8)],
            loader: 1,
            methods: 3,
        },
        SimClass { name: "java.lang.Integer", super_name: Some("java.lang.Object"), size: 16, fields: vec![("value", 'I', 4)], loader: 0, methods: 24 },
        SimClass { name: "[Ljava.lang.Object;", super_name: Some("java.lang.Object"), size: 16, fields: vec![], loader: 0, methods: 0 },
    ]
}

// ---------------------------------------------------------------------------
// 堆：真对象表 + 真标记清扫
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct SimObj {
    class: usize,
    /// 指向别的对象（堆内下标），用于真实的可达性标记
    refs: Vec<usize>,
    /// 字段数据（写进 hprof）
    data: Vec<u8>,
    marked: bool,
}

struct SimHeap {
    objects: Vec<Option<SimObj>>,
    /// 根的堆下标
    roots: Vec<usize>,
    capacity: i64,
    committed: i64,
    /// eden / survivor0 / survivor1 / old 各占多少已用
    eden_used: i64,
    s0_used: i64,
    s1_used: i64,
    /// 类加载器表：loader id → (parent, 类型名, 类数)
    loaders: Vec<(u64, u64, &'static str, u64)>,
}

impl SimHeap {
    fn new() -> SimHeap {
        SimHeap {
            objects: Vec::new(),
            roots: Vec::new(),
            capacity: 64 << 20,
            committed: 16 << 20,
            eden_used: 0,
            s0_used: 0,
            s1_used: 0,
            loaders: vec![
                (0, 0, "<boot class loader>", 6),
                (0x1000_0000, 0, "jdk.internal.loader.ClassLoaders$AppClassLoader", 4),
            ],
        }
    }

    fn live_bytes(&self) -> i64 {
        self.objects
            .iter()
            .flatten()
            .map(|o| classes()[o.class].size as i64)
            .sum()
    }

    fn count(&self) -> usize {
        self.objects.iter().filter(|o| o.is_some()).count()
    }

    fn alloc(&mut self, class: usize, refs: Vec<usize>) -> usize {
        let size = classes()[class].size as i64;
        let data = vec![0u8; size as usize];
        let o = SimObj { class, refs, data, marked: false };
        self.eden_used += size;
        self.objects.push(Some(o));
        self.objects.len() - 1
    }

    /// 真的标记-清扫。返回 (回收字节数, 存活字节数)。
    fn collect(&mut self) -> (i64, i64) {
        let before = self.live_bytes();
        let mut stack: Vec<usize> = self.roots.clone();
        while let Some(i) = stack.pop() {
            if let Some(Some(o)) = self.objects.get_mut(i) {
                if o.marked {
                    continue;
                }
                o.marked = true;
                let refs = o.refs.clone();
                stack.extend(refs);
            }
        }
        let mut freed = 0i64;
        for slot in self.objects.iter_mut() {
            match slot {
                Some(o) if o.marked => o.marked = false,
                Some(o) => {
                    freed += classes()[o.class].size as i64;
                    *slot = None;
                }
                None => {}
            }
        }
        // 压缩（清掉 None 洞，同时修正 roots 与 refs 的下标）
        let mut map = vec![usize::MAX; self.objects.len()];
        let mut packed: Vec<Option<SimObj>> = Vec::with_capacity(self.objects.len());
        for (i, slot) in self.objects.drain(..).enumerate() {
            if slot.is_some() {
                map[i] = packed.len();
                packed.push(slot);
            }
        }
        for o in packed.iter_mut().flatten() {
            o.refs = o.refs.iter().filter_map(|r| map.get(*r)).filter(|x| **x != usize::MAX).copied().collect();
        }
        self.roots = self.roots.iter().filter_map(|r| map.get(*r)).filter(|x| **x != usize::MAX).copied().collect();
        self.objects = packed;
        self.eden_used = self.roots.len() as i64 * 24;
        (freed, self.live_bytes())
    }

    /// 按类聚合 —— **逐对象扫**，不是估算。
    fn histogram(&self) -> Vec<ClassHisto> {
        let cls = classes();
        let mut agg: std::collections::HashMap<usize, (u64, u64)> = std::collections::HashMap::new();
        for o in self.objects.iter().flatten() {
            let e = agg.entry(o.class).or_insert((0, 0));
            e.0 += 1;
            e.1 += cls[o.class].size as u64;
        }
        let mut v: Vec<ClassHisto> = agg
            .into_iter()
            .map(|(c, (n, b))| ClassHisto {
                class_name: cls[c].name.to_string(),
                module: if cls[c].loader == 0 { Some("java.base".to_string()) } else { None },
                instances: n,
                bytes: b,
            })
            .collect();
        ClassHisto::sort_by_bytes(&mut v);
        v
    }

    fn model(&self) -> HeapModel {
        let cls = classes();
        let mut cd: Vec<ClassDef> = cls
            .iter()
            .map(|c| ClassDef {
                name: c.name.to_string(),
                super_name: c.super_name.map(|s| s.to_string()),
                instance_size: c.size,
                fields: c
                    .fields
                    .iter()
                    .map(|(n, t, s)| FieldDef { name: n.to_string(), ty: *t, size: *s })
                    .collect(),
            })
            .collect();
        cd.sort_by(|a, b| a.name.cmp(&b.name));
        let mut objects = Vec::with_capacity(self.count());
        for o in self.objects.iter().flatten() {
            let size = cls[o.class].size as usize;
            let mut data = o.data.clone();
            data.resize(size, 0);
            objects.push(HeapObject { class_name: cls[o.class].name.to_string(), data });
        }
        HeapModel { classes: cd, objects, jvm_version: vmless_attach::VM_VERSION.to_string() }
    }
}

// ---------------------------------------------------------------------------
// 线程模型：真 OS 线程 + 自己发布的 Java 栈
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ThreadPub {
    name: String,
    id: u64,
    daemon: bool,
    state: &'static str,
    frames: Vec<Frame>,
    locks: Vec<LockRef>,
    waiting_on: Option<LockRef>,
    started: Instant,
    cpu_ns: u64,
}

struct State {
    heap: Mutex<SimHeap>,
    threads: Mutex<Vec<ThreadPub>>,
    next_thread_id: AtomicU64,
    // 计数器
    young_gc: AtomicI64,
    young_time_ns: AtomicI64,
    full_gc: AtomicI64,
    full_time_ns: AtomicI64,
    live: AtomicI64,
    daemon: AtomicI64,
    started: AtomicI64,
    peak: AtomicI64,
    classes_loaded: AtomicI64,
    classes_unloaded: AtomicI64,
    class_bytes: AtomicI64,
    jit_compiled: AtomicI64,
    jit_time_ns: AtomicI64,
    safepoints: AtomicI64,
    safepoint_time_ns: AtomicI64,
    alloc_bytes: AtomicI64,
    alloc_calls: AtomicI64,
    native_peak: AtomicI64,
    start: Instant,
    // 可写标志（jinfo -flag 能真改）
    flag_heap_dump_on_oom: AtomicBool,
    flag_verbose_gc: AtomicBool,
    flag_print_gc: AtomicBool,
    tlab_fills: AtomicI64,
    tlab_waste: AtomicI64,
    stopped: AtomicBool,
}

struct SimVm {
    st: Arc<State>,
    identity: VmIdentity,
}

const YOUNG_LOCK: u64 = 0x1000;
const FULL_LOCK: u64 = 0x2008;

impl VmProbe for SimVm {
    fn perf(&self) -> PerfSnapshot {
        let st = &self.st;
        let heap = st.heap.lock().unwrap_or_else(|e| e.into_inner());
        let live = heap.live_bytes();
        let committed = heap.committed;
        let capacity = heap.capacity;
        let eden_cap = (capacity / 4).max(1 << 20);
        let surv_cap = (capacity / 16).max(1 << 18);
        let old_cap = capacity - eden_cap - 2 * surv_cap;
        let eden_used = heap.eden_used.min(eden_cap);
        let old_used = (live - eden_used).max(0);
        let metaspace_used = 4 << 20;
        let class_space_used = 1 << 20;
        let up_ms = st.start.elapsed().as_millis() as u64;
        let snap = PerfSnapshot {
            uptime_ms: up_ms,
            threads_started: st.started.load(Ordering::Relaxed),
            threads_live: st.live.load(Ordering::Relaxed),
            threads_daemon: st.daemon.load(Ordering::Relaxed),
            threads_peak: st.peak.load(Ordering::Relaxed),
            classes_loaded: st.classes_loaded.load(Ordering::Relaxed),
            classes_unloaded: st.classes_unloaded.load(Ordering::Relaxed),
            classes_shared_loaded: 0,
            class_bytes_shared: 0,
            class_bytes: st.class_bytes.load(Ordering::Relaxed),
            class_bytes_unloaded: 0,
            method_bytes: 512 * 1024,
            class_time_ns: 41_000_000,
            class_init_time_ns: 12_000_000,
            class_verify_time_ns: 9_000_000,
            class_link_time_ns: 6_000_000,
            classes_initialized: st.classes_loaded.load(Ordering::Relaxed),
            classes_linked: st.classes_loaded.load(Ordering::Relaxed),
            classes_verified: st.classes_loaded.load(Ordering::Relaxed),
            jit_compiled: st.jit_compiled.load(Ordering::Relaxed),
            jit_failed: 1,
            jit_invalid: 0,
            jit_time_ns: st.jit_time_ns.load(Ordering::Relaxed),
            jit_failed_type: 1,
            jit_failed_method: "com/example/Point mix(II)J".to_string(),
            safepoints: st.safepoints.load(Ordering::Relaxed),
            safepoint_time_ns: st.safepoint_time_ns.load(Ordering::Relaxed),
            safepoint_sync_time_ns: st.safepoint_time_ns.load(Ordering::Relaxed) / 3,
            application_time_ns: (up_ms as i64) * 1_000_000,
            generations: vec![
                // 顺序：space.0 = Eden，space.1 = Survivor 0，space.2 = Survivor 1
                GenStat {
                    spaces: vec![
                        SpaceStat::new("Eden Space", eden_cap, eden_used, capacity / 2, 0),
                        SpaceStat::new("Survivor Space", surv_cap, heap.s0_used, surv_cap, 0),
                        SpaceStat::new("Survivor Space", surv_cap, heap.s1_used, surv_cap, 0),
                    ],
                },
                GenStat { spaces: vec![SpaceStat::new("Old Space", old_cap, old_used, capacity, 0)] },
            ],
            collectors: vec![
                GcStat {
                    name: "VMless Young Collection".to_string(),
                    invocations: st.young_gc.load(Ordering::Relaxed),
                    time_ns: st.young_time_ns.load(Ordering::Relaxed),
                    ..Default::default()
                },
                GcStat {
                    name: "VMless Full Collection".to_string(),
                    invocations: st.full_gc.load(Ordering::Relaxed),
                    time_ns: st.full_time_ns.load(Ordering::Relaxed),
                    ..Default::default()
                },
                // 我们没有并发收集器，但这个计数器**必须存在**：jstat 的 GCT 列是
                // (collector.0.time + .1.time + .2.time)/hrt，少一个整列都算不出来。
                GcStat {
                    name: "VMless Concurrent Collection".to_string(),
                    invocations: 0,
                    time_ns: 0,
                    ..Default::default()
                },
            ],
            metaspace: SpaceStat::new("Metaspace", 8 << 20, metaspace_used, 256 << 20, 0),
            compressed_class: SpaceStat::new("Compressed Class Space", 2 << 20, class_space_used, 32 << 20, 0),
            tlab: Some(TlabStat {
                alloc_threads: st.live.load(Ordering::Relaxed),
                fills: st.tlab_fills.load(Ordering::Relaxed),
                max_fills: 128,
                alloc: st.alloc_bytes.load(Ordering::Relaxed),
                gc_waste: st.tlab_waste.load(Ordering::Relaxed),
                max_gc_waste: 65536,
                refill_waste: 16384,
                max_refill_waste: 8192,
                slow_alloc: st.alloc_calls.load(Ordering::Relaxed) / 64,
                max_slow_alloc: 32,
            }),
            extra: Vec::new(),
        };
        snap
    }

    fn heap_info(&self) -> Result<String, String> {
        let heap = self.st.heap.lock().unwrap_or_else(|e| e.into_inner());
        let live = heap.live_bytes();
        let mut s = String::new();
        s.push_str(&format!(
            "VMless mark-sweep heap   total reserved {}K, committed {}K, used {}K\n",
            heap.capacity / 1024,
            heap.committed / 1024,
            live / 1024
        ));
        s.push_str(&format!(
            "eden {}K, survivor {}K, old {}K, objects {}\n",
            heap.eden_used / 1024,
            (heap.s0_used + heap.s1_used) / 1024,
            (live - heap.eden_used).max(0) / 1024,
            heap.count()
        ));
        Ok(s)
    }

    fn heap_histogram(&self) -> Result<Vec<ClassHisto>, String> {
        Ok(self.st.heap.lock().unwrap_or_else(|e| e.into_inner()).histogram())
    }

    fn class_loaders(&self) -> Result<Vec<LoaderStat>, String> {
        let heap = self.st.heap.lock().unwrap_or_else(|e| e.into_inner());
        let cls = classes();
        let mut v = Vec::new();
        for (i, (id, parent, ty, n)) in heap.loaders.iter().enumerate() {
            let bytes: u64 = cls.iter().filter(|c| c.loader == i).map(|c| c.size as u64 * 8).sum();
            v.push(LoaderStat {
                class_loader: *id,
                parent: *parent,
                cld: *id ^ 0x2000,
                classes: *n,
                chunk_size: bytes.max(1024),
                block_size: bytes.saturating_sub(512).max(1024),
                loader_type: ty.to_string(),
                hidden: false,
            });
        }
        Ok(v)
    }

    fn class_stats(&self) -> Result<Vec<ClassStat>, String> {
        let heap = self.st.heap.lock().unwrap_or_else(|e| e.into_inner());
        let hist = heap.histogram();
        let cls = classes();
        let mut v = Vec::new();
        for c in cls.iter() {
            let (inst, bytes) = hist
                .iter()
                .find(|h| h.class_name == c.name)
                .map(|h| (h.instances, h.bytes))
                .unwrap_or((0, 0));
            v.push(ClassStat {
                name: c.name.to_string(),
                class_loader: if c.loader == 0 { "<boot>".into() } else { "app".into() },
                instances: inst,
                bytes,
                methods: c.methods,
                constant_pool_bytes: (c.methods * 13).min(4096),
            });
        }
        v.sort_by(|a, b| b.bytes.cmp(&a.bytes));
        Ok(v)
    }

    fn run_gc(&self) -> Result<String, String> {
        let t0 = Instant::now();
        let (freed, live) = {
            let mut heap = self.st.heap.lock().unwrap_or_else(|e| e.into_inner());
            heap.collect()
        };
        let ns = t0.elapsed().as_nanos() as i64;
        self.st.young_gc.fetch_add(1, Ordering::Relaxed);
        self.st.young_time_ns.fetch_add(ns, Ordering::Relaxed);
        self.st.safepoints.fetch_add(1, Ordering::Relaxed);
        self.st.safepoint_time_ns.fetch_add(ns, Ordering::Relaxed);
        Ok(format!("GC 完成：回收 {} 字节，存活 {} 字节，耗时 {:.3} ms", freed, live, ns as f64 / 1e6))
    }

    fn threads(&self) -> Result<ThreadDump, String> {
        let list = self.st.threads.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        for p in list.iter() {
            let mut t = JThread::new(p.name.clone(), p.id, p.state);
            t.daemon = p.daemon;
            t.frames = p.frames.clone();
            t.locks = p.locks.clone();
            t.waiting_on = p.waiting_on.clone();
            t.elapsed_ns = Some(p.started.elapsed().as_nanos() as u64);
            t.cpu_ns = Some(p.cpu_ns);
            t.tid = p.id;
            t.nid = p.id * 4;
            out.push(t);
        }
        Ok(ThreadDump { threads: out })
    }

    fn heap_model(&self) -> Result<HeapModel, String> {
        let heap = self.st.heap.lock().unwrap_or_else(|e| e.into_inner());
        Ok(heap.model())
    }

    fn native_memory(&self) -> Result<String, String> {
        let alloc = self.st.alloc_bytes.load(Ordering::Relaxed);
        let calls = self.st.alloc_calls.load(Ordering::Relaxed);
        let peak = self.st.native_peak.load(Ordering::Relaxed);
        Ok(format!(
            "Native Memory Tracking:\n\n\
             Total: reserved=10485760KB, committed=262144KB\n\n\
             -                 Java Heap (reserved=65536KB, committed=16384KB)\n\
             \x20                           (mmap: reserved=65536KB, committed=16384KB)\n\n\
             -                     Class (reserved=1048576KB, committed=8192KB)\n\
             \x20                           (classes #10)\n\n\
             -                    Thread (reserved=8192KB, committed=8192KB)\n\
             \x20                           (thread #{} )\n\n\
             -                      Code (reserved=245760KB, committed=10240KB)\n\n\
             -                        GC (reserved=1024KB, committed=1024KB)\n\n\
             -                  Compiler (reserved=1024KB, committed=1024KB)\n\n\
             -                  Internal (reserved=4096KB, committed=4096KB)\n\n\
             -                    Symbol (reserved=2048KB, committed=2048KB)\n\n\
             -    Native Memory Tracking (reserved=512KB, committed=512KB)\n\n\
             allocator: 分配 {} 字节 / {} 次调用，峰值 {} 字节\n",
            self.st.live.load(Ordering::Relaxed),
            alloc,
            calls,
            peak
        ))
    }

    fn flags(&self) -> Vec<(String, String)> {
        let b = |v: bool| if v { "true".to_string() } else { "false".to_string() };
        vec![
            ("MaxHeapSize".into(), (self.st.heap.lock().map(|h| h.capacity).unwrap_or(0)).to_string()),
            ("InitialHeapSize".into(), (self.st.heap.lock().map(|h| h.committed).unwrap_or(0)).to_string()),
            ("UseVMlessGC".into(), "true".into()),
            ("PrintGC".into(), b(self.st.flag_print_gc.load(Ordering::Relaxed))),
            ("VerboseGC".into(), b(self.st.flag_verbose_gc.load(Ordering::Relaxed))),
            ("HeapDumpOnOutOfMemoryError".into(), b(self.st.flag_heap_dump_on_oom.load(Ordering::Relaxed))),
        ]
    }

    fn get_flag(&self, name: &str) -> Option<String> {
        self.flags().into_iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    fn set_flag(&self, name: &str, value: &str) -> Result<(), String> {
        // 先判断"这个标志能不能改"，再判断值合不合法 —— 否则 `MaxHeapSize=1g`
        // 会被误报成"布尔标志取值不合法"。
        match name {
            "MaxHeapSize" | "InitialHeapSize" | "UseVMlessGC" => {
                return Err(format!(
                    "{} 是启动期标志，运行期不可写（jinfo 只能改 manageable 标志）",
                    name
                ))
            }
            "PrintGC" | "VerboseGC" | "HeapDumpOnOutOfMemoryError" => {}
            other => return Err(format!("没有这个标志: {}", other)),
        }
        let on = match value {
            "true" | "+" | "1" => true,
            "false" | "-" | "0" => false,
            _ => return Err(format!("布尔标志只能取 true/false，收到 {:?}", value)),
        };
        match name {
            "PrintGC" => {
                self.st.flag_print_gc.store(on, Ordering::Relaxed);
                Ok(())
            }
            "VerboseGC" => {
                self.st.flag_verbose_gc.store(on, Ordering::Relaxed);
                Ok(())
            }
            "HeapDumpOnOutOfMemoryError" => {
                self.st.flag_heap_dump_on_oom.store(on, Ordering::Relaxed);
                Ok(())
            }
            _ => unreachable!(),
        }
    }
}

// ---------------------------------------------------------------------------
// 线程具体行为
// ---------------------------------------------------------------------------

fn add_thread(st: &Arc<State>, p: ThreadPub) {
    let mut list = st.threads.lock().unwrap_or_else(|e| e.into_inner());
    list.push(p);
    let n = list.len() as i64;
    drop(list);
    st.live.fetch_add(1, Ordering::Relaxed);
    st.started.fetch_add(1, Ordering::Relaxed);
    let peak = st.peak.load(Ordering::Relaxed);
    if n > peak {
        st.peak.store(n, Ordering::Relaxed);
    }
}

fn update_thread(st: &Arc<State>, id: u64, f: impl FnOnce(&mut ThreadPub)) {
    let mut list = st.threads.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(t) = list.iter_mut().find(|t| t.id == id) {
        f(t);
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut main_class = "com.example.Main".to_string();
    let mut main_args: Vec<String> = Vec::new();
    let mut n_threads = 3usize;
    let mut n_objects = 1500usize;
    let mut deadlock = false;
    let mut gc_interval_ms = 1200u64;
    let mut quiet = false;
    let mut i = 0usize;
    while i < args.len() {
        let a = args[i].clone();
        let mut take = |i: &mut usize| -> Option<String> {
            let v = args.get(*i + 1).cloned();
            if v.is_some() {
                *i += 2;
            } else {
                *i += 1;
            }
            v
        };
        match a.as_str() {
            "--main" => {
                if let Some(v) = take(&mut i) {
                    main_class = v;
                }
            }
            "--arg" => {
                if let Some(v) = take(&mut i) {
                    main_args.push(v);
                }
            }
            "--threads" => {
                n_threads = take(&mut i).and_then(|v| v.parse().ok()).unwrap_or(3).min(64);
            }
            "--objects" => {
                n_objects = take(&mut i).and_then(|v| v.parse().ok()).unwrap_or(1500);
            }
            "--gc-interval" => {
                gc_interval_ms = take(&mut i).and_then(|v| v.parse().ok()).unwrap_or(1200);
            }
            "--deadlock" => {
                deadlock = true;
                i += 1;
            }
            "--quiet" => {
                quiet = true;
                i += 1;
            }
            "-h" | "--help" => {
                println!("用法: vmsim [--main CLASS] [--arg A] [--threads N] [--objects N] [--deadlock] [--gc-interval MS] [--quiet]");
                return;
            }
            other => {
                // 未识别参数当成 main 参数（模仿 java 的透传）
                main_args.push(other.to_string());
                i += 1;
            }
        }
    }

    let st = Arc::new(State {
        heap: Mutex::new(SimHeap::new()),
        threads: Mutex::new(Vec::new()),
        next_thread_id: AtomicU64::new(1),
        young_gc: AtomicI64::new(0),
        young_time_ns: AtomicI64::new(0),
        full_gc: AtomicI64::new(0),
        full_time_ns: AtomicI64::new(0),
        live: AtomicI64::new(0),
        daemon: AtomicI64::new(0),
        started: AtomicI64::new(0),
        peak: AtomicI64::new(0),
        classes_loaded: AtomicI64::new(classes().len() as i64),
        classes_unloaded: AtomicI64::new(0),
        class_bytes: AtomicI64::new(classes().iter().map(|c| c.size as i64 * 8).sum()),
        jit_compiled: AtomicI64::new(412),
        jit_time_ns: AtomicI64::new(180_000_000),
        safepoints: AtomicI64::new(0),
        safepoint_time_ns: AtomicI64::new(0),
        alloc_bytes: AtomicI64::new(0),
        alloc_calls: AtomicI64::new(0),
        native_peak: AtomicI64::new(0),
        start: Instant::now(),
        flag_heap_dump_on_oom: AtomicBool::new(false),
        flag_verbose_gc: AtomicBool::new(false),
        flag_print_gc: AtomicBool::new(false),
        tlab_fills: AtomicI64::new(0),
        tlab_waste: AtomicI64::new(0),
        stopped: AtomicBool::new(false),
    });

    // 预置一堆对象（让 jmap -histo 有内容）
    {
        let mut heap = st.heap.lock().unwrap_or_else(|e| e.into_inner());
        let mut prev: Option<usize> = None;
        let mut roots = Vec::new();
        for k in 0..n_objects {
            let cls = match k % 4 {
                0 => 1, // Point
                1 => 2, // Node
                2 => 7, // CacheEntry
                _ => 4, // [B
            };
            let refs: Vec<usize> = match prev {
                Some(p) if k % 3 == 0 => vec![p],
                _ => vec![],
            };
            let idx = heap.alloc(cls, refs);
            if k % 7 == 0 {
                roots.push(idx);
            }
            prev = Some(idx);
        }
        heap.roots = roots;
        heap.eden_used = heap.live_bytes() / 2;
        heap.s0_used = heap.live_bytes() / 8;
    }

    // 主线程
    let main_id = st.next_thread_id.fetch_add(1, Ordering::Relaxed);
    add_thread(
        &st,
        ThreadPub {
            name: "main".into(),
            id: main_id,
            daemon: false,
            state: "RUNNABLE",
            frames: vec![
                Frame::native_m("java.lang.Thread", "sleep"),
                Frame::java(&main_class, "main", "Main.java", 12),
            ],
            locks: vec![],
            waiting_on: None,
            started: Instant::now(),
            cpu_ns: 12_000_000,
        },
    );

    // jps 用的 javaCommand
    let java_command = if main_args.is_empty() {
        main_class.clone()
    } else {
        format!("{} {}", main_class, main_args.join(" "))
    };
    let mut id = VmIdentity::new(main_class.clone());
    id.java_command = java_command.clone();
    id.jvm_args = vec!["-Xmx64m".into(), "-Xms16m".into(), "-Dvmless.attach.demo=1".into()];
    id.vm_flags = vec!["-XX:+UseVMlessGC".into(), "-XX:MaxHeapSize=67108864".into()];
    id.class_path = "demo/classes".into();
    id.java_home = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.display().to_string()))
        .unwrap_or_else(|| ".".into());
    id.system_properties = vec![
        ("file.encoding".into(), "UTF-8".into()),
        ("file.separator".into(), "\\".into()),
        ("java.vm.name".into(), vmless_attach::VM_NAME.into()),
        ("java.vm.version".into(), vmless_attach::VM_VERSION.into()),
        ("java.class.version".into(), "69.0".into()),
        ("os.name".into(), "Windows 11".into()),
        ("os.arch".into(), "amd64".into()),
        ("user.name".into(), std::env::var("USERNAME").unwrap_or_else(|_| "user".into())),
        ("user.dir".into(), std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default()),
        ("java.io.tmpdir".into(), std::env::temp_dir().display().to_string()),
        ("line.separator".into(), "\n".into()),
        ("sun.java.command".into(), java_command.clone()),
        ("vmless.attach.demo".into(), "1".into()),
    ];
    id.gc_policy_name = "VMless Mark-Sweep".into();

    let vm = Arc::new(SimVm { st: st.clone(), identity: id.clone() });
    let handle = match vmless_attach::start(id, vm.clone(), AttachOptions::default()) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("attach 启动失败: {}", e);
            std::process::exit(1);
        }
    };

    if !quiet {
        println!("vmsim pid={} main={}", handle.pid, main_class);
        println!("perfdata = {}", vmless_attach::registry::perf_path(handle.pid).display());
        println!("endpoint = 127.0.0.1:{} token={}", handle.port(), handle.token());
        println!("按 Ctrl-C 退出");
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }

    // 工作线程：真的分配对象、真的走路
    for k in 0..n_threads {
        let st = st.clone();
        let name = format!("worker-{}", k);
        let id = st.next_thread_id.fetch_add(1, Ordering::Relaxed);
        add_thread(
            &st,
            ThreadPub {
                name: name.clone(),
                id,
                daemon: k > 0,
                state: "RUNNABLE",
                frames: vec![
                    Frame::native_m("java.lang.Thread", "run"),
                    Frame::java("com.example.Worker", "run", "Worker.java", 30 + k as i32),
                ],
                locks: vec![LockRef::new(YOUNG_LOCK, "java.lang.Object")],
                waiting_on: None,
                started: Instant::now(),
                cpu_ns: 1_000_000,
            },
        );
        if k > 0 {
            st.daemon.fetch_add(1, Ordering::Relaxed);
        }
        std::thread::spawn(move || {
            let mut tick = 0u64;
            while !st.stopped.load(Ordering::Relaxed) {
                tick += 1;
                // 真的分配
                let n = 1 + (tick % 3) as usize;
                let mut allocs = 0i64;
                {
                    let mut heap = st.heap.lock().unwrap_or_else(|e| e.into_inner());
                    for j in 0..n {
                        let root = heap.roots.get(j % heap.roots.len().max(1)).copied();
                        heap.alloc(if j % 2 == 0 { 1 } else { 7 }, root.into_iter().collect());
                        allocs += 24;
                    }
                }
                st.alloc_bytes.fetch_add(allocs, Ordering::Relaxed);
                st.alloc_calls.fetch_add(n as i64, Ordering::Relaxed);
                st.tlab_fills.fetch_add(1, Ordering::Relaxed);
                st.tlab_waste.fetch_add(16, Ordering::Relaxed);
                st.jit_compiled.fetch_add(1, Ordering::Relaxed);
                st.jit_time_ns.fetch_add(240_000, Ordering::Relaxed);
                let peak = st.native_peak.load(Ordering::Relaxed);
                let cur = st.alloc_bytes.load(Ordering::Relaxed);
                if cur > peak {
                    st.native_peak.store(cur, Ordering::Relaxed);
                }
                update_thread(&st, id, |t| {
                    t.cpu_ns += 400_000;
                    t.frames = vec![
                        Frame::native_m("java.lang.Thread", "sleep"),
                        Frame::java("com.example.Worker", "run", "Worker.java", 30 + k as i32),
                        Frame::java("com.example.Worker", "allocate", "Worker.java", 55),
                    ];
                    t.state = "TIMED_WAITING";
                });
                std::thread::sleep(Duration::from_millis(25));
            }
        });
    }

    // GC 线程
    {
        let st = st.clone();
        std::thread::spawn(move || {
            let id = st.next_thread_id.fetch_add(1, Ordering::Relaxed);
            add_thread(
                &st,
                ThreadPub {
                    name: "VMless-GC".into(),
                    id,
                    daemon: true,
                    state: "WAITING",
                    frames: vec![Frame::java("com.example.VMGc", "run", "VMGc.java", 20)],
                    locks: vec![],
                    waiting_on: Some(LockRef::new(FULL_LOCK, "java.lang.Object")),
                    started: Instant::now(),
                    cpu_ns: 2_000_000,
                },
            );
            st.daemon.fetch_add(1, Ordering::Relaxed);
            while !st.stopped.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(gc_interval_ms));
                if st.stopped.load(Ordering::Relaxed) {
                    break;
                }
                let t0 = Instant::now();
                let (freed, _) = {
                    let mut heap = st.heap.lock().unwrap_or_else(|e| e.into_inner());
                    heap.collect()
                };
                if freed > 0 {
                    st.young_gc.fetch_add(1, Ordering::Relaxed);
                    st.young_time_ns.fetch_add(t0.elapsed().as_nanos() as i64, Ordering::Relaxed);
                }
                st.safepoints.fetch_add(1, Ordering::Relaxed);
                st.safepoint_time_ns.fetch_add(t0.elapsed().as_nanos() as i64, Ordering::Relaxed);
            }
        });
    }

    // 死锁演示：两个线程交叉持锁（**建模**的锁，不动真 Rust 锁 —— 真锁上死锁就收不了场了）
    if deadlock {
        let l1 = LockRef::new(YOUNG_LOCK, "java.lang.Object");
        let l2 = LockRef::new(FULL_LOCK, "java.lang.Object");
        for (name, holds, waits) in [("deadlock-A", l1.clone(), l2.clone()), ("deadlock-B", l2, l1)] {
            let id = st.next_thread_id.fetch_add(1, Ordering::Relaxed);
            add_thread(
                &st,
                ThreadPub {
                    name: name.into(),
                    id,
                    daemon: false,
                    state: "BLOCKED",
                    frames: vec![
                        Frame::java("com.example.Deadlock", "run", "Deadlock.java", 18),
                        Frame::java("com.example.Deadlock", "main", "Deadlock.java", 9),
                    ],
                    locks: vec![holds],
                    waiting_on: Some(waits),
                    started: Instant::now(),
                    cpu_ns: 500_000,
                },
            );
        }
    }

    // 主线程常驻
    while !st.stopped.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
        update_thread(&st, main_id, |t| {
            t.frames = vec![
                Frame::native_m("java.lang.Thread", "sleep"),
                Frame::java(&main_class, "main", "Main.java", 12),
            ];
            t.state = "TIMED_WAITING";
        });
    }
    drop(handle);
}
