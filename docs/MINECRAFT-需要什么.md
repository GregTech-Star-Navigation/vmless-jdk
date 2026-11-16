# Minecraft 需要什么 —— 量化缺口清单

> 状态：分析完成（只测不改） · 2026-11-17 · 口径与全部命令见 §6
> 基线：`rustjavac` 解释器 177/202 指令、JIT 157/202（`node scripts/opcode-support.mjs --stats`），
> Rust 代码 25,083 行（`find rustjavac/src librustjrt/src -name '*.rs' | xargs wc -l`）。
> 本文只回答「跑 Minecraft 还缺什么、各有多大」，不含实现方案细节。

## 1. 结论摘要

1. **能跑，但不是现在这个 VM 能跑**：MC 本体 0 个 native、0 处 `java.lang.foreign`，全部 native 债务在
   **JDK 自己（java.base 748 个 native / 107 个类）** 和 **LWJGL（约 5,345 个 JNI 入口）** 上；
   我们目前**没有任何 native 方法机制**（`rustjavac/src` 里 grep `ACC_NATIVE` 零命中）。
2. **差多少**：字节码层只差 9 条「MC 必命中」的指令（`invokedynamic` 命中 5,665/11,383 个类、
   `tableswitch` 321 个类、`fconst_*` 942 个类），但真正的大头是四块基础设施：**反射、`java.lang.invoke`、
   JNI、线程**——合计约 2.0–2.8 万行 / 32–48 周（1 人，乐观）。
3. **最短路径**：先补 9 条指令拿到里程碑（2–4 周）→ **只跑服务端**（7,762 类、0 LWJGL、0 foreign，
   26–38 周）→ 客户端最后再上 JNI+GL。FFM 替代 JNI 这条路**不成立**（见 §4 路线 C）。

## 2. Minecraft 的依赖清单

### 2.1 版本与 Java 要求（来源：Mojang version manifest）

| 版本 | 最低 Java | launcher json 的 `javaVersion` | 库数量 | 本机 win64 生效 | assets |
| --- | --- | --- | --- | --- | --- |
| 26.3（当前 release） | **25** | `java-runtime-epsilon` / majorVersion 25 | 114 | 74 | 34 objects / 487 MB |
| 1.21.11（最后的 1.21.x） | 21 | `java-runtime-delta` / majorVersion 21 | 107 | 75 | 29 objects / 455 MB |

- release 列表：`https://launchermeta.mojang.com/mc/game/version_manifest_v2.json`（latest release = `26.3`）
- 26.3 json：`https://piston-meta.mojang.com/v1/packages/9f67e10ed013deeaad20ac3c800652765d493408/26.3.json`
- 1.21.11 json：`https://piston-meta.mojang.com/v1/packages/812ee7fdfdf648bb872f7a1dca33718de4e786b4/1.21.11.json`
- 两者 `mainClass` 都是 `net.minecraft.client.main.Main`；服务端 jar 62.3 MB（内含 7 个库目录 + `server-26.3.jar` 真身）。

### 2.2 主要依赖（26.3，win64 生效面）

| 组件 | 26.3 版本 | 1.21.11 版本 | 用途 |
| --- | --- | --- | --- |
| LWJGL | **3.4.3**（`lwjgl`, `-opengl`, `-sdl`, `-stb`, `-freetype`, `-openal`, `-jemalloc`, `-shaderc`, `-spvc`, `-vma`, `-vulkan`） | **3.3.3**（`-glfw`，无 sdl/shaderc/spvc/vma） | 窗口、输入、GL、音频、字体 |
| log4j | 2.26.0（api/core/slf4j2-impl） | 2.25.2 | 日志 |
| netty | 4.2.16.Final（11 个模块） | 4.2.7.Final | 多人网络 |
| gson / guava | 2.14.0 / 33.6.0-jre | 2.13.2 / 33.5.0-jre | JSON / 集合 |
| authlib / brigadier / datafixerupper | 10.0.77 / 1.3.11 / 10.0.21 | 7.0.61 / 1.3.10 / 9.0.19 | 登录 / 命令解析 / 存档版本迁移 |
| joml / fastutil / icu4j | 1.10.9 / 8.5.18 / 78.3 | 1.10.8 / 8.5.18 / 77.1 | 数学 / 原生集合 / 本地化 |
| jna + jna-platform | 5.17.0 | 5.17.0 | 自带 JNI（!） |
| oshi-core / lz4-java / commons-* / jopt-simple | 6.9.0 / 1.10.1 / 1.28.0 等 | — | 系统信息 / 压缩 / CLI |

