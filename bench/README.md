# vmless vs HotSpot 25 基准套件 —— 方法学

> 这份文档说的是「怎么测的、为什么这么测、哪些数字不能信」。
> 实际跑出来的对比表在 [`REPORT.md`](REPORT.md)，机器可读的原始数据在 [`result.json`](result.json)。

## 一条命令复跑

```bash
bash bench/run.sh                 # 全部维度，约 10-25 分钟（取决于机器负载）
bash bench/run.sh --only gc       # 只跑某一节
bash bench/run.sh --quick         # 每项 2 次，快速冒烟（不生成报告）
bash bench/run.sh --rebuild       # 重新编译基准程序
```

产物：

| 文件 | 内容 |
| --- | --- |
| `bench/REPORT.md` | 人看的对比表，每行注明谁快、快多少倍 |
| `bench/result.json` | 机器读：每个用例的 min/median/p90/max、峰值 RSS、退出码、程序自报数字 |
| `bench/raw/*.jsonl` | 每次运行一行（`{id,rep,wall_ms,peak_kb,exit,timedOut}`），可自己重新统计 |
| `bench/logs/*.log` | 子进程的 stdout+stderr（最后一次重复的那次），程序自报的毫秒数从这里解析 |

## 被测对象

| 角色 | 路径 | 说明 |
| --- | --- | --- |
| HotSpot 25 | `C:/Program Files/Eclipse Adoptium/jdk-25.0.4.101-hotspot/bin/java.exe` | Temurin 25.0.4.1+1 |
| vmless 解释器（内建 java.base 子集） | `D:/vmless/bparv/rustjava.exe` | 0.57.0 |
| vmless 解释器（真 java.base，5421 类） | `D:/vmless/bparv/run_jdk.exe` | 0.57.0，`VMLESS_JDK` 指向 `lib/modules` |
| vmless JIT | `D:/vmless/bparv/run_jit.exe` | 0.57.0，Cranelift |

基准程序一律用 **真 javac 25** 编译（`bench/out/`），这样两边跑的是同一份字节码。

## 机器与噪声

36 逻辑核 / 64 GB RAM / Windows Server。**这台机器不是安静环境**：

1. **每次创建进程都要过一遍带外扫描器** —— 一个 `hostname.exe` 的墙钟就要 110~150 ms。
   所以所有**墙钟**对比都同时给 `min` 与 `median`，并额外给一列「扣底噪」（min − 底噪）；
   `min` 是主口径，因为它受噪声污染最小。
2. **跑基准时机器上还有别的编译任务**，CPU 负载 25%~45%。这会把 `median` 抬高 1~2 倍
   （实测 `java -version` 的 min 469 ms / median 1152 ms）。
3. 因此：**同一个用例内部比较（我们 vs HotSpot）是可信的**（交错跑，噪声对两边一样），
   **绝对数字不要拿去跟别的机器比**。

`probe.exe` 做三件事来保证公平：交错执行（每轮把所有用例各跑一次，机器漂移均摊到所有用例）、
用 `CreateProcess` 自己起进程以便**在子进程退出后仍持有句柄**读取 `PeakWorkingSetSize`、
每轮都重新计时。

## 各维度方法学

### 1 启动时间

- 30 次/项，交错；取 `min`、`median`，另给「扣底噪」。
- 用例：`-version`、空 main、HelloWorld、1000 行类（`Big1000`，250 个静态方法）。
- 三个入口各测一遍：`rustjava`（内建 java.base 子集）、`run_jit`（JIT 冷启）、
  `run_jdk`（真 java.base 全量类表）。
- **`-version` 只有 HotSpot 有**：vmless 的三个运行时都没有 `-version`/`--version` 选项
  （会报「不认识的选项」）。所以表里 HotSpot 的 `-version` 与 vmless 的「空 main」并列，
  作为「最小启动」的对照 —— 两者做的事并不相同，这一点在 REPORT 里明写。

### 2 内存占用（峰值 RSS）

- 峰值 RSS 用 `GetProcessMemoryInfo(...).PeakWorkingSetSize`，在子进程退出**之后**、
  句柄仍打开时读取（这是 `probe.exe` 自己 `CreateProcess` 的原因）。
- 场景：空 main（常驻）、`Alloc8`（800 万次分配、对象立即死亡）、`Gc`（1/8 存活）。
- **对象大小按布局公式给**：`vmless = align8(HEADER + 字段数×4)`，`HEADER = 8`；
  HotSpot 25 = 12 B 头 + 压缩 oops + 8 B 对齐。RSS 斜率只用来交叉验证 HotSpot 那一侧
  （我们的堆是整块 `realloc` 增长，按 64 MB 步进，斜率会被容量台阶吃掉 —— REPORT 里说明了）。

### 3 GC 停顿

- 负载：`Gc.java` —— 8,000,000 次分配、1/8（100 万）存活、`Node{int,long}`。
- 两边都限制堆 256 MB：HotSpot `-Xmx256m`，vmless `VMLESS_HEAP_MB=256`。
- HotSpot 用默认 G1，`-Xlog:gc:file=...%p.log` 每次跑一个文件，解析 `Pause ... X.XXXms` 行，
  取**最大单次**与**总和**。
