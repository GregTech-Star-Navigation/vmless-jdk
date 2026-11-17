# VM 侧要做什么（给 rustjavac 的接入说明）

> 状态：2026-11-17 · 对应 `vmless-attach 0.1.0`

接 attach **不需要改 VM 的任何内部结构**，只需要"把已有的数拿出来"。分三档，按需选：

## 档 0：一行接入（先跑通）

```rust
let handle = vmless_attach::vmless_attach_start("com.example.Main", 0)?; // pid=0 → 用自己的 pid
handle.perf_counter_set(vmless_attach::PerfId::ThreadsLive, 12);
handle.perf_counter_set(vmless_attach::PerfId::YoungGcCount, 7);
```

这一档已经能让 `jps` 列出进程、`jstat -gc` 出数（缺的列显示 `-`，与真 jstat 行为一致）、
`jcmd PerfCounter.print` 有输出。

`PerfId` 现有 16 个：`ThreadsLive/Daemon/Started/Peak`、`ClassesLoaded/Unloaded/ClassBytes`、
`Safepoints/SafepointTimeNs/ApplicationTimeNs`、`JitCompiled/JitTimeNs`、
`YoungGcCount/YoungGcTimeNs`、`FullGcCount/FullGcTimeNs`。

> `perf_counter_set` 是**瞬时值**（写进去就覆盖）；累加请用 `handle.perf_counter_add(id, delta)`。

## 档 1：整份计数器快照（推荐起点）

实现 `VmProbe::perf()` 返回 `PerfSnapshot`。服务端每 `refresh_interval_ms`（默认 100ms）
调一次，原地覆写 perfdata 的值区 —— **不会重建文件**（客户端是 mmap 的）。

```rust
impl VmProbe for MyVm {
    fn perf(&self) -> PerfSnapshot {
        PerfSnapshot {
            uptime_ms: self.start.elapsed().as_millis() as u64,
            threads_live: self.threads.len() as i64,
            classes_loaded: self.classes.loaded as i64,
            class_bytes: self.classes.bytes as i64,
            // 空间顺序：space.0 = Eden，space.1 = Survivor0，space.2 = Survivor1（别搞反！）
            generations: vec![
                GenStat { spaces: vec![
                    SpaceStat::new("Eden Space", eden_cap, eden_used, eden_max, 0),
                    SpaceStat::new("Survivor Space", s0_cap, s0_used, s0_cap, 0),
                    SpaceStat::new("Survivor Space", s1_cap, s1_used, s1_cap, 0),
                ]},
                GenStat { spaces: vec![SpaceStat::new("Old Space", old_cap, old_used, old_max, 0)] },
            ],
            // 三个收集器都要在：jstat 的 GCT 是 (c0+c1+c2)/hrt，少一个整列算不出来
            collectors: vec![young_gc, full_gc, GcStat::default() /* concurrent，没有就给 0 */],
            metaspace: ..., compressed_class: ...,
            ..Default::default()
        }
    }
}
```

**时间一律写纳秒**，容量一律写**字节**。`sun.os.hrt.frequency` 由本库固定写 `1e9`，
所以 `jstat` 除一下就是秒。

## 档 2：完整能力（工具全部可用）

| 方法 | 支撑的命令 | 要求 |
| --- | --- | --- |
| `perf()` | `jps` `jstat` `PerfCounter.print` | 最小必需 |
| `heap_info()` | `jcmd GC.heap_info` | 人读文本 |
| `heap_histogram()` | `jmap -histo`、`jcmd GC.class_histogram` | **真扫堆**：逐对象聚合；别估算 |
| `class_loaders()` | `jmap -clstats` | 每个类加载器一行 |
| `class_stats()` | `jcmd GC.class_stats` | 每类实例数/字节 |
| `run_gc()` | `jcmd GC.run` | 真的 stop-the-world 回收，返回一行结果 |
| `threads()` | `jstack`、`jcmd Thread.print` | 每个线程：名字/状态/栈帧/持有与等待的锁 |
| `heap_model()` | `jmap -dump`、`GC.heap_dump` | 类的字段布局 + 每个对象的字段数据 |
| `native_memory()` | `jcmd VM.native_memory summary` | 自己的分配器统计 |
| `flags()` / `get_flag()` / `set_flag()` | `jinfo -flags/-flag`、`VM.flags` | 改不了的返回 `Err`，别假装成功 |

**没实现的方法有默认实现，返回 `Err("VM 未实现 …")`**，工具侧会打印"不支持"而不是崩掉或给假数据。
所以可以一块一块接。

### 线程栈的模型（`threads()`）

```rust
JThread {
    name, java_id, daemon, priority, state: "RUNNABLE"|"BLOCKED"|"WAITING"|"TIMED_WAITING",
    frames: vec![Frame::java("com.example.Main", "main", "Main.java", 12),
                 Frame::native_m("java.lang.Thread", "sleep")],
    locks: vec![LockRef::new(0x1234, "java.lang.Object")],   // 持有的
    waiting_on: Some(LockRef::new(0x5678, "java.lang.Object")), // 等着的
    tid, nid, cpu_ns, elapsed_ns,
}
```

**死锁检测不用你实现**：本库在 `waiting_on` + `locks` 组成的图上找环，环上所有线程
一起报出来（`jstack -l` 的 `Found one Java-level deadlock:`）。VM 只要如实上报
"谁持有哪把锁、在等哪把锁"。

### 堆转储的模型（`heap_model()`）

```rust
HeapModel {
    classes: vec![ClassDef { name, super_name, instance_size, fields: vec![FieldDef{name, ty:'I', size:4}] }],
    objects: vec![HeapObject { class_name, data: Vec<u8> /* 长度 = instance_size */ }],
    jvm_version,
}
```

`data` 就是按字段顺序拼起来的字节（HPROF 的 INSTANCE_DUMP 原样写进去）。
本库负责生成合法的 HPROF 1.0.2（含常量池、类快照、根、结束标记）。

## 安全模型（已经做好，VM 侧不用管）

- 监听**只 bind 127.0.0.1**（`TcpListener::bind((Ipv4Addr::LOCALHOST, 0))`），端口随机。
- 握手要 **128 位令牌**；令牌和端口写在 perfdata 里的两个计数器
  （`vmless.attach.port` / `vmless.attach.token`），而 perfdata 在
  `%TEMP%\hsperfdata_<用户>\` 下 —— 那个目录的 ACL 就是"只允许同用户"这一层。
- 所有连接检查 `peer.ip().is_loopback()`；并发上限 64；命令执行串行化（一把互斥锁）。

## 一次典型接入的 diff 长什么样

```rust
// src/vm/mod.rs 里加：
pub struct VmAttach { pub handle: Option<vmless_attach::AttachHandle> }

impl VmAttach {
    pub fn start(&mut self, main_class: &str, java_command: &str) -> std::io::Result<()> {
        let mut id = vmless_attach::VmIdentity::new(main_class);
        id.java_command = java_command.to_string();
        id.system_properties = self.sysprops();
        self.handle = Some(vmless_attach::start(id, Arc::new(Probe { vm: self.weak() }),
            vmless_attach::AttachOptions::default())?);
        Ok(())
    }
}
```

`Cargo.toml` 加一行：`vmless-attach = { path = "../jdk-attach" }`。
（注意 `../jdk-attach` 的 package 名是 `vmless-attach`，crate 名是 `vmless_attach`。）
