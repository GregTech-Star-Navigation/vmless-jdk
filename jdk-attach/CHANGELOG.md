# CHANGELOG

## 0.1.0 — 2026-11-17

首个版本：attach 协议 + 性能计数器，支撑 `jps / jstat / jcmd / jinfo / jmap / jstack / jstatd`。

### 新增

- **perfdata 写入（HotSpot v2.0 二进制兼容）**：`%TEMP%\hsperfdata_<用户>\<pid>`。
  真 `jps` / 真 `jstat` 可直接读我们的 VM（`-gc` 19 列、`-gcutil` 13 列、`-class` 5 列、
  `-compiler` 6 列全部有值，表头逐字符一致）。
- **attach 控制通道**：回环 TCP + 8 字节头帧协议（版本号参与握手、1 MiB 分块、
  128 位令牌）。端点信息写在 perfdata 的两个计数器里，**不留第二个文件**。
- **`VmProbe` 接口**：VM 侧只需实现 `perf()` 即可让 `jps`/`jstat` 工作；其余能力
  （堆直方图/线程栈/堆转储/类加载器/标志读写）逐块可选。另有最短接入路径
  `vmless_attach_start(name, pid)` + `perf_counter_set(id, value)`。
- **七个工具**：`jps`（-q/-l/-m/-v/-V）、`jstat`（gc/gcutil/class/compiler + 远端）、
  `jcmd`（12 条命令 + help）、`jinfo`（读/写标志、系统属性）、`jmap`（histo/clstats/dump）、
  `jstack`（-l 含死锁检测）、`jstatd`（TCP 服务，非 RMI）。
- **HPROF 1.0.2 导出**（大端、id_size u4、JVM 类型码）+ `hprofcheck` 严格校验工具。
- **自检工具** `attachstress`：1000 次 attach/detach 句柄数、错令牌/错版本/半帧/并发。

### 设计取舍

- 不引任何第三方依赖（perfdata 是二进制文件、协议是自己定的，用不上序列化库）。
- `Cargo.toml` **不依赖** `../rustjavac`：骨架里写的 `classfile = { path = "../rustjavac" }`
  编译不过（那个 package 名是 `rustjavac`），而且本 crate 一个 class 文件都不需要读。
- `jstatd` 不做 RMI/JRMP：它唯一的作用是把 perfdata 字节搬给客户端，直接做那条路，
  客户端侧用真 jstat 的 vmid 语法 `<pid>@<host>:<port>`。

### 已知限制

- `jmap -dump` 的 `CLASS_DUMP` 常量池/静态字段段与 JDK 25 的实际编码不完全一致；
  本库恒写 0，故不触发。真实工具（MAT）能否打开**未在本机验证**（`jhat` 已从 JDK 移除）。
  详见 [docs/hprof.md](docs/hprof.md)。
- `jstatd` 不是 RMI，真 `jstat` 连不上它；用本库的 `jstat` 走 `pid@host:port`。
- 线程栈只在 VM 如实上报"持有/等待哪把锁"时才能做死锁检测。
