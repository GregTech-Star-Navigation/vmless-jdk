# vmless-jfr

JFR（Java Flight Recorder）的 `.jfr` **读 / 写 / 录制**，加上 `jhsdb` 的等价能力。

一句话：**真 `jfr` 能读我们写的文件，我们也能读真 `jfr` 写的文件** —— 而且是**逐字节**：
在 JDK 25 录出来的 275KB 文件上，`summary` / `print` / `metadata` 的输出与真 `jfr`
`cmp` 完全一致（详见下面的对照表）。

> 状态：0.1.0（2026-11-17）。除 `vmless-attach`（只给 `jhsdb` 用）外**零依赖**。

## 怎么跑

```bash
export PATH="$PATH:/c/Users/Administrator/.cargo/bin"
cd /d/vmless/jdk-jfr
export CARGO_TARGET_DIR='D:\vmless\rt_jfr1'
cargo build --release

# 三个读命令
./…/release/jfr.exe summary  recording.jfr
./…/release/jfr.exe print    --events jdk.ExecutionSample recording.jfr
./…/release/jfr.exe metadata recording.jfr

# 录制（不给 --pid 时生成演示事件；给 --pid 就真采一个 VMless 的 VM）
./…/release/jfr.exe record out.jfr
./…/release/jfr.exe record out.jfr --pid 49212 --events 200

# jhsdb 等价物
./…/release/jhsdb.exe jmap --histo --pid 49212
./…/release/jhsdb.exe --heap --pid 49212
./…/release/jhsdb.exe jstack --pid 49212
./…/release/jhsdb.exe jinfo --pid 49212

# 解析入口的 fuzz（默认 5 万例）
./…/release/jfrfuzz.exe --cases 50000 --file seed.jfr
```

## 和真 `jfr` 的对照（实测）

| 命令 | 输入 | 结果 |
| --- | --- | --- |
| `jfr summary` | 真 JDK 25 录的 275KB / 1 chunk | **逐字节相同**（`cmp` 通过） |
| `jfr print` | 同上，40540 行 | **逐字节相同** |
| `jfr print --events jdk.ExecutionSample` | 同上，778 行 | **逐字节相同** |
| `jfr metadata` | 同上，6408 行 | **逐字节相同** |
| `jfr summary` | 真 JDK 25 录的 **54MB / 5 chunk**（3.02M 事件） | **逐字节相同** |

真 `jfr` 读**我们写的**文件：

```
$ jfr summary ours.jfr
 Version: 2.1
 Chunks: 1
 ...
 jdk.ExecutionSample         26           988
$ jfr print --events jdk.ExecutionSample ours.jfr
jdk.ExecutionSample {
  startTime = 13:51:01.138 (2026-11-17)
  sampledThread = "worker-0" (javaThreadId = 42)
  state = "STATE_RUNNABLE"
  stackTrace = [
    demo.Busy.work(long, String) line: 42
    java.lang.Thread.run() line: 1474
  ]
}
```

## 目录

```
src/
├── lib.rs
├── format/{mod,chunk,metadata,cp,event}.rs   二进制格式（varint / chunk 头 / 元数据树 / 常量池 / 事件块）
├── model.rs                                  类型定义与「值」
├── reader.rs                                 解析：chunk → 元数据 / 常量池 / 事件流
├── writer.rs                                 Metadata + 事件 → 合法 chunk 字节
├── recorder.rs                               Recorder::start → 环形缓冲 → dump
└── bin/
    ├── jfr.rs                                summary / print / metadata / record / inspect
    ├── jfr/record.rs                         record 的实现（演示数据 或 attach 采样）
    ├── jfrfuzz.rs                            fuzz 驱动
    └── jhsdb.rs                              jmap/jstack/jinfo 等价物
```

## 格式要点（读代码前先看这几条）

这些是**踩过才知道**的，全部对照 `src.zip` 里的 `jdk.jfr.internal.*` 核过：

1. **变长整数**是 7 位一组、小端组序，最多 9 字节（第 9 字节是裸 8 位）。
2. **`byte` 是裸 1 字节**，不是变长整数；`short`/`char`/`int`/`long` 才是变长。
   `float`/`double` 是**裸大端** 4/8 字节。
3. **常量池的链是倒着走的**：头里的 `constantPoolPosition` 指向**最后**一块，
   每块的 `delta` 是「到上一块」的负数偏移，`delta == 0` 收尾。
