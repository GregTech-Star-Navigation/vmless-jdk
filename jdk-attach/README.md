# vmless-attach

VMless JVM 的 **attach 协议 + 性能计数器**，目标是让 `jps / jstat / jcmd / jinfo / jmap / jstack / jstatd`
这七个工具能对一个 VMless VM 干活 —— 而且**真的**干活，不是打桩。

- **零外部依赖**（一个 crate 都不引）。perfdata 是二进制文件、attach 走自有协议，
  用不到序列化库，也不需要 class 文件解析。
- **计数器格式与 HotSpot 二进制兼容**：真 `jps` / 真 `jstat` 能直接读我们的 VM
  （实测见下）。这比"我们的工具读我们的文件"强得多 —— 它把"列名对齐"从
  "我抄了表头"变成"真工具算出来的数就是这些列"。
- **列定义照抄 JDK 的 jstat 规格**（那份规格是 `lib/modules` 里的文本资源，
  `option gc { column { header ^S0C^ data ... } }`）。不是猜的。

## 30 秒上手

```bash
# 1. 构建（零依赖，离线也能编）
export PATH="$PATH:/c/Users/Administrator/.cargo/bin"
export CARGO_TARGET_DIR='D:\vmless\rt_att1'
cd /d/vmless/jdk-attach
cargo build --release          # 8 个可执行文件
cargo test --lib               # 42 个单元测试

# 2. 起一个"参考 VM"（实现了 VmProbe 的 vmsim：真对象表、真标记清扫 GC、真线程）
cp /d/vmless/rt_att1/release/*.exe /d/vmless/batt/
./vmsim.exe --deadlock --threads 2

# 3. 用工具看它（同时真 jps/真 jstat 也看得见）
./jps.exe -l
./jstat.exe -gc <pid> 1000 3
./jcmd.exe <pid> VM.version
./jmap.exe -histo <pid>
./jstack.exe -l <pid>
```

## 文档

| 文档 | 内容 |
| --- | --- |
| [docs/api-for-vm.md](docs/api-for-vm.md) | **VM 侧要做什么** —— 接 attach 只需要实现 `VmProbe`（或更少的 `perf_counter_set`） |
| [docs/attach-design.md](docs/attach-design.md) | 机制选择与踩到的 5 个坑（魔数大端、具名共享内存、vector_length、空间顺序、非阻塞继承） |
| [docs/cli.md](docs/cli.md) | 七个工具的参数、退出码表、超时语义 |
| [docs/hprof.md](docs/hprof.md) | HPROF 结构与校验方式（`jhat` 已死，怎么证明文件能打开） |

## 与 HotSpot 的对齐程度（实测）

| 能力 | 状态 |
| --- | --- |
| 真 `jps` 列出我们的 VM | ✅ 显示主类名 |
| 真 `jstat -gc/-gcutil/-class/-compiler` | ✅ 19/13/5/6 列全部有值，表头逐字符一致 |
| 我们的 `jstat` 输出 | ✅ 行宽与真 jstat 相同（`-gc` 200 列），右边界逐个对齐 |
| `jcmd` 12 条命令 | ✅ 真做（堆是真扫的、GC 是真跑的、栈是真线程的） |
| `jmap -histo` | ✅ 逐对象扫描聚合 |
| `jmap -dump` hprof | ✅ 严格解析器校验通过（见 docs/hprof.md） |
| `jinfo` 读写标志 | ✅ 读全部、写 manageable 的、改不了的明确报错 |
| `jstack -l` 死锁 | ✅ `Found one Java-level deadlock:` |
| `jstatd` | ⚠️ **不是 RMI**：换成同语义的 TCP 服务（见 docs/attach-design.md 末节） |
