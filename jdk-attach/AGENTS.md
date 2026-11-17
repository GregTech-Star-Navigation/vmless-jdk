# AGENTS.md —— 本仓库的约定（给 agent 看）

> 状态：2026-11-17 · `vmless-attach 0.1.0`

## 这是什么

VMless JVM 的 attach 协议 + perfdata 计数器。八个 bin、零依赖。
面向的使用者是**另一个仓库里的 VM**（`../rustjavac`），以及要排查线上进程的人。

## 命令（都实测过）

```bash
export PATH="$PATH:/c/Users/Administrator/.cargo/bin"
export CARGO_TARGET_DIR='D:\vmless\rt_att1'      # 别用默认 target，和别的 crate 抢锁
cargo build --release                            # 8 个 exe
cargo test --lib                                 # 42 个单元测试，必须全绿
cp /d/vmless/rt_att1/release/*.exe /d/vmless/batt/
```

跑验收（Windows PowerShell 5.1，脚本是 **ASCII only** —— 5.1 会把无 BOM 的 UTF-8
当 GBK 读，中文注释会把脚本解析坏）：

```bash
powershell -NoProfile -ExecutionPolicy Bypass -File D:\vmless\batt\accept.ps1
```

## 关键约定

- **不引第三方依赖**。要加之前先问：标准库能不能做？协议/文件格式都是自定义的，
  真需要序列化再谈。
- **不依赖 `../rustjavac`**。它在被另一个 agent 改；把它当依赖会让本 crate 的构建
  跟着它一起红。本 crate 不需要读 class 文件。
- **所有网络 IO 必须有超时**（connect/read/write 三处）。新增通道时照抄
  `client.rs` 的三处 `set_*_timeout`。
- **不 panic 是硬指标**：pid 不存在、协议版本不符、超时、半帧、目标中途退出，
  全部走 `AttachError` + 明确的退出码（见 `docs/cli.md`）。
- **测试不许写"看起来对"的断言**。能对照真 JDK 输出的地方一律对照
  （`jstat` 的列宽、`jmap -histo` 的表头、`jstack` 的行格式都是从真输出量的）。
- 排查协议问题的开关：`VMLESS_ATTACH_LOG=<文件>`（服务端写日志到文件），
  `VMLESS_ATTACH_DEBUG=1`（两边都往 stderr 打）。**不要用 stderr 做长期日志** ——
  Windows 上隐藏窗口/重定向会把调用方的输出搅乱。

## 禁区

- 不要改 `../rustjavac`、`repo/`、`dllwork/`、`jbcov/`、`jdkapi/`、`jdk-tools/`、`jdk-crypto/`。
- 不要把 `%TEMP%\hsperfdata_*` 里的文件提交或复制进仓库。
- 不要按进程名杀 `node.exe`（dsh 自己就是 node）。

## 改格式时先读

- `docs/attach-design.md` —— 5 个会让工具"静默失败"的坑（魔数大端、Windows 具名共享内存、
  字符串 `vector_length`、`generation.0.space.0` 是 Eden、accept 继承非阻塞）。
- `docs/hprof.md` —— HPROF 是大端、类型码是 JVM 编号、以及"为什么没法用真工具验证"。
