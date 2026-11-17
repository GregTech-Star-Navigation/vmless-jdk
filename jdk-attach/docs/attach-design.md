# 机制选择与踩到的坑

> 状态：2026-11-17 · 对应 `vmless-attach 0.1.0`

## 1. 为什么用 hsperfdata 那套（而不是自造一个目录）

要求里写了"套路参考 HotSpot，机制可以自选"。最后**照抄了 HotSpot 的两个机制**，理由不是
"像"，而是"真工具能直接用"：

```
%TEMP%\hsperfdata_<用户>\
    <pid>          perfdata 二进制（HotSpot v2.0 格式）
    （没有第二个文件）
127.0.0.1:<随机端口>   attach 控制通道（令牌在 perfdata 里）
```

- perfdata 格式一字节不差地对齐 HotSpot，所以 **真 jps / 真 jstat 直接读我们的 VM**
  （`jstat -gc` 的 19 列全部有值）。这不是炫技：它把"列名对齐"从"我抄了表头"
  变成"真工具算出来的就是这些列"，验伪成本降到零。
- 进程发现靠 perfdata 文件本身（文件名 = pid），`jps` 不 attach，所以永远不会被
  挂死的目标拖住。
- 命令行控制走自己的 TCP 协议（HotSpot 是 `.attach_pid` 触发文件 + 信号/管道）。
  不走触发文件的原因：跨平台要处理信号与命名管道两套，而且**我们控制的就是 VM 自己**，
  监听线程一直在，不需要外部文件把它叫醒。

### 没有第二个"端点文件"

一开始我写了一个 `<pid>.attach`（port + token），结果踩了一整晚：**文件会被清掉**，
表现是 `jps` 看得见、`jcmd` 说"不是 VMless VM"。排查到后来决定**消灭这个故障类**：

> 端口和令牌就是两个 perfdata 计数器（`vmless.attach.port` / `vmless.attach.token`）。

好处：只有一个共享文件，不存在"两个文件不一致 / 谁把谁删了 / 名字对不上"。
安全性不变 —— perfdata 本来就在按用户隔离的 `%TEMP%` 下。

## 2. 五个必须知道的坑（每一个都让工具"静默失败"）

### ① 魔数是**大端**，其余字段是**小端**

真文件前 4 字节是 `ca fe c0 c0`，不是 `c0 c0 fe ca`。读端用 `ByteBuffer.getInt(0)`
（默认大端）校验，按小端写会得到 `Bad Magic: c0c0feca`。

### ② Windows 上读的不是文件，是**具名共享内存**

`Perf.attach0` → `OpenFileMapping(FILE_MAP_READ, false, "hsperfdata_<用户>_<pid>")`。
只写文件的话，真工具报 `Could not attach to <pid>`（`IOException: Could not open
PerfMemory, error 2`）。名字是**实测**出来的（用 `OpenFileMapping` 逐个候选试，只有一个能开）。
所以 `PerfFile::create` 里必须：

```rust
CreateFileMappingW(file_handle, …, PAGE_READWRITE, 0, size, "hsperfdata_<user>_<pid>")
```

文件本身仍要写 —— 那是给 `jps` 枚举 pid 用的面包屑。

### ③ 字符串条目的 `vector_length` 必须 > 0

`type='B'` 在 HotSpot 里只有"字节数组"一种形态：字符串也是 `'B'` + `units=STRING` + `vl>0`
（vl = 槽容量）。`vl=0` 会让读端抛
`Unexpected type code encountered: … type_code = byte (0x42)`。

### ④ `generation.0.space.0` 是 **Eden**，不是 Survivor0

按"0、1、2 = S0、S1、Eden"的直觉写，真 jstat 会把 S0C 和 EC 的值对调 —— 数字都"像样"，
但全错。真实顺序：`space.0 = Eden`，`space.1 = S0`，`space.2 = S1`。
另外 `jstat -gc` 的 `GCT` 列要 `collector.0/1/2` 三个都在（少一个整列都算不出来）。

### ⑤ Windows 上 `accept()` 出来的 socket **继承非阻塞**

监听器为了轮询 accept 被设成了 nonblocking。accepted socket 在 Windows 上继承这个模式，
于是服务端第一次 `read` 就拿到 `WouldBlock`，然后按"读失败"关连接 —— 客户端看到的是
`连接被对端关闭`，而且**只在客户端发得比服务端读得慢时复现**（典型竞态：加了日志就"好了"）。
修法：`serve()` 里第一件事 `s.set_nonblocking(false)`。

## 3. 协议

```text
 0        1        2        3        4                8
+--------+--------+--------+--------+----------------+
| 0x56   | 版本   |  op    | flags  |  payload_len   |   8 字节头
+--------+--------+--------+--------+----------------+
|                   payload（≤1 MiB）                   |
+-----------------------------------------------------+
```

- `flags & 0x80` = 回复；`flags & 0x01` = 还有后续帧（大载荷分块，hprof 走这条路）。
- 回复的 `payload[0]` 是状态码：`0 OK / 1 BadToken / 2 BadVersion / 3 BadOp /
  4 BadCommand / 5 Failed / 6 Unsupported / 7 Internal`。
- **版本不符也回一个合法帧**（`BadVersion` + 服务端支持的版本号），所以客户端拿到的是
  "协议版本不符"而不是"连接被重置"。
- 载荷上限 1 MiB 是**先校验再分配**的（防止对端乱发把内存撑爆）。

## 4. 并发与串行化

- accept 线程非阻塞轮询（20ms），每个连接一个线程，上限 64。
- **命令执行用一把互斥锁串行化**：两个客户端同时 `GC.run` 会把 VM 状态搞乱，串行是这里
  唯一正确的选择。响应仍然各自回各自的连接。
- 标记为兼容性测试：16 个客户端并发跑 `VM.version` / `GC.heap_info` / `Thread.print`
  → 16/16 成功。

## 5. jstatd：为什么不是 RMI

真 jstatd 是 RMI 服务（JRMP + Java 序列化 + 动态 stub 绑定）。它**唯一的作用**是把
perfdata 的字节搬给客户端。这里直接做那条路：

```
Hello（匿名，只读服务）
Command "list"              → "pid<TAB>mainClass\n"…
Command "perf <pid>"        → perfdata 原始字节（分块）
Command "cmd <pid> <命令>"   → 文本（间接 attach 目标）
```

客户端侧是 `jstat -gc <pid>@<host>:<port>` —— **真 jstat 的 vmid 语法**
（`<lvmid>[@<hostname>[:<port>]]`），所以命令行是兼容的，只是底层不是 RMI。

远端拿到的还是同一份 perfdata，因此列定义、格式、缺失列的 `-` 全都一致。
默认只 bind 127.0.0.1；要对外必须显式 `--bind 0.0.0.0`，并且要知道 jstatd 按设计是
**匿名只读**的（JDK 自带的也一样，这正是历史上 jstatd 被打的原因）。
