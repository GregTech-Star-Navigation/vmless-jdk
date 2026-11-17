# AGENTS.md — 给 agent 的项目约定

## 确切命令

```bash
export PATH="$PATH:/c/Users/Administrator/.cargo/bin"
cd /d/vmless/jdk-jfr
export CARGO_TARGET_DIR='D:\vmless\rt_jfr1'     # 换并行构建时用 rt_jfr2…

cargo build --release          # 产物：D:\vmless\rt_jfr1\release\{jfr,jhsdb,jfrfuzz}.exe
cargo test                     # 26 + 5 + 1 个测试
D:/vmless/rt_jfr1/release/jfrfuzz.exe --cases 50000 --file D:/vmless/seed.jfr
```

真 JDK 对照（改格式相关代码后必须重跑）：

```bash
export MSYS_NO_PATHCONV=1
JB="/c/Program Files/Eclipse Adoptium/jdk-25.0.4.101-hotspot/bin"
"$JB/jfr.exe" summary D:/vmless/t.jfr > /tmp/ref_sum.txt
/d/vmless/rt_jfr1/release/jfr.exe summary D:/vmless/t.jfr > /tmp/our_sum.txt
cmp /tmp/ref_sum.txt /tmp/our_sum.txt        # 必须无输出
```

`print` / `metadata` 同理。**这里的判据是 `cmp`，不是「看着像」**。

## 代码约定

- 注释只写「为什么」，而且优先写**踩坑点**（例如「常量池链是倒着走的」）。
  代码本身能说清的事情不要复述。
- 解析路径不许 `unwrap`/`expect`/索引切片：一律 `Cursor` + `Result`。
- 加新字段/新类型时：先确认它在真文件里的 `simpleType` / `dimension` /
  `constantPool` 三个标志，用 `jfr inspect --type <名字> <文件>` 看。
- 改写出侧后，必须用**真 jfr** 读一遍（`jfr print`），只跑自己的 reader 不算验证。

## 禁区

- `D:\vmless\jdk-attach` 只读（依赖它的公开 API，不改它）。
- 不要 `git add` 任何 `.jfr` 产物（`t.jfr` / `big.jfr` / `ours.jfr` 是测试素材，
  几十 MB，不进仓库）。
- 不要按进程名杀 node / java：本机 dsh 自己是 node。

## 已知坑（见 README 的「格式要点」）

`byte` 是裸字节、`int` 要截断、key 0 可能是真值、`ticksPerSecond` 用浮点除、
`@Timespan` 有 5 种单位、`StackTrace.frames` 不能进池、`fileState` 必须 0。