- vmless 用 `VMLESS_GC_STATS=1`，运行时在退出时打印
  `[gc] ... minor=N major=M 暂停=Xms 最大=Yms ...`，直接取这两个数。
- 解释器那条路**没有停动遥测**（`src/vm/gc.rs` 不计 pause），只能给程序耗时，标 n/a。

### 4 常驻内存

空 main 跑完的峰值 RSS（就是 §2 的第一行）。注意 `run_jdk` 因为要 eager 装 5421 个类，
常驻是 111 MB 级 —— 这不是「空 VM」，是「类表已装好的 VM」。

### 5 类加载

两种口径，**不可直接比**，REPORT 里分开列：

- HotSpot：`ClassLoad2` 用 `Class.forName(name,false,loader)` 加载 5000 个 java.base 类，
  程序自报毫秒；另有 `-Xshare:off` 的空 main 作为「冷启不读 CDS」的参照。
- vmless：`run_jdk` 启动即 eager 装 5421 个 java.base 类（运行时自己打印「类表: N 个类」），
  墙钟就是这个成本；我们**跑不了** `Class.forName` 那条路
  （解析 `ConcurrentHashMap` 时缺 `jdk.internal.access.JavaLangAccess.currentCarrierThread()`）。

### 6 解释执行吞吐

- 程序：`P2j`（8M×4 次循环、无 invokedynamic），同一份字节码两边都跑；
  另跑任务指定的原始 `P2`（带 `+` 字符串拼接 → invokedynamic）。
- 取**程序自报的内层毫秒数**（`System.nanoTime()` 前后差），把启动成本排除掉。
- 解释器入口用 `run_jdk`（真 java.base）；`rustjava` 跑不了这些程序（缺 `nanoTime` 解析）。

### 7 JIT 吞吐

- `run_jit` 跑 `P2j`（可编译）、以及原始 `P2`（会**整体退回解释器**，因为 JIT 不支持
  `invokedynamic`）—— 后者说明「JIT 数字偏低」的一个真实原因。
- **已知偏差**：JIT 目前是「一处不支持就整个程序退回解释器」，
  所以任何用到 lambda / 字符串拼接 / `synchronized` 的程序在 `run_jit` 下都拿不到 JIT 数字。
  报告里凡遇到这种情况都注明，并同时给 `run_jdk`（真字节码解释路径）的对照值。

### 8 并行扩展性

- 程序形状与 `D:/vmless/jdkms/par/P1.java` 相同：起 N 个线程，**每个线程各做 8M 次循环**
  （总工作量随 N 增长），线程数 1/2/4/8 各编一个类（`P1L1..P1L8`）。
- **加速比按吞吐算**：`N × t(1) ÷ t(N)`。完全并行 = N，等于 1 = 一点没并行。
- 为什么不用 `Thread` 子类那版（`P1j*`）：**我们的解释器不派发 `Thread` 子类的 `run()` 覆写**，
  会静默输出 0（HotSpot 输出 42）。这是运行时的 bug，已单独记录；基准改用 lambda 版，
  两边跑同一份字节码。
- **不测 `D:/vmless/jdkms/thread/` 下的样例**（有泄漏 bug，会把内存吃光）。

## 已知偏差与限制（读数字前先看这一节）

| 现象 | 影响 | 处理 |
| --- | --- | --- |
| 进程创建底噪 110~150 ms | 所有墙钟绝对值 | 报告给「扣底噪」列；比较时看 min |
| 机器有别的任务（25~45% CPU） | median 偏高 1~2 倍 | min 为主口径，交错跑 |
| `run_jit` 遇 indy 整程序回退 | JIT 数字偏低 | 报告注明 + 给 `run_jdk` 对照 |
| `Thread` 子类不派发 `run()` | 并行维度不能用那版程序 | 改用 lambda 版 `P1L*` |
| 解释器无 GC 停顿遥测 | GC 维度只有 JIT 侧数字 | 标 n/a 并说明 |
| vmless 无 `-version` | 启动维度口径不完全对齐 | 用空 main 对照并注明 |
| 我们的堆按 64 MB 步进增长 | RSS 斜率法失效 | 对象大小改用布局公式 |

## 目录结构

```
bench/
  run.sh                 一条命令跑完（环境自检 → 编译 → 跑批次 → 汇总）
  README.md              本文（方法学）
  REPORT.md              跑出来的对比报告（由 run.sh 生成）
  result.json            结构化结果（由 run.sh 生成）
  progs/                 基准程序的 .java 源码
  out/                   用 javac 25 编出来的 .class（每个程序一个目录）
  tools/
    probe.cs / probe.exe 测量核心：交错跑、记 wall_ms 与 PeakWorkingSetSize、落日志
    cases.mjs            用例清单（id / 程序 / 参数 / 环境变量 / 重复次数）
    mkmanifest.mjs       把用例按 (section,env,reps) 分组，生成 probe 的 manifest
    summarize.mjs        读 raw/*.jsonl + logs/*.log → REPORT.md + result.json
  raw/                   每次运行一行 JSON
  logs/                  子进程输出
  tmp/                   临时产物（fixtures、大文件、中间数据），可随时删
```
