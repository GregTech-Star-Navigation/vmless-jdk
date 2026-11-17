# java.base native 方法清单

> 状态：**M1–M4 + `ldc Class` 全通** · 2026-11-17 · rustjavac 0.43.0
> 验收：`../verify.sh /d/vmless/bnat1` → 三路一致 103 / **失败 0**；`corpus` 三路一致 **35/45**（与基线同）；四个里程碑输出与 HotSpot 逐字节一致。
> 数据来源：`src/vm/native_reg.rs` 的注册表（102 条）+ `examples/run_jdk.rs` 的运行时累积清单。

## 一句话

`java.*` 不再一律靠 `natives.rs` 写死：只要 classpath 上有 java.base（`VMLESS_JDK`），
`Object.<init>`、`String.length`、`ArrayList.add`、`HashMap.put` 跑的都是 **javac 编出来的真字节码**；
真字节码里标了 `native` 的方法才落到这张注册表，缺了就报 `UnsatisfiedLinkError`（**绝不静默回退**）。

## 机制

```rust
// src/vm/native_reg.rs
pub(crate) type NativeFn = fn(&mut Vm, &mut Frame) -> Result<Slot, VmError>;
// key = (类内部名, 方法名, 描述符)
```

| 位置 | 是什么 | 谁用 |
| --- | --- | --- |
| `jdk.rs::VIRTUALS/STATICS` | **内建替身**（整类顶替 java.*） | 编译器取签名、JIT 的 op 号、**无 java.base 时的解释器** |
| `natives.rs::builtin_*` | 上面那张表的实现（按 op 号分发） | 同上 |
| `native_reg.rs`（本清单） | **真 native 方法**的实现（按 `(类,名,描述符)` 精确匹配） | `exec` 遇到 `ACC_NATIVE` 时 |

判据：**这个方法在 OpenJDK 源码里带 `native` 关键字吗**？带 → 进注册表；不带 → 它有字节码，
必须由解释器执行，不许在 Rust 里再写一份。

分派顺序（`exec.rs`）：

1. `VMLESS_PREFER_BUILTIN=1` → 先问 `builtin_*`，**给出值就用**（给不出继续往下）
2. `resolve_method` 命中 → **执行真字节码**
3. 类没字节码 → `builtin_*`（独立模式兜底）
4. 都没有 → 明确报错

`ACC_NATIVE` 的方法在第 2 步里由 `vm/exec.rs:69` 拦下来查注册表；
未注册时错误形如 `UnsatisfiedLinkError: java/lang/Object.hashCode()I not registered`，
并累积进 `Vm::missing_natives()`（`examples/run_jdk.rs` 结束时打印）。

## 已注册：102 条

### java.lang.Object（7）
| 方法 | 描述符 |
| --- | --- |
| `getClass` | `()Ljava/lang/Class;` |
| `hashCode` | `()I` |
| `clone` | `()Ljava/lang/Object;` |
| `notify` / `notifyAll` | `()V` |
| `wait` | `(J)V` / `(JI)V` |

### java.lang.Class（8）
`registerNatives()V`（`<clinit>` 第一句，不接住连类都初始化不了）、
`initClassName()Ljava/lang/String;`、`getPrimitiveClass(Ljava/lang/String;)Ljava/lang/Class;`、
`isInstance(Ljava/lang/Object;)Z`、`isAssignableFrom(Ljava/lang/Class;)Z`、
`getSuperclass()Ljava/lang/Class;`、`isHidden()Z`、`desiredAssertionStatus0(Ljava/lang/Class;)Z`

### java.lang.System（8）
`registerNatives()V`、`setIn0/setOut0/setErr0`、`currentTimeMillis()J`、`nanoTime()J`、
`arraycopy(Ljava/lang/Object;ILjava/lang/Object;II)V`、`identityHashCode(Ljava/lang/Object;)I`

### java.lang.String（1）/ Throwable（2）/ Double（2）/ Float（2）
`String.intern()Ljava/lang/String;`
`Throwable.fillInStackTrace(I)Ljava/lang/Throwable;`、`getStackTraceDepth()I`
`Double.doubleToRawLongBits(D)J`、`longBitsToDouble(J)D`
`Float.floatToRawIntBits(F)I`、`intBitsToFloat(I)F`