`jna` 的存在意味着：**即使不用 LWJGL，JNA 自己也是一层 JNI**（它 `dlopen` + libffi 调用）。
`com.mojang:jtracy`、`text2speech`、`blocklist` 为 Mojang 自研小库。

### 2.3 LWJGL 是 JNI 还是 FFM（关键结论）

| 证据 | 结果 |
| --- | --- |
| `javap -p -cp lwjgl-opengl-3.4.3 org.lwjgl.opengl.GL11C` | 62 个 `public static native`（`glEnable`、`glClear`…）→ **GL 入口仍是 JNI** |
| opengl 模块整体（493 类）native 方法数 | **2,278** 个，且 `org/lwjgl` 里 `java/lang/foreign` 引用 **0** 处 |
| lwjgl core 3.4.3 | 540 类中 **2,768** 个 native；**98 个类引用 `java/lang/foreign`**，全部在 `META-INF/versions/25/org/lwjgl/system/ffm/`（`FFM`、`BCCallDown`、`BCUtil`…），即 **FFM 只是 core 里的可选后端，GL 模块没有** |
| lwjgl core 3.3.3（1.21.11 用） | 335 类、2,027 个 native、`java/lang/foreign` 引用 **0** → **纯 JNI** |
| MC 26.3 client jar 里 `java/lang/foreign` 引用 | **0** 个类 |
| MC 26.3 的 JVM 参数 | 新增 `--enable-native-access=ALL-UNNAMED`、`--add-exports java.base/jdk.internal.misc=ALL-UNNAMED`、`-XX:StackShadowPages=32`；1.21.11 没有这些 |

**结论：跑 MC 要的是 JNI（native 方法链接 + 动态库加载），不是 `java.lang.foreign`。**
26.3 那个 `--enable-native-access` 更可能是为 `jdk.internal.misc`（JDK 24+ 受限方法）而加；
找不到任何 MC 侧 FFM 引用。

## 3. 我们缺什么

### 3.0 现状基线（都是实测）

| 项 | 数字 | 来源 |
| --- | --- | --- |
| 解释器 / JIT 指令覆盖 | 177 / 157 / 交集 156（共 202） | `opcode-support.mjs --stats` |
| **native 方法机制** | **完全没有**（grep `ACC_NATIVE`/`0x0100` 零命中） | `grep -rn ACC_NATIVE rustjavac/src` |
| 内建 JDK 方法表 | **74 条**（String / 5 包装类 / Object / System / Arrays） | `grep -c '    b("' rustjavac/src/jdk.rs` |
| 编译器吐出的 class 版本 | major **69**（Java 25） | `rustjavac/src/codegen/class.rs:436` |
| 支持矩阵语言层 | lambda / 方法引用 / record / 装箱 / 内部类 / 线程 / 反射 **都不通** | `docs/支持矩阵.md:5` |

### 3.1 P0 —— 缺了直接起不来

**P0-1 指令：MC 必命中的 9 组（括号内＝含该指令的类数，MC 客户端 / 全部 31,795 个依赖类）**

| 指令 | MC 客户端 26.3 | 服务端 | java.base | 依赖库 |
| --- | --- | --- | --- | --- |
| `invokedynamic` | **5,665** 类 / 32,235 次 | 4,204 类 | 642 | 2,510 |
| `tableswitch` | 321 | 199 | 323 | 428 |
| `lookupswitch` | 189 | 114 | 248 | 340 |
| `dup_x1` | 125 | 81 | 355 | **2,792** |
| `dup_x2` / `dup2_x1` / `dup2_x2` | 1 / 6 / 0 | 0 / 4 / 0 | 39 / 30 / 1 | 452 / 143 / 68 |
| `fconst_0/1/2` | 942 / 970 / 392 | 442 / 532 / 174 | 21 / 25 / 7 | 519 / 392 / 24 |
| `fadd / fsub / fmul / fdiv / frem / fneg` | 551/394/732/447/25/156 | 290/184/378/223/13/70 | 4/1/18/8/0/2 | 102/53/85/363/1/23 |
| `monitorenter` / `monitorexit` | 22 | 12 | 240 | 507 |
| `wide` | 3 | 2 | 19 | 40 |

