# vmless

**用 Rust 从零写的一套 JDK 25**：自己的编译器、自己的 VM（解释器 + JIT）、自己的 GC、自己的 39 个工具。
没有 `jvm.dll`、没有 HotSpot —— **它能直接执行 OpenJDK 自己的 `.class`**。

```bash
bin/javac.exe   H.java        # 自研编译器：.java → 标准 .class（真 JVM 也能加载）
bin/java.exe    H.class       # 自研 VM：解释器 + JIT + 分代 GC
VMLESS_JDK=<jdk/lib/modules>  # 指定后，java.base 走 OpenJDK 自己的字节码
```

这条路线的取舍：**JDK 的类库不自己重写**（`java.base` 有 7350 个类，重写不现实），
而是让自研 VM 去执行 OpenJDK 编好的字节码；**VM 之下的一切都是自己写的** ——
字节码解释器、Cranelift JIT、分代 GC、类加载器、native 层（JNI 与会话内建）、JPMS、线程与监视器。

- 逐条指令 + 逐条语法：[docs/支持矩阵.md](docs/支持矩阵.md)
- **现在做到哪、什么不能用、踩过哪些坑**：[docs/状态.md](docs/状态.md)
- 内部结构：[docs/架构.md](docs/架构.md) · 为什么这么设计：[docs/设计.md](docs/设计.md)
- 构建与验收：[docs/工程.md](docs/工程.md) · 命令行契约：[docs/接口.md](docs/接口.md)
- 给 agent 的约定：[AGENTS.md](AGENTS.md)

## 当前状态（2026-11-18）

| 判据 | 当前值 | 怎么测 |
| --- | --- | --- |
| **三路一致**（HotSpot / 解释器 / JIT 逐字节） | **103 / 103，失败 0** | `bash verify.sh <bin目录>` |
| 单元测试 | **106 passed** | `cargo test --release` |
| **自研 javac 编 `java.base`** | **73.9%**（2497 / 3381 个源文件） | `cover_dir <源码根> <classpath>` |
| 字节码覆盖 | 解释器 **200/202**、JIT **197/202** | 见支持矩阵 |
| `java.base` native 注册 | **约 800 条** | `grep -c 'r\.add(' src/vm/nat_*.rs` |
| 代码规模 | **109,852 行 Rust** / 8 个 crate / 82 次提交 | — |

### 已完成的能力

| 能力 | 实测证据 |
| --- | --- |
| **执行真 OpenJDK 字节码** | `VMLESS_JDK` 指 `lib/modules`，`String` / `ArrayList` / `HashMap` 全走 OpenJDK 自己的 class |
| **类来源三路** | 目录 / **jar（自写 inflate，零依赖）** / **jimage 真二进制格式**（30482 条） |
| **JIT** | Cranelift；优化器已开、除余常数换魔数（**i32 对 2^32 全量穷举验证**）、局部变量驻留寄存器、方法内联、cmp 与跳转融合 |
| **GC** | **精确引用位图**（字节码数据流分析，96.3% 槽精确、失配 0）、标记-清扫不搬动、并行标记；**停顿 12.5 ms ≈ G1 的 12.7 ms** |
| **线程** | 每 Java 线程一个 OS 线程、无全局锁（真并行）、`monitorenter/exit` 可重入、`LockSupport.park` 带许可 |
| **JNI** | `JNINativeInterface_` 236 槽位 / **196 条真实现**，含 `RegisterNatives`、回调进 Java、`ThrowNew` |
| **反射 + 注解** | `Field.get/set`、`Method.invoke`、`getAnnotation` 一族、`getParameters0`（真读 `MethodParameters`） |
| **JPMS** | 70 个模块 `describe` 内容全对；`--add-opens` / `--add-exports` / `-m` 与真 JDK 一致 |
| **`invokedynamic`** | `LambdaMetafactory` / `StringConcatFactory` / `MethodHandles` 构造族 + 调用点缓存 |
| **`java.desktop`** | `BufferedImage` + `Graphics2D`（fillRect/drawLine/Blit + SrcOver）**与 HotSpot 逐像素相同** |
| **工具 39 个** | `jimage extract` 逐字节相同、`jmod` 真 JDK 能读、我们签的 jar 真 `jarsigner` 验得过、真 `jfr` 能读我们录的文件 |
| **attach 协议** | perfdata 用 **HotSpot v2.0 二进制格式** → **真 `jps`/`jstat` 能直接读我们的 VM** |

### 性能（同机、同一份 `.class`、多次取中位）

|  | vmless | HotSpot 25 | |
| --- | --- | --- | --- |
| P1（4 线程 × 800 万累加） | **22–34 ms** | 44–48 ms | **快约 2×** |
| P2（同 + 字符串拼接） | 90–105 ms | 77–86 ms | 落后 1.03–1.2× |
| GC 最大停顿 | **12.5–12.9 ms** | 12.7 ms | 持平 |
| 启动（空 main） | **133.9 ms** | 727.3 ms | 快 5.4× |
| 常驻 RSS | **5.5 MB** | 42.7 MB | 省 7.8× |
| 解释执行（无 JIT） | 慢 96.9× | — | 已知短板（JIT 方法级回退兜住） |

