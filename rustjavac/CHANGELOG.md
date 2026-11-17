# 变更记录

格式：`## <版本>` + 「为什么 / 改了什么」。修 bug → patch，加能力或改默认行为 → minor，
不兼容的配置/接口变更 → major。纯文档/测试/重构不动版本号。

## 0.54.0

**为什么**：解释器能跑并发 Java 了，但**反射一行都没有** —— `Class.forName`、
`Field`/`Method`、运行期注解、泛型签名全是空的。这是「跑 Minecraft」四块硬骨头之一
（FML 的 `@Mod` 扫描、Jackson 的属性绑定都压在它上面）。

**改了什么**

- 新增 `src/vm/reflect.rs`：反射层（语义全在 VM 里，java.lang.reflect 只当载体）
  - **native**（OpenJDK 里本来就是 native）：`Class.forName0` / `getDeclaredFields0` /
    `getDeclaredMethods0` / `getDeclaredConstructors0` / `getInterfaces0` /
    `getDeclaringClass0` / `getSimpleBinaryName0` / `getGenericSignature0`；
  - **override**（是字节码但语义必须由 VM 给）：`getDeclaredFields/Methods/Constructors`
    一族与 `getField/getMethod/getConstructor`、`Field.get/set` 全 18 个、
    `Method.invoke`、`Constructor.newInstance`、`getAnnotation/getAnnotations/
    isAnnotationPresent`（`Class` 与 `AccessibleObject` 两侧）、`getParameterAnnotations`；
  - 反射对象用它们**自己的构造器**造出来，`clazz`/`slot`/`name`/`type`/`modifiers`/
    `signature` 全部填好 —— 于是 `getName`/`getType`/`getModifiers`/`equals`/`toString`
    这些纯字节码方法一行都不用写；
  - 运行期类加载：`Class.forName` 走 `ClassStore::load_closure`（与启动同一套闭包规则），
    找不到才报 `ClassNotFoundException`；`Class.forName(null)` 是 NPE。
- 新增 `src/classfile/annot.rs`：解 `RuntimeVisibleAnnotations`（含嵌套、数组、枚举、
  Class 字面量）/ `RuntimeVisibleParameterAnnotations` / `AnnotationDefault` / `Signature`。
- 注解实例 = 「运行时类 = 注解接口 + 成员值在 `Vm::ann_objs`」的代理对象，成员方法在
  `exec` 的 `invokeinterface` 里解析之前分流（与 lambda 同一层）；成员顺序按
  `AnnotationParser` 的 `LinkedHashMap(memberDefaults)` 语义合并，`toString` 逐字符对齐
  `AnnotationInvocationHandler`（转义、`(byte)0x1f`、`1.0f`、`{"a", "b"}` 等）。
- `src/vm/exec.rs`：`is_override_owner` 加上反射类与 `StringJoiner`；调用点写
  `Vm::last_caller`（访问检查要知道调用者）；`invokeinterface` 解析失败的报错补上
  「调用点」与「接收者类型」。
- `src/vm/heap.rs`：类对象上填 `modifiers`（含 `InnerClasses` 合并、摘掉 `ACC_SUPER`、
  数组/基本类型的口径）、`primitive`、`componentType`；`class_obj` 收归一化后的键。
- `src/vm/class.rs`：`type_assignable` —— 数组的 `isAssignableFrom`/`instanceof`
  （`String[] → Object[]`、任何数组 → `Object`/`Cloneable`/`Serializable`、
  基本类型元素必须完全相同）；字符串句柄按 `java.lang.String` 的真继承链判
  （`Collections.sort(List<String>)` 的 checkcast 靠它）。
- `src/vm/natives.rs`：`System.arraycopy` 按规范放宽 —— 不再要求两边类型相同，
  按「元素可赋」判并逐元素抛 `ArrayStoreException`（以前 `Object[] ← TypeVariable[]`
  被一律拒绝，泛型签名解析直接卡死）；`throw_msg` 造带消息的异常对象。