另 12 条缺的指令（`jsr`/`ret`/`goto_w`/`jsr_w`/`swap`/`fstore_*`/`i2b` 等）在 MC+依赖里**零命中**，可以不急。
注意 `fload`/`dup2`/`i2b`/`iinc`/`fcmpl` 这几个「只有解释器有」的，跑 JIT 时一样会炸。

**P0-2 `java.lang.invoke`（`invokedynamic` 的落地，最大单块）**

MC 的 lambda、方法引用、`switch` 字符串、以及 javac 的字符串拼接**全走 indy**。至少要：
`MethodHandles.Lookup.findVirtual/findStatic/findSpecial/findConstructor/unreflect/privateLookupIn`、
`LambdaMetafactory.metafactory` + `altMetafactory`、`StringConcatFactory.makeConcatWithConstants`、
`MethodHandle.invokeExact/invoke`（8 个 native）+ `MethodHandleNatives`（12 个 native）、
`VarHandle` 全套（**31 个 native**：`get/set/getVolatile/compareAndSet/compareAndExchange/…`）、
`CallSite/ConstantCallSite/MutableCallSite` + 调用点缓存。java.base 里 `java/lang/invoke` 共 **265** 个类型。

**P0-3 反射**（Gson/DFU/log4j 全靠它）

- `java.lang.Class` **28 个 native**：`forName0`、`isInstance`、`isAssignableFrom`、`getSuperclass`、
  `getInterfaces0`、`getDeclaredFields0`、`getDeclaredMethods0`、`getDeclaredConstructors0`、`getModifiers`、
  `getPrimitiveClass`、`getName0`、`getDeclaredClasses0`、`getNestHost`…
- `java.lang.reflect.Array` **21 个 native**、`jdk.internal.reflect.ConstantPool` **18 个 native**
- `AccessibleObject.setAccessible`、`Proxy`/`InvocationHandler`、注解（`AnnotationParser`）
- java.base 里引用 `java/lang/reflect` 的唯一类型 **34** 个

**P0-4 native 方法机制（JNI）**

必须先有「认一个 native 方法」的能力，否则上面全是空话：

| 面 | 具体 API（native 个数） |
| --- | --- |
| 装载 | `System.loadLibrary`、`Runtime.loadLibrary0`、`ClassLoader.defineClass0/1/2`、`findLoadedClass0`、`findBootstrapClass`（7） |
| 内存 | `jdk.internal.misc.Unsafe` **68**（`allocateMemory`/`putInt`/`objectFieldOffset`…）；另外 52 个依赖类用 `sun.misc.Unsafe`（netty-common 24、lwjgl core 19、guava 5、log4j-core 2、joml/lz4 各 1） |
| 线程 | `java.lang.Thread` **20**（`start0`/`currentThread`/`sleep`/`interrupt0`/`setPriority0`）、`Object.wait/notify(All)`、`System.currentTimeMillis/nanoTime/arraycopy`（9） |
| 文件 | `sun.nio.fs.WindowsNativeDispatcher` **69**、`sun.nio.ch.FileDispatcherImpl` **22**、`java.io.WinNTFileSystem` 19、`RandomAccessFile` 10 |
| 网络 | `sun.nio.ch.Net` **40**、`IOUtil` 12、`NativeSocketAddress` 12（netty NIO 通道要用） |
| 压缩 | `java.util.zip.Inflater` 11 / `Deflater` 10（region 文件、资源包） |
| 图形（客户端） | LWJGL core **2,768** + opengl **2,278** + stb 224 + vma 75 个 JNI 入口；`NewDirectByteBuffer` / `GetDirectBufferAddress`（MemoryUtil 依赖 direct buffer） |

