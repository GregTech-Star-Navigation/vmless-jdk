# CHANGELOG

## 0.1.0 — 2026-11-17

首个可用版本：`.jfr` 的读、写、录制，以及 `jhsdb` 等价能力。

### 读（`jfr summary` / `print` / `metadata`）

- 从 `src.zip` 的 `jdk.jfr.internal.*` 逐行对齐二进制格式：chunk 头（68 字节）、
  元数据字符串池 + 元素树、常量池链（**倒着链**，delta 为负）、事件块、以及
  每种类型的值编码。
- 与真 `jfr` 的输出**逐字节一致**（JDK 25 录的 275KB/1 chunk 文件；`summary`
  204 行、`print` 40540 行、`metadata` 6408 行全部 `cmp` 通过）。
- 54MB / 5 chunk / 3.02M 事件的录制，`summary` 也逐字节一致。
- 为了对上字节，照抄了真 jfr 的几个怪癖：`summary` 表头与表格行**行分隔符不同**、
  字段值里的换行原样输出、同一时间戳的事件顺序按 `PriorityQueue` 的 sift 规则
  （而不是稳定排序）。
- 事件值里的常量池引用**按需单层解引用**，不展开进值树：54MB 文件的
  `print --events jdk.ExecutionSample` 从 2.5GB/53s 降到 180MB/8.8s。

### 写（`Recorder` + `jfr record`）

- `Recorder::start(config)` → 环形缓冲 → `dump(path)`，产出真 `jfr` 读得动的 chunk。
- 内置 5 个事件类型：`jdk.ExecutionSample`、`jdk.GCPhasePause`、`jdk.JavaMonitorEnter`、
  `jdk.ThreadStart`、`jdk.ThreadEnd`、`jdk.ClassLoad`（字段顺序与真 JFR 定义一致）。
- 真 `jfr print` 能直接打出线程名、方法名、行号、栈帧（说明常量池、注解、
  `@ContentType` 都对上了）。
- `jfr record --pid <pid>` 走 vmless-attach：`Thread.print` 采样 → `ExecutionSample`，
  perfdata 的 GC 计数 → `GCPhasePause`。

### jhsdb

- `jmap --histo/--heap/--clstats`、`jstack`、`jinfo`、`--heap`、`--locks`，
  数据来自 vmless-attach（**只服务 VMless 的 JVM**，Windows 上没有 ptrace）。

### 健壮性

- 解析入口全部返回 `Result`：截断、坏魔数、版本不支持、偏移越界、池下标越界、
  类型未定义、数组长度离谱 —— 都是明确错误，不 panic。
- `jfrfuzz`：5 万例结构感知变异 + 随机字节，**0 panic**（10.3s）。
- `--stats` 上报耗时与峰值内存（`K32GetProcessMemoryInfo`）。