- `java.util.StringJoiner.toString()` 由 VM 接管（真字节码是 `JLA.join`，而那个钩子由
  `System.initPhase1()` 挂上 —— 本 VM 不走 initPhase1）。
- `run_main`：VM 内部错误时先把已缓冲的输出刷出去再报错（否则「前面跑得好好的」全丢）。
- 收尾补 `ClassLoader.registerNatives`、`Unsafe.objectFieldOffset1(Class,String)`。

**验收**：`D:/vmless/jdkms/refl/cmp.sh` **R1–R8 8/8 与 HotSpot 逐字节一致**（反射入口、
`Field` 读写、`Method.invoke` 的 `InvocationTargetException`、`Constructor.newInstance`、
自定义注解与默认值、参数注解、泛型签名、迷你 DI 容器）；`./verify.sh` 103/0；
`cargo test` 56/56；corpus 40/45（与基线相同）。
需求边界 `docs/requirements/REQ-003-反射与运行期注解.md`（含「注解 toString 顺序」等已知差距）。

## 0.53.0

**为什么**：解释器能跑单线程 Java 了，但 `synchronized` 的两条字节码
（`monitorenter` 0xc2 / `monitorexit` 0xc3）一直是「未实现」，任何真并发程序
（含 Minecraft 的渲染 / 网络线程）一碰就断；`java.lang.Thread` 也完全不存在。
这是「跑 Minecraft」最后一块字节码缺口（解释器 195/202 → 197/202）。

**改了什么**

- 新增 `src/vm/thread.rs`：Java 线程 + 对象监视器 + **GIL**
  - Java 线程 = OS 线程（各自帧栈）；`Vm` 一把**票据锁**（FIFO）保证同一时刻只有一个线程
    访问堆 / 静态字段，阻塞点显式交还锁 —— 等价于「加锁粒度 = 整个 VM」，不需要拆 `Heap`、
    也不需要 TLAB；
  - 解释器每 512 条字节码一个**抢占点**，死循环线程不会饿死别的线程；
  - 监视器：可重入、FIFO 等锁、`monitorexit` 归零时**移交**队首（防插队）、
    计数下溢 → `IllegalMonitorStateException`；
  - `wait` / `notify` / `notifyAll` 的条件队列（不持锁调用 → 异常；支持超时）；
  - `sleep` / `join` / `interrupt` / `isInterrupted` / `getState` / `holdsLock` /
    `Unsafe.park` / `unpark`（`LockSupport` 的底座，AQS 靠它）；
  - 主线程退出等非 daemon 线程；`System.exit` 刷输出后立刻退出；
  - 线程表 + 死锁环检测 + `jstack -l` 风格的死锁报告（`VMLESS_NO_DEADLOCK_REPORT=1` 可关）。
- `src/vm/exec.rs`：`0xc2`/`0xc3` 指令；`ACC_SYNCHRONIZED` 方法在 `execute` 同一层成对加解锁
  （异常路径也解锁）；同步异常先过**本方法**的异常表（javac 把 `monitorexit` 放在 handler 里）；
  safepoint 计数。
- `src/vm/native_reg.rs`：`Object.wait0`/`notify`/`notifyAll`、`Thread` 的 native 一整套、
  **VM 接管表**（`Thread.<init>` ×9 与 `Thread.start()`）、`Shutdown.halt0`、
  `Unsafe.get/putXVolatile` 加 `fence(SeqCst)`。
- `src/vm/class.rs`：**`<clinit>` 按 JVMS 5.5 串行化**（一个类同时只许一个线程初始化，
  其余线程阻塞等）—— 多线程下不做这件事会读到半初始化的静态字段
  （`TimeUnit.MILLISECONDS` 变 null → `Thread.sleep` 里操作数栈下溢）。
- `src/vm/indy.rs`：VM 自己抛的异常也写 `Throwable.detailMessage`，`getMessage()` 才有值。
- JIT：`synchronized` 方法与 `monitorenter`/`monitorexit` **明确报错**，`run_jit` 整体退回解释器
  （不允许静默不加锁）。