java.base 合计 **748 个 native / 107 个类**；`NIO`+`Unsafe`+`Thread`+`Class/反射` 四类就占 400 个左右。

**P0-5 线程与同步**（当前 VM 单线程、`synchronized` 只解析不加锁）

MC 至少 4 类线程：main、Render thread、Netty event loop、`Util.backgroundExecutor`。
要有 `Thread` 20 个 native + 真并发执行栈 + `monitorenter/exit` 语义 + `Object` 监视器。

**P0-6 类加载器 + 模块系统**（原版可用单 CL，**NeoForge 必需**）

NeoForge `26.3.0.104` 的启动 profile（installer 内 `version.json`）：
`mainClass = net.neoforged.fml.startup.Client`，JVM 参数 `--add-opens java.base/java.lang.invoke=ALL-UNNAMED`、
`--add-exports jdk.naming.dns/com.sun.jndi.dns=java.naming`，库含 **ASM 9.11**、`sponge-mixin 0.17.4`、`bus 8.0.5`、
`fancymodloader 12.0.12`。实测 `nfx`（1,580 类）里：

- `net/neoforged/fml/classloading/ModuleClassLoader` 调 `defineClass`（4 个类用到）
- `net/neoforged/bus/EventListenerFactory` 调 **`MethodHandles.Lookup.defineHiddenClass`**
- `java.lang.module` 的 **11 个类型**（`ModuleDescriptor`/`ModuleLayer` 链路/`ModuleFinder`/`ModuleReader`/`Configuration`/`ResolvedModule`…）在 18 个类里出现
- `java.lang.instrument` 在 8 个类里出现（mixin 自建 Instrumentation 的可能入口，**未实测是否真的走 `-javaagent`**）

### 3.2 P1 —— 能到主菜单但进不了世界

- **datafixerupper 10.0.21**：存档/数据迁移，反射 + record + 大量 `MethodHandle`；进世界第一步就是它。
- **世界读写**：`FileChannel` + `Deflater`（`.mca` region 压缩）+ 路径/属性 API（`Files`/`Path`）。
- **注解与泛型反射**：`ConstantPool` 18 个 native；`ParameterizedType`/`TypeVariable` 解析。
- **音频**：OpenAL（主菜单就有背景音，缺了通常不致命但会抛初始化异常）。

### 3.3 P2 —— 进得去但某个功能炸

| 面 | 证据 | 影响 |
| --- | --- | --- |
| JFR | MC 客户端 **183** 处引用 `jdk/jfr` | 崩溃报告/性能分析；launcher 还传了 `flightrecorder-config.jfc` |
| 桌面 API | `javax/swing` 52、`java/awt` 41 处引用 | 文件选择框、剪贴板 |
| 加密/TLS | `javax/crypto` 57、`java/security` 137 | 正版登录、多人 TLS |
| 字体/着色器 | freetype / shaderc / spvc / vma | 文字渲染、光影 |

### 3.4 P3 —— 性能

- JIT 只覆盖 157/202：float 运算全落解释器（MC 每帧几万次 `fmul/fadd`）。
- 无 `tableswitch`/`lookupswitch` 的 JIT 只能退化比较链；MC 里有 321 个类用它。
- 现有 GC（`young.rs`/`weak.rs`）能否扛住 MC 的分配速率**没测**。

### 3.5 规模估算（依据：现有 25,083 行 Rust ≈ 177 条解释器指令 + 157 条 JIT + 运行时 ⇒ 约 124 行/指令；native 按 30–50 行/个）

| 项 | 行数 | 人周 |
| --- | --- | --- |
| 补齐 9 组必命中指令（解释器+JIT） | 2,000–3,000 | 2–3 |
| `java.lang.invoke` + MethodHandle/VarHandle 运行时 | 4,000–6,000 | 6–10 |
| 反射（Class/Array/ConstantPool/AccessibleObject/Proxy/注解） | 2,500–3,500 | 4–6 |
| JNI 机制（JNIEnv 表 + loadLibrary + 符号解析 + direct ByteBuffer） | 3,500–5,000 | 6–8 |
| 线程与同步（重做执行模型：栈、监视器、ThreadLocal） | 4,000–6,000 | 8–12 |
| NIO/文件/zip/网络 natives | 3,000–4,000 | 5–7 |
| 模块系统 + 隐藏类（NeoForge） | 800–1,200 | 1–2 |
| **合计（客户端全量）** | **20,000–28,000** | **32–48 周** |
| 其中「只服务端」要砍掉 | −3,500–5,000（GL/JNI-GL 面） | −6–10 周 |