### jdk.internal.misc.VM（2）/ CDS（6）
`VM.initialize()V` —— **必须真建一个空 Map 给 `savedProps`**，否则 `VM.getSavedProperty`
抛 `IllegalStateException("Not yet initialized")`，`Integer.IntegerCache.<clinit>` 直接挂掉，
`Integer.valueOf(1)` 一调用就炸。
`VM.getNanoTimeAdjustment(J)J`
`CDS.getCDSConfigStatus()I`、`isDumpingClassList0/isDumpingArchive0/isSharingEnabled0()Z`、
`getCorchestra()Ljava/lang/Object;`、`initializeFromArchive(Ljava/lang/Class;)V`

### jdk.internal.misc.Unsafe（64）
用得最多的一个类，分四组：

* **布局**（8）：`objectFieldOffset0(Ljava/lang/reflect/Field;)J`、`staticFieldOffset0`、`staticFieldBase0`、
  `shouldBeInitialized0`、`ensureClassInitialized0`、`arrayBaseOffset0(Ljava/lang/Class;)I`、
  `arrayIndexScale0(Ljava/lang/Class;)I`、`addressSize()I`
* **读写**（17 + 17 volatile/ordered）：`getBoolean/getByte/getChar/getShort/getInt/getLong/getFloat/getDouble/getReference`
  与对应的 `put*`；volatile 变体与 `putIntRelease`/`putReferenceRelease`/`putIntOrdered` 同普通读写
* **CAS**（6）：`compareAndSetInt/Long/Reference`、`compareAndExchangeInt`、`getAndAddInt/Long`
* **杂项**（9）：`registerNatives`、`allocateInstance`、`throwException`、`park`、`unpark`、
  `fullFence/acquireFence/releaseFence/loadFence/storeFence`

## 里程碑

每个里程碑的「最小集」= 该例子真正调到的 native（其余是传递依赖带进来的）。

### M1 `new java.lang.Object()` — 通
例子：`D:/vmless/jdkms/M1.java`（javac 25 编译），输出与 `java` 一致。

| 用到 | 说明 |
| --- | --- |
| `Object.<init>()V` | **有字节码**，就是一条 `return` |
| `Object.hashCode()I` | native → `identity_hash` |
| `Object.getClass()Ljava/lang/Class;` | native → 类对象（同一类永远同一句柄） |
| `Class.initClassName()` | native；`Class.name` 存**二进制名**（点号） |
| `Class.registerNatives()V` | `Class.<clinit>` 第一句 |
| `Class.desiredAssertionStatus0` | `assert` 的 `$assertionsDisabled`（= false，与不带 `-ea` 一致） |
| `System.arraycopy` / `Unsafe.*` | `Object.toString()` → `StringBuilder` → 传递依赖 |

`Object.toString()` 走真 `StringBuilder`/`AbstractStringBuilder` 字节码，输出
`java.lang.Object@7febcafb`（HotSpot 同形）。

### M2 `java.lang.String` — 通
例子：`M2.java`。`<init>(char[])`、`length()`、`charAt(I)`、`hashCode()`、`equals()`、`substring()`、
`isEmpty()` **全是真字节码**；新出现的 native 只有：

| 用到 | 说明 |
| --- | --- |
| `String.intern()` | 返回 this（我们本来就只有一份） |
| `Unsafe.arrayBaseOffset0/arrayIndexScale0` | `StringUTF16.compress` 之类靠 `Unsafe.ARRAY_*` 常量 |
| `Unsafe.get*`/`put*` 全部宽度变体 | `StringUTF16`/`StringLatin1` 的字符访问 |
| `ArraysSupport.vectorizedMismatch` 链上的 `getLongUnaligned` | 它**不是** native（Java 组合 getLong/getInt/getShort/getByte），所以只是上面那批 native 的调用者 |

关键实现：`String` 的 `value:[B` / `coder:B` / `hash:I` / `hashIsZero:Z` 在 `ldc` 出来的字符串常量上
也要能看见（`vm/heap.rs::str_field_slots` 的「字符串字段视图」），否则 `"abc".length()`
一执行就取到 null。

### M3 `java.util.ArrayList` — 通
例子：`M3.java`（`add`/`get`/`size`/`contains`/`indexOf`/`remove`，扩到 12 个元素触发扩容）。
新出现的 native：**0 条** —— 全靠 M1/M2 已有的那批 + `System.arraycopy`。

`Object[]` 扩容走 `Arrays.copyOf`，其中 `Object[].class.equals(newType)` 为真
（我们的类对象是同一句柄）→ 走 `new Object[n]` 快路径，**没有走到 `Array.newInstance`**。

