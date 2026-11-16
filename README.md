# vmless

**不带 JVM 的 Java 工具链**：自己把 `.java` 编成 `.class`，自己把 `.class` 跑起来。
没有 `jvm.dll`、没有 HotSpot、没有 `java.exe` —— 三个原生可执行文件搞定编译与执行。

```
bin/compile_src.exe   .java → .class
bin/rustjava.exe      解释器：直接跑 .class
bin/run_jit.exe       JIT：字节码 → Cranelift IR → 本机机器码
```

目标是**把 Java 从 JVM 里拆出来**：编译器、执行器、内存管理都是自己写的 Rust 实现，
产出的 `.class` 仍然是标准字节码（能被真 JVM 加载 —— `verify.sh` 就是拿 HotSpot 当参照物）。

- 支持范围（逐条指令 + 逐条语法）：[docs/支持矩阵.md](docs/支持矩阵.md)
- 内部结构（流水线、两个引擎、内存层）：[docs/架构.md](docs/架构.md)

## 装

三种方式，任选：

| 方式 | 命令 | 说明 |
|---|---|---|
| 便携包 | 解压 `vmless-<ver>-win-x64.zip` | 只有 `bin/` 三个 exe + `README.txt` + `LICENSE`，不写注册表 |
| 安装包 | 运行 `vmless-<ver>-setup.exe` | 装到 `%LOCALAPPDATA%\vmless`，可选加入用户 PATH，带卸载 |
| 源码 | `cargo build --release --bin rustjava` + `--example compile_src --example run_jit` | 需要 Rust（实测 1.100）|

产物由 `packaging/build.sh` 一条命令出（zip + 安装包 + `SHA256SUMS` + `release-notes.md`），
详见 [packaging/README.md](packaging/README.md)。

## 第一个例子

写 `Hello.java`：

```java
public class Hello {
    public static void main(String[] a) {
        System.out.println("hello, vmless");
    }
}
```

编译并运行（实测输出 `hello, vmless`，退出码 0）：

```bash
bin/compile_src.exe Hello.java out     # hello: 379 字节 → out/Hello.class
bin/rustjava.exe out/Hello.class       # 解释器
bin/run_jit.exe  out/Hello.class       # JIT（stderr 会打编译/执行耗时）
```

> 上面的 `bin/` 指**装好之后**的布局（便携包解压 / 安装包装完都长这样）。
> 直接在仓库里用源码构建时，三个程序的路径是
> `rustjavac/target/release/rustjava.exe` 与 `rustjavac/target/release/examples/{compile_src,run_jit}.exe`。

两个引擎的输出必须逐字节一致 —— 这正是验收脚本在做的事：

```bash
bash scripts/verify-samples.sh          # 20 个代表样例，三方对照（解释器/JIT/HotSpot）
bash scripts/verify-samples.sh --all    # samples/ 下全部样例
```

## 能跑什么（摘要）

完整版在 [docs/支持矩阵.md](docs/支持矩阵.md)（29 项 ✅ / 3 项 ⚠️ / 13 项 ❌，每条都有实测依据）。

| 类别 | 支持 |
|---|---|
| 基础语句、循环、递归、三元、标签跳转 | ✅ |
| `switch`（int / String / enum，含箭头 `case ->`） | ✅ |
| 类、继承、`super`、虚分派、接口（含 `default`/`static`） | ✅ |
| 枚举、嵌套类型、静态字段与 `<clinit>` | ✅ |
| 泛型**擦除**（含桥接方法）、变参、for-each | ✅ |
| 异常：`throw` / `try-catch-finally` / 多重 catch / try-with-resources | ✅ |
| 数组（含多维、`new int[3][]`）、字符串拼接（真 StringBuilder） | ✅ |
| `long` / `double` 全链路 | ✅ |
| lambda、方法引用、`record`、文本块、`instanceof` 模式绑定、`var` | ❌ |
| 装箱/拆箱、非静态内部类 | ❌（能编过，但生成的字节码被 HotSpot 判 VerifyError）|
| 线程、反射、JNI、类加载器、jar | ❌（设计上没有）|
| `float` 算术 | ❌（相关指令一条没实现）|
| 包名 + `import` 跨编译单元 | ❌（当前提交上解析有 bug，见支持矩阵）|

## 和真 JVM 差在哪

**没有**的东西，不打算有：

- **没有 JNI**，`System.loadLibrary` 之类不存在
- **没有线程**：`java.lang.Thread` 跑不了，`synchronized` 语法接受但**不加锁**（不生成 `monitorenter`）
- **没有反射**：`Class.forName`、注解运行时读取都没有
- **没有类加载器**：类路径就是「入口 `.class` 所在目录树」，运行期不能决定加载什么
- **不支持 jar / 模块系统**：只吃 `.class` 文件