## 4. 三条（四条）候选路线对比

| 路线 | 需要什么 | 规模 | 风险 |
| --- | --- | --- | --- |
| **A 全客户端** | P0 全部 + JNI + GL/音频/字体 + direct buffer | 2.0–2.8 万行 / 32–48 周 | **最高**：JNI+OpenGL 是全新领域；失败现场是黑屏/驱动崩溃，几乎没有栈可看 |
| **B 只服务端** | P0 去掉图形面；保留 indy/反射/线程/NIO/Unsafe/zip | 1.6–2.3 万行 / 26–38 周 | **中**：无 GL 赌注，可用命令行自动化验证；硬骨头是线程模型与 `sun.misc.Unsafe`（netty-common 24、lwjgl core 19、guava 5 个类） |
| **C FFM 替代 JNI** | —— | **不成立** | ① MC 的 GL 入口是 JNI `native`（GL11C 62 个 / opengl 2,278 个）；② MC 0 处引用 `java.lang.foreign`；③ JDK 自己的 FFM 也带 28 个 native（`jdk/internal/foreign/abi/fallback/LibFallback`）；④ 换 FFM 还要多实现 `MemorySegment`/`MemoryLayout`/`Linker`/upcall stub |
| **D 里程碑路（中等复杂度真程序）** | 只补 9 组指令 + 最小反射，单线程 | 2,000–3,000 行 / 2–4 周 | **低**：目标程序建议 `brigadier`+`jopt-simple`（纯 Java、无 native、大量 indy 与 switch），能立刻暴露 VM 层缺陷 |

> 关于 FFM 的唯一成立用法：**将来**若 LWJGL 把 GL 也换成 FFM 后端（今天 opengl 模块 0 处 foreign 引用），
> 那时才轮到 `java.lang.foreign`。它不能替代现在的 JNI 工作，只能替换「以后那一层」。

## 5. 推荐路线与下一步三个动作

**推荐：D → B → A 三段走。先 2–4 周拿里程碑（D），再 26–38 周跑通服务端（B），客户端最后。**
理由：① 服务端 7,762 类里有 4,204 个类含 indy、0 个 LWJGL、0 个 foreign —— 它**就是**「MC 减去图形」，
验证价值与客户端同源；② 服务端可以用 `java -jar server.jar nogui` + stdin 命令脚本自动验收，
客户端只能靠人眼；③ JNI 那块（约 3,500–5,000 行）在服务端可以先只做 netty/zip 需要的最小子集，
把「JNI 设计对不对」这个最大不确定性推迟到有真实负载时再验证。

**下一步三个动作（都可独立验收）**

1. **补 9 组必命中指令 + 反向实验**（`tableswitch`/`lookupswitch`/`invokedynamic` 之外的 8 组先做，
   indy 单独一块）。验收：从 `client263.jar` 抽 200 个 `net/minecraft/**` 类，做 **load+verify 不执行**，
   断言「不认识的指令 = 0」；再对每个新指令写一条反向实验（拆掉实现 → 测试必须变红）。
2. **JNI 冒烟（120 行级）**：`System.loadLibrary` + 一个自编 `libsmoke.dll`（`int add(int,int)`）+
   `NewDirectByteBuffer` 往返一次。验收：Java 侧打印 `add(2,3)=5`、direct buffer 写入能被 native 读回。
   这一步同时决定客户端路线 A 的可行性上限。
3. **把估算换成实测清单**：写一个只 load+resolve 的扫描器，把 `server-26.3.jar` 的 7,762 个类全部加载、
   逐个 resolve 常量池里的方法/字段引用，输出「缺哪个 native / 缺哪个类」的 Top-N。
   产出应是**首启动阻塞项的真实列表**（预计头几名：`java/lang/invoke/*`、`jdk/internal/misc/Unsafe`、
   `sun/nio/ch/*`），用它替换 §3.5 的区间估算。