4. **`int` 要按 32 位截断**：JVM 写的是零扩展值，不截断会把 `-1` 读成 `4294967295`。
5. **`key 0` 不等于 null**：池里可能真有 key 0（`jdk.types.FlagValueOrigin` 的
   `"Default"` 就是），只有「查不到且 key==0」才当 null。
6. **`simpleType` 的类型没有外层结构**：`jdk.types.ThreadState` 只有 `String name`，
   于是 `state` 字段在文件里就是**一个字符串**。
7. **`ticksPerSecond` 用浮点除法**：`divisor = tps / 1e9`，Windows 上 QPC 频率
   2.3GHz → 2.3。按整数除法算会让时间戳随时长线性漂移（2 秒差 6ms）。
8. **`@Timespan`/`@Timestamp` 的单位不都是 TICKS**：`MILLISECONDS` / `NANOSECONDS` /
   `MILLISECONDS_SINCE_EPOCH` 都有；`Long.MAX_VALUE` 打印成 `Forever`。
9. **行分隔符在真 jfr 里是分裂的**：`summary` 表头用平台分隔符、表格行用字面 `\n`；
   `print`/`metadata` 全用平台分隔符；而**字段值里的换行**（如 `osVersion`）原样输出。
10. **`@ContentType` 决定格式化**：不给注解类型标 `@ContentType`，真 `jfr` 会把
    时间戳打成 ISO-8601 而不是 `HH:mm:ss.SSS (yyyy-MM-dd)`。
11. **`StackTrace.frames` 不能进常量池**（`cp=false`）：进了池，元素会被包成
    `RecordedFrame`，而 `RecordedObject.structifyArray` 期望裸 `Object[]` →
    真 jfr 直接 `ClassCastException`。
12. **写出去的 `fileState` 必须是 0**：非 0 时真 jfr 认为还在录，会一直等。
13. **`chunkSize` 必须正好等于文件长度**：真 jfr 靠它判断「最后一个 chunk」。

## 性能（本机实测）

| 场景 | 耗时 | 峰值内存 |
| --- | --- | --- |
| `summary` 54MB / 5 chunk / 3.02M 事件 | **0.22 s** | **59.6 MB** |
| `print --events jdk.ExecutionSample` 54MB（245k 条事件） | **8.8 s** | **180 MB** |
| `print`（全量，3.3M 事件） | 26 s | 2.6 GB |
| fuzz 5 万例（结构感知变异） | 10.3 s | —— |

全量 `print` 的内存是「先收齐再按时间戳出队」（真 jfr 同构），3.3M 条事件 × 内联栈帧
就是这个量级；**带 `--events` 过滤时只留匹配的事件**，所以常用路径是 180MB。
峰值内存由工具自己上报（`--stats`，走 `K32GetProcessMemoryInfo`）。

## fuzz

```bash
./…/release/jfrfuzz.exe --cases 50000 --file seed.jfr
```

- 六成样本是**真 `.jfr` 的结构感知变异**（截断 / 翻位 / 插入 / 清零），
  四成是纯随机字节（一半带正确魔数，否则全被第一道检查挡掉）。
- 每例在 `catch_unwind` 里跑，覆盖 `from_bytes` / `summary` / `chunk_data` / `events`。
- 结果：**5 万例 0 panic**（2026-11-17 实测，seed=12345）。

## jhsdb 的边界

真 `jhsdb`（Serviceability Agent）靠 **ptrace / 调试器**读别的进程内存，Windows 上没有。
这里做的是「目标自己上报」：只服务**跑着 vmless-attach 的 VMless JVM**，
对真 HotSpot 无效（真 HotSpot 请用 JDK 自带的 `jhsdb`）。没有 `--core` 模式、
没有反汇编/寄存器视图；`--heap` 是 VM 上报的堆布局摘要，不是从原始内存刨出来的对象图。

## 已知差异 / 未做

- `jfr print --xml` 未实现（`--json` 有）。
- 命名差异：我们的 `jdk.types.ClassLoader` 带一个 `id` 字段（真文件没有，
  它的 `(id = N)` 是常量池键），读真文件时按池键反查。
- 多 chunk 文件的时间线是**跨 chunk 统一按结束时间出队**（真 jfr 是逐 chunk 排序），
  单 chunk 文件逐字节一致；多 chunk 的 `summary` 也逐字节一致。
- 全量 `print` 3.3M 事件时内存 2.6GB（见上表）。

## 更多

- 需求边界与验收记录：`docs/requirements/REQ-001-jfr.md`
- 改动历史：`CHANGELOG.md`
- 给 agent 的约定（确切命令、禁区）：`AGENTS.md`