**验收**：`D:/vmless/jdkms/thread/cmp.sh` 8/8 与 HotSpot 一致（死锁用 `jstack -l` 对照）；
`./verify.sh` 103/0；`cargo test` 50/50；corpus 40/45（与基线相同）。
设计说明 `docs/线程与监视器.md`，需求边界 `docs/requirements/REQ-002-线程与对象监视器.md`。

## 0.44.0

**为什么**：javac 从 Java 9 起把字符串拼接、lambda、方法引用都编译成 `invokedynamic`
（0xba）。解释器一碰 0xba 就报「未实现的字节码」，于是任何现代 `.class` 都跑不起来 ——
这是「跑真实程序」最后的字节码层缺口。

**改了什么**

- 新增 `src/vm/indy.rs`：`invokedynamic` / `CONSTANT_Dynamic` / `java.lang.invoke` 的完整实现
  - 引导方法解析 + **按调用点缓存**（`-Dvmless.indy.stats` 可验：I6 里 100001 次执行只解析 2 次）；
  - `StringConcatFactory.makeConcatWithConstants` / `makeConcat`（`\u0001`/`\u0002` 配方、
    常量折叠、`Double`/`Float` 与 Java 一致的文本化）；
  - `LambdaMetafactory.metafactory` / `altMetafactory`（捕获、方法引用、bridge、marker、
    `FLAG_SERIALIZABLE`）；
  - `ConstantBootstraps`（nullConstant / primitiveClass / invoke / fieldVarHandle /
    staticFieldVarHandle / getStaticFinal / explicitCast）；
  - `java.lang.invoke` 的字节码级接管（Lookup.find* / MethodHandle.invoke* / asType / bindTo /
    insertArguments / constant / identity / CallSite / MutableCallSite / VarHandle 的 get/set/
    compareAndSet 族），以及 `asType` 的拆装箱与基本类型转换；
  - 错误一律给 `BootstrapMethodError` / `WrongMethodTypeException`，消息里带引导方法
    类名/方法名/描述符与调用点。
- `src/classfile/attr.rs`：解析 `BootstrapMethods` 属性。
- `src/vm/exec.rs`：`0xba` 分派；`ldc` 支持 MethodHandle / MethodType / `CONSTANT_Dynamic`；
  invoke* 三处对 `java/lang/invoke/` 的接管（静态那处放在 `<clinit>` 之前）。
- `src/vm/class.rs`：`java.lang.invoke` 的继承链（独立模式无字节码时靠它）、合成 lambda 类的
  `instanceof` / `checkcast`。
- `src/vm/load.rs`：`resolve_method` 跳过抽象方法（接口方法调用不该解析到没有 Code 的声明）。
- JIT：`0xba` 明确报错（带类名/方法名/pc），`run_jit` 遇到它**整体退回解释器**并在 stderr 明说；
  刻意不假装支持。
- 独立模式补齐 `String.valueOf`（7 个重载）、`Class.getName()`、`Object.getClass()`、
  包装类 `TYPE` 字段 —— javac 21+ 会把拼接里的 Object 实参提到 `String.valueOf` 上。
- 新增 native `Runtime.availableProcessors`（`list.sort` 的路径会问）。
- 修 `run_class_file`：不带目录的 class 路径（`I1.class`）不再报「classpath 为空」。
- 修**虚分派的运行时类判定**：字符串句柄与数组也要给出运行时类（String / Object）。
  少这一条时，按接口声明的调用（`CharSequence.toString()`）会解析到 `Object.toString` 上，
  而 `String.contains` 的字节码正是 `indexOf(s.toString())` —— 于是它静默返回 `false`。
  同一处修在解释器与 `MethodHandle` 的虚分派里（两处必须一致）。

**验证**：`jdkms/indy/I1..I11` 四路（HotSpot / 真字节码 / 独立模式 / JIT 降级）逐字节一致 11/11；
`run_jdk` 缺 native 0；`verify.sh` 103 / 失败 0；`cargo test --release` 50/50。
