# REQ-001 · JFR（`.jfr` 读 / 写 / 录制）+ `jhsdb` 等价能力

- 状态：**已完成**
- 日期：2026-11-17
- 版本：0.1.0

## 要做什么（用户原话）

> 实现 **JFR（Java Flight Recorder）** 的事件框架 + `jfr` 工具 + `jhsdb`。
> 用户要求**生产级**。
>
> 第一段：`.jfr` 文件的**读**（先把格式搞对）。关键判据：**真 `jfr` 能读我们产出的 `.jfr` 文件**。
> `jfr print --events <类型>`、`jfr summary`、`jfr metadata` 三个子命令的输出要与真 `jfr`
> 的同名命令对得上（字段名、事件名、顺序）。
>
> 第二段：`.jfr` 的**写**（我们自己的录制器）。判据：我们写出的 `.jfr` 能被真 JDK 的
> `jfr summary` / `jfr print` 打开且至少 `jdk.ExecutionSample` 事件可读。
>
> 第三段：`jhsdb` —— 真身依赖 Serviceability Agent（ptrace/调试器），我们做**等价能力**：
> `jmap --histo` / `jstack` / `jinfo` / `--heap`。Windows 上没有 ptrace，
> **不要试图 attach 别的 JVM**；只服务我们自己的 VM，help 里写明。
>
> 生产级硬指标：不 panic；fuzz ≥5 万例 0 panic；能解析 ≥50MB 的 `.jfr` 并给出耗时与内存峰值；
> `Cargo.toml` 只保留 `vmless-attach`。

## 不做什么

- **不做**对任意 HotSpot 进程的 `jhsdb`（需要 ptrace；Windows 上不存在）。
  只做「目标跑着 vmless-attach」的等价能力，help 里明确写出这个边界。
- **不做** `jfr print --xml`（`--json` 有；XML 没有额外信息量）。
- **不做** GC 的完整事件集：只做任务点名的 5 个内置事件 + `ThreadStart/End`。
  其余事件类型（GC 细节、编译、I/O）**读**能读（格式是通用的），**写**不支持。
- **不改** `D:\vmless\jdk-attach`（只读依赖）、`rustjavac/`、`repo/`、其它 `jdk-*`。
- **不加**任何外部 crate（除 `vmless-attach`）：变长整数、UTF-16、时区偏移、
  峰值内存统计都自己写。

## 验收标准（可执行）

| # | 怎么验 | 结果 |
| --- | --- | --- |
| 1 | 真 `jfr summary` 与我们 `summary` 对同一真文件 `cmp` 无输出 | ✅ 275KB 文件、54MB 文件都对上 |
| 2 | `jfr print --events jdk.ExecutionSample` 前 5 条与原版逐字节一致 | ✅ 778 行全一致 |
| 3 | 真 `jfr summary` / `print` 打开我们写的文件且 `ExecutionSample` 可读 | ✅ 见验收记录 |
| 4 | `jfrfuzz --cases 50000` 0 panic | ✅ 10.3s |
| 5 | 解析 ≥50MB 文件，给出耗时 / 峰值内存 | ✅ 0.22s / 59.6MB |
| 6 | `jhsdb jmap --histo --pid <pid>` 对着 `vmsim.exe` 有真实输出 | ✅ 见验收记录 |

## 已知约束（调研结论）

- 格式的**唯一真相源**是 JDK 的 `src.zip`（`jdk.jfr.internal.consumer.*` 与
  `MetadataWriter`），不是文档、不是别的实现。
- Windows 上没有 ptrace，SA 不可用；`jhsdb` 只能走「VM 自己上报」。
- 真 `jfr` 的输出版式有几处**自相矛盾的分裂**（行分隔符、同时间戳事件顺序），
  想逐字节对上必须照抄，不能「讲道理地统一」。

## 方案

1. `format/` 手写二进制层（varint、chunk 头、元数据树、常量池链、事件块），
   每个模块的文件头注释记录**权威实现出处**与踩坑点。
2. `reader` 按「先读常量池、再顺序读事件」的顺序实现（与 `ChunkParser` 同构）。
3. `writer` 采用「头 → 元数据块 → 事件块 → 常量池块 → 回填头」的顺序：
   常量池放最后，因为写事件时才知道要收哪些常量。
4. `bin/jfr` 的格式化代码逐个函数对照 `PrettyWriter` / `MetadataWriter` /
   `Summary` 移植，包括 `PriorityQueue` 的 sift 规则。
5. `jhsdb` 做成 `vmless-attach` 的薄壳（数据来自目标 VM 上报）。

## 验收记录（实际跑过的命令与输出）

**1) summary 逐字节一致（275KB / 1 chunk）**

```
$ cmp ref_summary.txt our_summary3.txt   → 无输出（204 行全同）
```

**2) print 逐字节一致（40540 行）**

```
$ cmp ref_print_all.txt our_p6.txt       → 无输出
$ cmp ref_print_es.txt  our_print_es.txt → 无输出（778 行）
$ cmp ref_metadata.txt  our_metadata.txt → 无输出（6408 行）
```

**3) 54MB / 5 chunk 的 summary 逐字节一致**

```
$ cmp big_ref_summary.txt bigsum2.txt    → 无输出
$ jfr.exe summary D:/vmless/big.jfr --stats
stats: elapsed=0.221s peak_rss=59.6 MB
 jdk.ObjectAllocationInNewTLAB        3022088      50930790   （共 3.02M 事件）
```

**4) 真 jfr 读我们写的文件**

```
$ jfr.exe summary D:/vmless/ours.jfr
 Version: 2.1 / Chunks: 1
 jdk.ExecutionSample 26 / jdk.GCPhasePause 2 / jdk.JavaMonitorEnter 1 / jdk.Checkpoint 1 / jdk.Metadata 1

$ jfr.exe print --events jdk.ExecutionSample D:/vmless/ours.jfr
jdk.ExecutionSample {
  startTime = 13:51:01.138 (2026-11-17)
  sampledThread = "worker-0" (javaThreadId = 42)
  state = "STATE_RUNNABLE"
  stackTrace = [ demo.Busy.work(long, String) line: 42 / java.lang.Thread.run() line: 1474 ]
}
```

**5) fuzz**

```
$ jfrfuzz.exe --cases 50000 --file D:/vmless/seed.jfr
fuzz 完成：50000 例，0 panic，耗时 10.34s，seed=12345（含结构感知变异）
```

**6) jhsdb 对着 vmsim（pid 49212）**

```
$ jhsdb.exe jmap --histo --pid 49212
 num     #instances         #bytes  class name (module)
   1:         1018        40720  com.example.CacheEntry
   2:          952        30464  com.example.Node
$ jhsdb.exe --heap --pid 49212
VMless mark-sweep heap   total reserved 65536K, committed 16384K, used 111K
Summary: 4 classes, 4050 live instances, 113704 live bytes
$ jhsdb.exe jstack --pid 49212    → "main" #1 … at com.example.Bench.main(Main.java:12)
$ jhsdb.exe jinfo --pid 49212     → java.vm.name=VMless Server VM 等
```

## 略过未修

- P2 · `src/bin/jfr.rs` · 全量 `print` 3.3M 事件时内存 2.6GB（未做逐 chunk flush）· 略过
  （带 `--events` 过滤的常用路径是 180MB；逐 chunk 改造会动已验证过的出队路径，
  风险大于收益，留作后续）
- P2 · `src/bin/jfr.rs` · `--xml` 未实现 · 略过（需求未要求）
