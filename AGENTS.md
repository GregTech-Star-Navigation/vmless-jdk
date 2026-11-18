# AGENTS.md — 给 agent 的项目约定

> 这份文件是**可执行的事实**，不是愿景。里面的命令都实测过；没跑过的会标「（未验证）」。
> 更新：2026-11-18 · `rustjavac 0.67.0`

## 这是什么

一套用 Rust 从零写的 JDK：自己的 `javac`、自己的 VM（解释器 + JIT）、自己的 GC、自己的 39 个工具。
**能直接执行 OpenJDK 自己的 `.class`**（`jimage` / `jar` / 目录三路类来源），不依赖 `jvm.dll`。

目录（`D:\vmless\`）：

| 路径 | 是什么 |
| --- | --- |
| `repo/` | **git 仓库，唯一真相源**。所有改动最终要落到这里 |
| `rustjavac/` | 编译器 + VM + JIT + native 实现（主力工作区，**不是 git 仓库**） |
| `librustjrt/` | 内存层（分代 GC、卡表、TLAB、句柄表） |
| `repo/jdk-tools` `jdk-crypto` `jdk-gui` `jdk-jfr` `jdk-krb` `jdk-attach` | 6 个工具 crate |
| `patch/img/` | 真 JDK 的 `lib/modules` 解开后的类树 —— **`VMLESS_JDK` 指这里** |
| `jbsrc/java.base/` | `java.base` 的 3381 个 `.java` 源码（测量编译率用） |
| `src/*.java` | 106 个手写样例（`verify.sh` 用） |
| `btools/` | 39 个工具 exe（从 `repo/jdk-*` 编出来后收集到这里） |

## 确切命令

### 构建

```bash
cd /d/vmless/rustjavac
export PATH="$PATH:/c/Users/Administrator/.cargo/bin"
export CARGO_TARGET_DIR='D:\vmless\rt_xxx'      # 每次换一个新的，见「坑」
cargo build --release --bins --examples
```

### 验收（三道，缺一不可）

```bash
# ① 三路一致：HotSpot / 解释器 / JIT 输出逐字节相等（含退出码）
#    入参是「含 rustjava.exe + compile_src.exe + run_jit.exe 的目录」
cp 'D:\vmless\rt_xxx\release\rustjava.exe' /d/vmless/bxxx/
cp 'D:\vmless\rt_xxx\release\examples\'{compile_src,run_jit,run_jdk}.exe /d/vmless/bxxx/
bash /d/vmless/verify.sh /d/vmless/bxxx        # 期望：三路一致 103  失败 0

# ② 单元测试
cd rustjavac && cargo test --release            # 期望：106 passed

# ③ GC 压力模式（漏根的主检出通道，必须也全绿）
VMLESS_GC_STRESS=1 bash /d/vmless/verify.sh /d/vmless/bxxx
```

### 测量 javac 的 `java.base` 编译率

```bash
# 必须带 classpath，不带只有 22%
'D:\vmless\rt_xxx\release\examples\cover_dir.exe' \
    'D:/vmless/jbsrc/java.base' 'D:/vmless/patch/img/java.base' \
    --md rustjavac/docs/java-base-失败分布.md
```

### 跑真实字节码

```bash
export VMLESS_JDK='D:/vmless/patch/img'
./bxxx/run_jdk.exe 'D:/out' MainClass      # 目录 + 主类名
```

## 代码约定

- **中文注释，写「为什么」不写「是什么」**。踩过的坑必须留注释，格式：
  「症状 → 根因」，例如：
  ```rust
  // **必须真挂住**。`Reference$ReferenceHandler.run()` 是 `while(true){...}`，
  // 立刻返回会让它 100% CPU 空转，把别的线程饿死 —— 症状是「卡死、有 CPU、无输出」。
  ```
- **native 分组文件**：`nat_intrinsics` / `nat_os` / `nat_util` / `nat_nio` / `nat_misc` / `nat_desktop`。
  新增 native 只改自己那份，`register_all` 里各调一次，**不要动 `native_reg.rs`**（多条并行线）。
- **`add_override` 的陷阱**：`exec.rs::is_override_owner` 是**白名单**，
  类名不在里面注册了也永远不会被调用。要加类就改那一个 `matches!`。
- **`java/lang/invoke/*` 不走 native 注册表**：`exec.rs` 的三条 invoke 分支先判
  `is_invoke_api_class`（前缀匹配）交给 `indy.rs::invoke_api`，在那里注册是死代码。

## 禁区

| 别碰 | 为什么 |
| --- | --- |
| `patch/img/` 下的 `.class` | 那是真 JDK 的类，是 `VMLESS_JDK` 的数据源 |
| `repo/` 之外的 `.git` | `rustjavac/` 等目录**不是** git 仓库，改了不会自动有历史 |
| `C:\Users\Administrator\.dsh\attachments\**` | 内容寻址、只增不删。删了会让**每一轮**都抛 `Attachment object is missing`，整个会话报废 |
| 进程名杀 node | DSH 自己是 `node.exe`。清理残留要按命令行特征过滤 |

## 坑（按症状索引）

| 症状 | 根因 / 对策 |
| --- | --- |
| `LNK1104` / `Device or resource busy` | 带外文件监视器锁住刚跑过的 exe。**覆盖会静默失败**（cp 失败但你跑的是旧 exe）。对策：每轮复制到新目录，`CARGO_TARGET_DIR` 准备两三个轮换 |
| 改了代码但行为没变 | 同上 —— 你可能在跑旧 exe。先确认 exe 的 mtime |
| 构建失败但报错在别人的文件里 | 多条并行线在改同一个树。比较 `src/vm/*.rs` 的 mtime 是否晚于自己最后一次成功构建；不是自己弄坏的**不要去改** |
| 通过率突然掉一大截 | `cover_dir` 的 `platform()` 整个进程只反解一次 classpath，一个 `.class` 读失败就静默当「类不存在」→ **先重跑再怀疑代码** |
| 程序卡死、有 CPU、无输出 | 先用 `VMLESS_HANG_DUMP=1`（进程级指令计数，每 1600 万条打一次「当前 类.方法@pc」）。注意：**dump 不出来 ≠ 不是 spin** —— 「方法进→native→返回」这种循环每轮只有几十条指令，也到不了阈值；用 `Get-Process` 的 CPU 时间两次采样判断 |
| `verify.sh` 全绿但真程序挂了 | 那 103 个样例只用 ASCII 路径、不建线程、不用原子类。**真程序路径要单独验** |
| 编译器「编过了」但产出坏 class | 用 `javap -p -c` 解析一遍 + `-Xverify:all -Xbootclasspath/a:` 跑真 JVM 对照 |

## 调试开关

| 变量 | 作用 |
| --- | --- |
| `VMLESS_JDK` | `java.base` 的类来源（`lib/modules` 或解包目录） |
| `VMLESS_GC_STATS=1` | GC 停顿/回收统计 |
| `VMLESS_GC_STRESS=1` | 每次分配都 GC（漏根检测） |
| `VMLESS_GC_OFF=1` | 关回收，量「改前」基线 |
| `VMLESS_HANG_DUMP=<秒>` | 卡死探针，每约 1600 万条指令打一次位置 |
| `VMLESS_JIT_NO_INLINE=1` | 关 JIT 内联（二分用） |
| `VMLESS_TRACE=1` | 每条字节码一行 —— **几秒就刷爆屏，只在极小样例上用** |

## 提交

一次提交只做一件事。标题 ≤50 字符，类型 `feat` / `fix` / `refactor` / `perf` / `docs` / `test` / `chore` / `build`。
正文写「为什么」。

**修 bug / 加能力必须同时动版本号**：`rustjavac/Cargo.toml` 的 `version` + `CHANGELOG.md` 加一节
（只修 bug → patch，加能力 → minor）。纯文档/测试/重构不动版本号。

提交信息末尾带 `Co-Authored-By` 由主 agent 统一处理，子代理不要自己加。