## 6. 全部数字的来源（命令 + 实际输出摘要）

> 统计脚本与扫描输出留在 `D:\vmless\.tmp-an\`（`scan.mjs` / `natives.mjs` / `*.txt`，453 KB）；
> 下载的 jar 与解包目录已清理，重跑需重新 `curl` + `unzip`。

```text
# 1) 版本面
curl -sS https://launchermeta.mojang.com/mc/game/version_manifest_v2.json   # latest release 26.3；923 个版本
curl -sS <26.3.json>    # javaVersion majorVersion=25，libraries=114，mainClass=net.minecraft.client.main.Main
curl -sS <1.21.11.json> # javaVersion majorVersion=21，libraries=107

# 2) 指令面（自写 class 解析器，统计每类出现次数）
node scan.mjs "D:/vmless/patch/img/java.base"   # classes: 7,350  major {52:1, 69:7349}  ACC_NATIVE 748
node scan.mjs mc26                              # classes: 11,383 major {69:11383}  indy 5,665 类
node scan.mjs mc121                             # classes: 10,291 major {65:10291} indy 5,203 类
node scan.mjs libsx                             # classes: 31,795（26.3 的 52 个 win64 依赖 jar）
node scan.mjs srvx                              # classes: 7,762（server-26.3.jar 真身） indy 4,204 类

# 3) native 面
node natives.mjs "D:/vmless/patch/img/java.base"   # 748 个 native / 107 个类；Unsafe 68、Class 28、VarHandle 31…
node natives.mjs libsx/org.lwjgl_lwjgl-opengl_3.4.3   # 2,278 / 194 类
node natives.mjs libsx/org.lwjgl_lwjgl_3.4.3          # 2,768 / 35 类
javap -p -cp lwjgl-opengl-3.4.3.jar org.lwjgl.opengl.GL11C | grep -c native   # 62

# 4) LWJGL 后端
grep -rl "java/lang/foreign" lw/lwjgl343 | wc -l   # 98（全在 META-INF/versions/25/org/lwjgl/system/ffm）
grep -rl "java/lang/foreign" lw/gl343   | wc -l   # 0
grep -rl "java/lang/foreign" lw/lwjgl333| wc -l   # 0（3.3.3 纯 JNI）

# 5) NeoForge
curl -x http://127.0.0.1:7897 https://maven.neoforged.net/releases/net/neoforged/neoforge/26.3.0.104/...-installer.jar
node -e "…require('./nfi/version.json')"   # mainClass=net.neoforged.fml.startup.Client，--add-opens java.base/java.lang.invoke
grep -rl "defineHiddenClass" nfx            # nfx/bus/net/neoforged/bus/EventListenerFactory.class
grep -rl "defineClass" nfx/loader           # nfx/loader/net/neoforged/fml/classloading/ModuleClassLoader.class
```

## 7. 没查到 / 没验证的（不要当成结论）

1. **MC 26.3 是否真的启用 LWJGL 的 FFM 后端**：找不到开关名。已知证据只有「MC 0 处 foreign 引用」+
   「opengl 模块 0 处 foreign 引用」+「GL11C 仍是 native」——足以判断 GL 走 JNI，但 FFM core 后端是否被 core 内部启用未证实。
2. **首启动真实触及的类集合**：没有真跑，P0 的「必命中」是**静态统计**而不是运行期 profiling。
3. **NeoForge 是否隐式起 `-javaagent`**：`version.json` 里没有 agent 参数，但 mixin/loader 里有 8 个类引用 `java.lang.instrument`，未实测。
4. **现有 GC/堆能不能扛 MC**：未测（无 `-Xmx` 语义、无大对象/世代统计）。
5. **1.21.x 与 26.3 之间的中间版本是否更省**：只抽了两端；26.x 换了 SDL/shaderc/spvc/vma，native 面**变大**了，
   低版本理论上更省，但没逐版本统计。
6. **assets（34 objects / 487 MB）** 与 JVM 无关，本文未展开。