### M4 `java.util.HashMap` — 通
例子：`M4.java`（`put`/`get`/`size`/`containsKey`/覆盖，触发 `resize()` 到 16 槽）。
新出现的 native：**0 条**，但踩出两个必补的传递依赖：

| 用到 | 说明 |
| --- | --- |
| `VM.initialize()` | 必须建 `savedProps`（空 Map），否则 `Integer.IntegerCache` 初始化失败 |
| `CDS.getCDSConfigStatus()` / `CDS.initializeFromArchive` | `String`/`Integer` 的 `<clinit>` 会问「有没有 CDS 归档」 |
| `Throwable.fillInStackTrace` | 异常路径（`IllegalStateException` 的构造器） |

## `ldc Class`（`X.class`）— 通
例子：`M5.java`，12 行输出与 `java` 逐字节一致。

`ldc` 的 `CONSTANT_Class` 项不再报「不支持的类型」：压入一个**类对象**。
类对象是**真对象**（运行时类 `java/lang/Class`，不是另一种句柄），因为 `java.lang.Class`
自己的字节码（`getName`/`getSuperclass`/`isInstance`）要能对它 `getfield`/`putfield`。

要点：

* **同一个类永远同一个句柄**（`class_obj_cache`）—— `X.class == X.class`、`o.getClass() == X.class` 靠它
* `Class.name` 存**二进制名**：基本类型是 Java 写法（`I` → `int`），数组是 `[I` / `[Ljava.lang.String;`
* 覆盖到的写法：`String.class` / `int.class` / `int[].class` / `String[].class` / `本类.class` /
  `getClass() == X.class` / `instanceof` / `Class.isInstance` / 类对象之间的 `==`

## 还没实现（下一步会撞上）

按「先撞到再补」的原则，下面这些**目前没注册**，用到时会报 `UnsatisfiedLinkError`：

| 类 | 方法 | 什么时候需要 |
| --- | --- | --- |
| `java.lang.reflect.Array` | `newArray(Class,I)Object`、`multiNewArray(Class,[I)Object`、`get*/set*`（19 个） | `Arrays.copyOf` 的非 `Object[]` 分支、反射建数组 |
| `jdk.internal.misc.Unsafe` | `copyMemory0`、`setMemory0`、`allocateMemory0`、`reallocateMemory0`、`freeMemory0` | 直接内存、`Arrays.fill` 的向量化分支 |
| `jdk.internal.misc.Unsafe` | `getAndSetInt/Long`、`getAndBitwiseOr*`、`compareAndExchangeLong/Reference` | 并发容器、`AtomicXxx` 的完整 API |
| `java.lang.Class` | `getDeclaredFields0`、`getDeclaredMethods0`、`getConstantPool` | 反射、`Unsafe.objectFieldOffset` 的真实字段偏移 |
| `java.lang.Thread` | `currentThread`、`start0`、`sleep`、`interrupt0` | 任何 `synchronized`/`Thread` 代码 |
| `java.io.FileOutputStream` / `FileDescriptor` | `writeBytes`、`initIDs` | 真文件 I/O（现在是 `println` 直通 stdout） |
| `java.lang.StringCoding` / `sun.nio.cs.*` | `hasNegatives`、`implEncodeAsBytes` 等 | 非 ASCII / UTF-8 编解码、`String.getBytes(Charset)` |
| `java.lang.invoke.MethodHandle` 一族 | `invokeBasic`、`linkToStatic`… | `invokedynamic`（字符串拼接 `+`、lambda、switch on string） |
| `jdk.internal.access.SharedSecrets` | 需要 `JavaLangAccess` 的实现对象 | **非 ASCII 字符串**（见下） |
| 解释器 | `invokedynamic`（opcode `0xba`）**根本没实现** | javac 编出来的 `"a" + b`、lambda、字符串 switch 都会在第一处 `0xba` 报 `未实现的字节码 0xba @pc=N`（`exec.rs` 末尾的 `other =>` 兜底） |
| `ldc` | `CONSTANT_Dynamic`（condy）/ `MethodHandle` / `MethodType` | 现代 java.base 里 condy 用得不少（`String` 的惰性常量等） |

### 已知不通：非 ASCII 字符串（UTF16 路径）

`new String(new char[]{'中','文','A'})` 报：

```
执行失败: jdk/internal/access/JavaLangAccess.uncheckedGetUTF16Char([BI)C: 常量池里没有 Code
```

