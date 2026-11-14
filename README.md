# vmless-jdk

一个**不含 JVM** 的 JDK 25 编译工具链：`javac.exe` / `jar.exe` 是原生可执行文件，
目录里没有 `jvm.dll`、没有 `libjvm`、没有 HotSpot。

同时把 javac 内部的**名字池**接到 Rust 分配器（`rustcore`），
让编译器的一部分内存不走 Java 堆。

## 组成

| 目录 | 内容 |
|---|---|
| `launcher/` | `bin/javac.exe` 的入口：从 exe 位置推导 `java.home`，默认补 `--release 25` |
| `patches/` | 打进 OpenJDK 源码树的补丁（名字池 → Rust arena） |
| `rustcore/` | Rust 侧的内存 arena（`arena_new` / `arena_alloc` / `arena_free` / `arena_stats`） |
| `scripts/` | 构建脚本（需按本机路径修改） |

## 工作原理

```
Java 侧 javac  ──SharedNameTable──►  RustNameArena (FFM downcall)
                                          │
                                          ▼
                                    rustcore.dll
                                 bump arena, 一次 free 全释放
```

- 名字池的字节不再分配在 Java 堆上，`dispose()` 时整块还给 Rust。
- FFM 下调用在 native-image 里需要 `foreign.downcalls` 元数据（参数类型写 `jlong` / `void*`）。

## 构建（Windows）

需要：OpenJDK 25 源码树、Cygwin、Visual Studio 2022 Build Tools、Windows SDK、GraalVM 25（提供 `native-image`）。

1. 把 `patches/src/**` 覆盖到 OpenJDK 源码树的对应路径
2. `make images`（详见 [scripts/](scripts/)）
3. 用 `launcher/` 编译并 `native-image` 出 `bin/javac.exe`
4. `cargo build --release` 生成 `rustcore.dll`，放进 `bin/`

## 状态

| 项 | 状态 |
|---|---|
| `bin/javac.exe` 独立编译（无需 `-D`/`JAVA_HOME`） | ✅ |
| `bin/jar.exe` | ✅ |
| 名字池 → Rust arena | ✅ 产物 sha256 与标准 javac 一致 |
| `bin/java` | ❌ 按设计不提供；后续由自研引擎 `rustjava` 顶替 |
| `javap` / `jdeps` | ⚠️ 构建成功，`lib/modules` 的 jrt-fs 解析未打通 |
| 注解处理器 | ❌ 需要执行引擎 |

## 许可

`patches/` 下的文件源自 OpenJDK（GPLv2 + Classpath Exception），沿用同一许可。

## 运行时（线 B，进行中）

`librustjrt/` 是 Rust 版的 Java 运行时子集：Java 对象 = Rust 结构体（16 字节头 + 8 字节字段槽），
由 bump arena 分配，一次释放整块。

```
cargo run --release --example bench
bump   :     19.1 ns/对象   10M 个对象 191 ms，458 MB
Box    :     69.9 ns/对象   10M 个对象 699 ms
```
