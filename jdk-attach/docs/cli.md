# 命令行：七个工具、退出码、超时

> 状态：2026-11-17 · 对应 `vmless-attach 0.1.0`

## 退出码表（所有工具一致）

| 码 | 含义 | 典型触发 |
| --- | --- | --- |
| 0 | 成功 | |
| 1 | 命令行用错 | 缺参数、未知选项、不支持的 hostid |
| 2 | 目标 VM 不存在 / 不是 VMless VM | pid 不存在、进程已退出、只有 perfdata 没有 attach 端点 |
| 3 | 协议错误（含**版本不符**、令牌错） | 服务端 `BadVersion` / `BadToken` |
| 4 | 超时 | 半帧、对端不响应 |
| 5 | 连接 / IO 失败 | 端口连不上、perfdata 读不出来、写文件失败 |
| 6 | 目标明确拒绝 | 命令不存在、`jinfo` 改不了的标志 |

> 真 JDK 的这些工具基本都只返回 1。这里细分是为了脚本能分辨"没这个 VM"和"连不上"。

## 超时

- 默认 **2000 ms**，所有网络操作都设了（connect / read / write 三处）。
- 通用写法：`-t <毫秒>`、`-t<毫秒>`、`--timeout <毫秒>`。
- **`jstat` 例外**：`-t` 是真 jstat 的"加时间戳列"。所以：
  `-t`（不跟数字）= 时间戳；`-t<数字>` = 超时；`--timeout <数字>` = 超时。

## 各工具

### jps
```
jps [-q] [-mlvV] [<hostid>]
  -q 只 pid    -l 全类名    -m main 参数    -v JVM 参数    -V 只要 pid+JVM 参数
```
数据源是 perfdata 文件（**不 attach**）。顺手清理死进程留下的残留。
本实现只支持本地；给 hostid 会明确报错（退出码 1）。

### jstat
```
jstat -<option> [-t] [-h<lines>] <vmid> [<interval> [<count>]]
  <option> = class | compiler | gc | gcutil
  <vmid>   = <pid>  或  <pid>@<host>:<port>   （后者走 jstatd）
  <interval> = <n>[ms|s]
```
列定义、宽度、格式全部来自 JDK 里的 jstat option 规格（`option gc { column { header ^S0C^ … } }`）。
**缺的计数器输出 `-`**（与真 jstat 一致）。

### jcmd
```
jcmd [-l] [-h] [<pid> | <main-class>] <command...>
```
`help` / `VM.version` / `VM.uptime` / `VM.flags` / `VM.get_flag` / `VM.set_flag` /
`VM.system_properties` / `VM.command_line` / `VM.info` / `VM.native_memory summary` /
`GC.heap_info` / `GC.run` / `GC.class_histogram` / `GC.class_stats` / `Thread.print [-l]` /
`PerfCounter.print`，外加每条的 `<cmd>_help`。

### jinfo
```
jinfo [option] <pid>
  （无）  -flags  -sysprops  -flag <name>  -flag +<name>  -flag -<name>  -flag <name>=<值>
```
写成功**不输出**（与真 jinfo 一致）；改不了的标志打印原因并返回 6。

### jmap
```
jmap -histo[:live] <pid>
jmap -dump:format=b,file=<路径> <pid>
jmap -clstats <pid>
```
`-histo` 的表头/列宽与真 jmap 一致（含 `(module)` 列是否出现的判定）。
`-dump` 走流式通道，服务端不会把整个 hprof 攒在内存里。

### jstack
```
jstack [-l] <pid>
```
第一行是时间戳（真 jstack 有、`jcmd Thread.print` 没有），所以时间戳由本工具加。
`-l` 追加 ownable synchronizers 与**死锁检测**。

### jstatd
```
jstatd -p <port> [--bind <addr>] [-n <name>]
```
默认 bind 127.0.0.1。不是 RMI，理由见 [attach-design.md](attach-design.md) 第 5 节。

### attachstress（自检工具，不是 JDK 工具）
```
attachstress --probe <pid>           1000 次 attach/detach + 句柄数变化
attachstress --endpoint <pid>        打印端口与令牌
attachstress --bad-token <pid>       错令牌（期望 exit 3）
attachstress --bad-version <pid>     错协议版本（期望 exit 3）
attachstress --partial <pid>         半帧后干等（期望 exit 4）
attachstress --concurrent <pid> <n>  n 个并发客户端
attachstress --missing <pid>         不存在的目标（期望 exit 2）
```

### hprofcheck（自检工具）
```
hprofcheck [--names] <file.hprof>
```
严格校验 hprof 结构（`jhat` 已从 JDK 移除，只能这么验）。