原因链：非 latin1 的字符让 `StringUTF16.compress` 返回 null → 走 UTF16 分支 →
`StringUTF16` 的字符访问经 `SharedSecrets.getJavaLangAccess()` 转发，而
`JavaLangAccess` 是个**抽象接口**，它的实现对象由 JDK 启动流程（`System.initPhase1`
/ `SharedSecrets.setJavaLangAccess`）注册，我们没有那段启动流程，于是解析到接口上的
抽象方法（无 `Code` 属性）就报错。

M1–M5 全部只用 latin1（`coder == 0`）路径，不受影响。

## 踩过的坑（都是实测，别再踩）

1. **native 是实例方法，参数槽位不能手算。**
   `Unsafe.getByte(Object o, long offset)` 是**实例**方法：`this`(Unsafe) 在槽 0、`o` 在槽 1、`offset`
   在槽 2——不是 0/1。手写下标让 `offset` 读成了 `o`，读到的全是 0。
   现在统一走 `Frame::arg(i)`（`vm/frame.rs`），它按 `param_slots(desc) + arg_base` 算。

2. **`Unsafe.ARRAY_*_INDEX_SCALE` 必须返回真值（1/2/4/8），不能一律 1。**
   `ArraysSupport.vectorizedMismatch` 用 `log2(ARRAY_LONG_INDEX_SCALE) - log2(ARRAY_xxx_INDEX_SCALE)`
   当移位量；全返回 1 会让它按 1 字节步长比较 8 字节窗口，
   `String.startsWith` 在「前缀确实相同」时**误报不同**（`"abcdefghijklmnopqrstuvwxyz".startsWith("abcdefghij")` → false）。
   同时 `Unsafe.getLong` 对 `[B` 必须**打包 8 个字节**（`read_mem`），否则机器字比较永远相等，
   反过来「不同」也看不出来（`startsWith("abcdefghijX")` → true）。

3. **`ldc "abc"` 的字符串句柄没有字段槽。**
   真 `String.length()` 是 `value.length >> coder`，走 `getfield`。字符串堆句柄必须能被当成
   `java.lang.String` 实例看（`vm/heap.rs::str_field_slots` 懒材料化出 `value/coder/hash/hashIsZero`），
   否则一碰字符串常量就断。反向也要通：`new` 出来的真 String 对象，`println` 要能打印出文本
   （`natives.rs::real_string_text`）。

4. **`VM.initialize()` 不能是空实现。** 见上文 —— 它不建 `savedProps`，`Integer.valueOf` 就炸。

## 怎么复现

```bash
export PATH="$PATH:/c/Users/Administrator/.cargo/bin"
export CARGO_TARGET_DIR='D:\vmless\rt_nat1'
cd /d/vmless/rustjavac && cargo build --release && cargo build --release --examples
# 例子源码用真 javac 编（我们的编译器还不支持 invokedynamic）
export PATH="/c/Program Files/Eclipse Adoptium/jdk-25.0.4.101-hotspot/bin:$PATH"
cd /d/vmless/jdkms && javac -d out M1.java M2.java M3.java M4.java
cd /d/vmless && ./bnat1/run_jdk.exe 'D:/vmless/jdkms/out' M4      # 缺省用本机 JDK 的 lib/modules
./bnat1/run_jdk.exe 'D:/vmless/jdkms/out' M2                       # 打印「缺 native: N 条」
VMLESS_JDK='C:/Program Files/Eclipse Adoptium/jdk-25.0.4.101-hotspot/lib/modules' \
  ./bnat1/rustjava.exe -cp 'D:/vmless/jdkms/out' M3                # 同一能力从 CLI 走
```

两种模式：

| 环境变量 | 行为 |
| --- | --- |
| （不设） | **独立模式**：只有用户自己的类，`java.*` 由 `jdk.rs`/`natives.rs` 的内建替身实现（verify/corpus 的基线靠它） |
| `VMLESS_JDK=<lib/modules 或解包目录>` | **真字节码模式**：`java.*` 全部真执行，缺 native 报 `UnsatisfiedLinkError` |
| `VMLESS_PREFER_BUILTIN=1` | 调试用：先试内建替身，给不出值再走真字节码。**它认不出真字节码 `new` 出来的 String 对象**（只认字符串堆句柄），所以结果会与真实语义不符 —— 这是排查工具，不是运行模式 |