**不一样**的地方：

- `switch` 不生成 `tableswitch`/`lookupswitch`，降级成比较链 —— 分支多时比 HotSpot 慢
- 方法调用没有内联、没有去虚拟化；JIT 是「方法级一次编译」，不是分层热点编译
- GC 是**分代但非并发**的：最大单步停顿很小（见下），但不追求吞吐峰值

## 基准（历史实测，出处逐条标）

> 下列数字来自 `CHANGELOG.md` 里记录的实测，**本轮没有重测**；每次记录的机器状态与日期不同，比较的是同一次记录里的两个数。

| 指标 | 结果 | 对照 | 出处 |
|---|---|---|---|
| 启动耗时（减去 50 ms 进程创建基线） | `rustjava` **≈2 ms** | `java` ≈96 ms | CHANGELOG `0.15.0` |
| 分代堆分配（8M 分配 / 1M 存活） | **10.2 ns/对象**（多类 + 屏障 + TLAB） | 旧实现 10.8 ns/对象（单类、无屏障） | CHANGELOG `0.36.0` |
| 同上，与 G1 交替 4 轮取最小值 | **4.0 ns** | G1 12.6 ns | CHANGELOG `0.18.0` |
| 一次回收（同上场景） | **34.5 ms** | G1 39.8 ms | CHANGELOG `0.18.0` |
| 最大单步停顿 | **0.021 ms**（0.17 实现）/ **0.918 ms**（0.36 新实现） | G1 23.4 ms | CHANGELOG `0.17.0` / `0.36.0` |
| `fib(30)`（270 万次调用） | **8.1 ms** | HotSpot 247 ms | CHANGELOG `0.21.0` |
| 50M 次循环累加 | 解释器 3281.9 ms / **JIT 140.9 ms**（快 23.3×） | HotSpot 278 ms（含 96 ms 启动） | CHANGELOG `0.20.0` |
| 单对象内存 | **12–16 B** | HotSpot 24 B | CHANGELOG `0.17.0` |
| bump 分配 vs `Box`（10M 对象，4 字段） | 19.1 ns/对象 | `Box` 69.9 ns/对象 | CHANGELOG `0.2.0` |
| JIT 侧 GC 压测（50 万次短命对象） | `ALLOC_NS=188.4 GC_COUNT=6 LIVE_AFTER=4026528 HEAP_MB=3.84` | 分配量翻 4 倍堆占用不变 | CHANGELOG `0.39.0` |

复现用的基准程序在 `librustjrt/examples/bench_*.rs` 与 `rustjavac/examples/bench_jit*.rs`：

```bash
cd librustjrt && cargo run --release --example bench_final    # ALLOC_NS / GC_MS / MAX_STEP_MS / BYTES_PER_OBJ
cd rustjavac  && cargo run --release --example bench_jit_gc
```

## 已知坑

- `rustjava.exe` / `run_jit.exe` 的路径参数**要带目录**（`out/Hello.class`）。裸文件名（`Hello.class`）
  在 `0.42` 上会报「没有可执行的 class」（加载器把空的父路径当作类目录）。
- `rustjavac/samples/` 不存在时，crate 里那个「class 往返逐字节一致」的测试会**自动跳过** ——
  样例集现在放在仓库根的 `samples/`。
- 仓库历史版本号：`CHANGELOG.md` 记到 `0.42.0`，而 `rustjavac/Cargo.toml` 还是 `0.1.0`（没跟着动）。
  打包脚本按 `Cargo.toml` 取版本，两者不一致时会打警告并在 release notes 里写明。

## 仓库结构

| 路径 | 内容 |
|---|---|
| `rustjavac/` | 编译器（crate 名 `rustjavac`，库名 `classfile`）+ 解释器 + JIT |
| `librustjrt/` | 内存层：分代堆、TLAB、写屏障、卡表、根集 |
| `rustcore/` | 给 OpenJDK `javac` 名字池用的 Rust arena（另一条线）|
| `launcher/` `patches/` | 无 JVM 的 `bin/javac.exe` 那条线（OpenJDK 补丁 + native-image 入口）|
| `samples/` | 验收样例（`.java`），CI 与本地验收共用 |
| `scripts/` | `verify-samples.sh`（样例三方对照）、`opcode-support.mjs`（指令支持表生成）|
| `packaging/` | 打包脚本（portable zip + NSIS 安装包 + Release 清单）|
| `verify.sh` | 本机全量验收（106 个样例，路径写死在本机）|
| `docs/` | 架构、支持矩阵 |

## 许可

`patches/` 下的文件源自 OpenJDK，沿用 **GPLv2 + Classpath Exception**。
其余原创代码的许可证**尚未指定** —— 分发包里的 `LICENSE` 目前是占位说明，定了之后放进仓库根即可。