### 还没做到的

> 这一节 2026-11-20 重新核对过（旧版停在 11-18，五项里已有四项做完）。

| 缺口 | 说明 |
| --- | --- |
| **自研 javac 产出的 class 有 11% 过不了真 JVM 验证器** | 425 个抽样类里 46 个被 `-Xverify:all` 拒载（栈图/帧生成）。这是「生产级」的最后一关 |
| **自研 javac 编 `java.base` 85.2%** | 剩 436 编译失败 + 65 解析失败，长尾：泛型返回不代回、非匿名内层类的 `this$0`、`case Type var ->`、`record` |
| **`drawString` 与 HotSpot 逐像素不一致** | 窗口和绘制都通了（GDI 接管），但 JDK 25 的字体栈走 HarfBuzz/FFM，字形栅格化结果与 HotSpot 不同 |
| **FFM（`java.lang.foreign`）** | 本机无 libffi，`LibFallback.doDowncall` 明确报错 |
| **`Continuation` / 虚拟线程** | 虚拟线程用平台线程顶（语义等价），`Continuation` 明确抛 UOE |
| **attach 服务端未接** | `jstack`/`jmap`/`jstat`/`jcmd` 对真 HotSpot 进程可用（读真 hsperfdata），对自己 `rustjava` 进程还不行 |

**已经做完的**（旧版列在这里，现在全部达成）：

| 原缺口 | 现状 |
| --- | --- |
| 帧栈模型 | ✅ `Thread.getStackTrace` / `StackWalker` / 死锁报告 / 未捕获异常栈，与 HotSpot 逐字节一致 |
| `Condition.await/signal` | ✅ 根因是 `thread_unpark` 的 MutexGuard 自死锁，修好后全通 |
| AWT 真窗口 | ✅ 真实 Win32 窗口，客户区能画矩形/文字/空心框（`awtE/evidence/screen_z7.png`） |
| 内存映射文件 | ✅ `FileChannel.map` / `MappedMemoryUtils` 全通，与 java 逐字节一致 |

详见 [docs/状态.md](docs/状态.md)。

## 装

三种方式，任选：

| 方式 | 命令 | 说明 |
|---|---|---|
| 便携包 | 解压 `vmless-<ver>-win-x64.zip` | 只有 `bin/` + `README.txt` + `LICENSE`，不写注册表 |
| 安装包 | 运行 `vmless-<ver>-setup.exe` | 装到 `%LOCALAPPDATA%\vmless`，可选加入用户 PATH，带卸载 |
| 源码 | `cargo build --release --bins --examples` | 需要 Rust；构建与验收命令见 [docs/工程.md](docs/工程.md) |

产物由 `packaging/build.sh` 一条命令出（zip + 安装包 + `SHA256SUMS` + `release-notes.md`）。

## 第一个例子

```java
public class Hello {
    public static void main(String[] a) {
        System.out.println("hello, vmless");
    }
}
```

```bash
bin/javac.exe Hello.java out          # 自研编译器 → out/Hello.class
bin/java.exe  out/Hello.class         # 解释器
```

跑真实库代码（用 OpenJDK 自己的 `java.base`）：

```bash
export VMLESS_JDK='C:/Program Files/Eclipse Adoptium/jdk-25.0.4.101-hotspot/lib/modules'
bin/run_jdk.exe 'D:/out' Hello        # 目录 + 主类名
```

两个引擎的输出必须逐字节一致 —— 这正是验收脚本在做的事：

```bash
bash verify.sh <bin目录>              # 103 个样例，三路对照（解释器/JIT/HotSpot）
```

## 仓库结构

| 路径 | 内容 |
|---|---|
| `rustjavac/` | 编译器（`front`/`sema`/`codegen`）+ VM（解释器/JIT/GC/native）+ 类加载 |
| `librustjrt/` | 内存层：分代堆、TLAB、写屏障、卡表、根集、句柄表 |
| `jdk-tools/` `jdk-crypto/` `jdk-gui/` `jdk-jfr/` `jdk-krb/` `jdk-attach/` | 6 个工具 crate（39 个 exe） |
| `samples/` | 验收样例（`.java`），CI 与本地验收共用 |
| `scripts/` | `verify-samples.sh`（样例三方对照） |
| `packaging/` | 打包脚本（portable zip + NSIS 安装包 + Release 清单） |
| `docs/` | [状态](docs/状态.md) · [架构](docs/架构.md) · [设计](docs/设计.md) · [工程](docs/工程.md) · [接口](docs/接口.md) · [支持矩阵](docs/支持矩阵.md) |
| `patches/` `launcher/` `rustcore/` | 早期「无 JVM 的 `javac.exe`」那条线（OpenJDK 补丁 + native-image 入口），现已是支线 |

## 许可

`patches/` 下的文件源自 OpenJDK，沿用 **GPLv2 + Classpath Exception**。
其余原创代码的许可证**尚未指定** —— 分发包里的 `LICENSE` 目前是 GPLv2 + CPE 全文占位，定了之后替换即可。
