# 变更记录

格式：`## <版本>` + 「为什么 / 改了什么」。修 bug → patch，加能力或改默认行为 → minor，
不兼容的配置/接口变更 → major。纯文档/测试/重构不动版本号。

## 0.67.0

**为什么**：自研编译器对 OpenJDK `java.base` 全部源码（3381 个 `.java`）的通过率只有
**22.2%（750/3381）**。这是 vmless 的两条硬判据之一，一直没达标。失败原因不是「某个
复杂功能没做」，而是几处**通用机制**的缺口被放大：跨编译单元的符号来源只有本单元 +
classpath 上的 `.class`，而调用点根本没用上 classpath；同名重载只保留第一个描述符；
`System.err` 不在 `println` 特化里；嵌套类型一律按 `/` 拼；Windows 的文件系统大小写
不敏感被当成了「类存在」的证据。

**改了什么**（通过率 22.2% → 62.0%，`cargo test` 106 通过、`verify.sh` 103 一致 0 失败）

- **测量脚手架**：`examples/cover_dir.rs` 重做（错误骨架归一化、失败桶表、直接出
  `docs/java-base-失败分布.md`）+ 新增 `examples/probe_src.rs`（单文件诊断，报错带方法
  名）与 `examples/emit_src.rs`（带 classpath 真编译落盘）。codegen 的错误消息统一加上
  `（方法 C.m(desc)）` —— 没有行号时这是唯一能定位的线索。
- **符号表**：classpath 的元数据改为整进程共享 `Arc`（此前每个编译单元复制 7350 个
  `Own`，批量编译 99% 的时间花在搬这个）；classpath 上真有 `java/lang/Object` 时替换
  内建占位（占位少了 `clone`/`getClass`/`notifyAll`）；没写 `extends` 的类显式补
  `java/lang/Object`（否则本单元的类查不到 Object 的方法）。
- **同名的多个重载全部保留**（`instance_methods`/`static_methods` → `Vec<desc>`），
  调用点用 `pick_desc` 按实参个数 → 形状 → 精确描述符挑。只留第一个必然挑错，
  然后实参按错的类型转换，报出一堆假错误。
- **类名解析**：表达式位置补同包（`AESConstants.X`）与嵌套类型（`Opcode.Kind.X` →
  `Opcode$Kind`）；类型位置的多段名按 `$` 拼（`Map.Entry` 不再生成 `LMap/Entry;` 这种
  不存在的类名）；qualify 的「类存不存在」改成**大小写敏感**比较（Windows 上变量名
  `key` 会命中 `Key.class`，这是「未定义的变量」最大的一桶）。
- **Java 语法/语义补齐**：`import static`（方法与常量）、class 字面量（`X.class` /
  `int.class`）、`instanceof` 模式绑定（`if (x instanceof T t)`、`!(…)` 与守卫写法）、
  嵌套 record、`File[]::new`、`try (…)` 之外的 `(a & m) != 0`（cast 回溯）、
  `System.err.println`、数组 `clone()`、引用转基本类型的 cast、`arr[纯表达式] op= v`。

## 0.58.0

**为什么**：JIT 一直是「一遇不支持就整程序作废」——只要方法体里出现一条
`invokedynamic`（哪怕只是最后那句 `"sum=" + s` 的字符串拼接），整个 `main` 连同
里面 800 万次纯算术循环一起退回解释器。实测 P2（4×800 万次循环 + 一句拼接）：
解释器 6679ms，JIT 完全没机会出手；而同一个循环单独 JIT 只要 145ms。JIT 本身不慢，
慢在**覆盖率与回退粒度**。

**改了什么**

- `synchronized` 之外的两条「整程序作废」判据拆开：
  - **方法级回退**：`compile_into` 改成 wrapper + inner，编译失败的方法记进
    「解释执行集合」（带原因）并从函数缓存里摘掉，调用方不再跟着一起废；
    被调方**描述符无引用**时调用点改发解释器桩（`vmless_interp_call`），
    否则才向上传染。统计打一行 `JIT: 编译 N 个方法，解释 M 个（原因：… ×K）`。
  - **`invokedynamic` 接进来**：`jit/indy.rs` 在编译期分类调用点，
    `StringConcatFactory` 的 recipe 直接解码成一条「拼接计划」，
    运行期一次 `rt::indy_concat(plan, i64 值数组)` 拼完（`` 取栈上参数、
    `` 取引导方法常量）；其它引导方法（`LambdaMetafactory` 等）
    只让**这个方法**回退。`jit/refmap.rs` 的栈形状分析同步支持 0xba。
- 新增 `src/jit/pure.rs`：判定「纯数值方法」（签名无引用、静态、字节码不碰堆/字段/
  监视器、调用图传递闭包干净）。只有这些方法能跨解释器/JIT 边界调用。
- 新增 `src/vm/bridge.rs`：解释器 ↔ JIT 的方法级调用桥。解释器 `execute` 进门先查
  JIT 表（`OnceLock`，读路径无锁），命中就调机器码；JIT 侧的解释器桩走
  `call_interpreted_ptr`。
- **解释器由 `run_jit` 一次性预建**（`vm::open_interp`），并补上
  `set_main_tid + safepoint.register + thread_register`：懒建会让 `Vm::open`
  （95MB）落在生成代码的调用路径上，任何岔子都变成「每次调用重建一个解释器」，
  工作集按 90MB/s 涨到把机器吃光；不进线程登记则 `cur_tid()`=0，
  监视器/hand-off 空转，永不返回。
- 回归：`samples/thread/{SyncOne,SyncFour}.java` + `scripts/verify-sync.sh`
  （配 `scripts/memrun.ps1`），三个入口 × 两个样例，**同时**断言输出、峰值内存
  （默认 200MB）与墙钟（默认 30s）。

**实测**（`VMLESS_JDK=D:/vmless/patch/img`，同一份 `.class`）

| 程序 | 解释器 | JIT | HotSpot 25 |
| --- | --- | --- | --- |
| P1 4 线程 × 800 万次 | 1782ms | 185ms | 45ms |
| P2 4 × 800 万次 + 字符串拼接 | 6679ms | 393ms | 71ms |

`Perf2.sum(50M)` 145.9ms、`fib(30)` 12.1ms —— 纯数值路径与 0.57 持平（141ms）；
`verify.sh` 三路一致 103/0。

## 0.55.0

**为什么**：解释器一直有 GIL（一把大锁 + `AtomicPtr<Vm>`，CPython 式）：谁持锁谁独占
整个 VM，每 512 条字节码交还一次。实测 4 线程比串行还慢 2.2 倍
（P1 15583ms / P2 7024ms）—— 票据锁的 condvar 乒乓比干活还贵。多线程要真并行，
就得把「整个 VM 一把锁」换成「共享状态各有各的同步」。

**改了什么**

- 新增 `src/vm/shared.rs`：`Shared`（跨线程状态的根）+ `Heap` + `PtrTable` + `Cell`
  - 对象/数组 = `ObjHdr`（类名 `Arc<str>`、长度、thin lock 字、`Box<[Cell]>`），
    句柄仍是 `u32` 下标，但下标查到的是**永不被搬动的指针**（页表只追加，
    扩容不会让别的线程手里的引用失效 —— 这是旧 `Vec<Vec<Slot>>` 做不到的）；
  - 槽位值 = `AtomicU64` + `AtomicU8` 标签，字段/元素读写**无锁**；
  - 分配走页表 + 原子序号，无全局锁；字符串池 `RwLock<Vec<Arc<str>>>`。
- 新增 `src/vm/monitors.rs`：监视器改成**对象头 thin lock**
  （`0` = 未锁 / `tid<<8|count` = 持有者 / `1` = 已膨胀）。
  未竞争的 `synchronized` 只付一次 CAS；真竞争时单向膨胀成 `WaitSet`
  （entry / cond 两个 FIFO 队列 + hand-off 移交）。语义与旧实现一致：
  可重入、FIFO 移交、下溢抛 `IllegalMonitorStateException`。
- 新增 `src/vm/gc.rs`：停靠点（轮次 + 每线程到达计数 + park/unpark）与卡表写屏障接线。
  **当前没有 GC 去发起它**（原因见文件头：`Frame.locals` 没有引用位图，
  追溯扫描无法区分句柄和整数），所以稳态只花一条 relaxed 原子读。
- `Vm` 拆成三层：`Arc<Shared>`（堆/监视器/类表快照/静态字段…）+ 每线程一个 `Vm`
  （深度/计数器/持有的锁）+ 栈上 `Frame`。`Vm::fork()` 是 `Thread.start` 与
  死锁看门狗的入口。
- **删掉 GIL**：`gil_lock` / `gil_unlock` / `vm_ref` / `VM_PTR` 全部移除；
  阻塞点就是普通 park，没有「交还/重抢」这一步。`safepoint` 从「换线程」变成
  「查一眼 GC 要不要停世界」。
- 类表改成 `Arc<ClassStore>` 快照（COW）：加类时 `Arc::make_mut` 换一份并推进版本号，
  读路径不拿锁；`load_closure` 走 `store_mut` + `publish_store`。
  顺带把 `Rc` 全部换成 `Arc`（类表、indy 的 `BsmTable`/`MhKind::Concat` 要跨线程）。
- 静态字段 / 类对象表 / indy 状态 / 注解代理 / classpath 用分组 `Mutex`：
  它们都不在字节码热路径上，逐个做无锁结构的收益抵不过出错风险。
- 修一个跟着暴露出来的老 bug：字符串常量的字段视图（`value`/`coder`）之前存的是
  全零向量，导致 `"hello".hashCode()` = 0、`String.equals` 与真 `String` 对象永不相等。

**实测**（36 核机器，P1 = 4 线程各 800 万次、P2 = 串行 4 遍，各取 5 次中位）

| | 串行 | 4 线程 | 加速 |
| --- | --- | --- | --- |
| 旧（GIL） | 7024ms | 15583ms | **0.45×（劣化）** |
| 新 | 7015ms | 1842ms | **3.81×** |
| HotSpot | 71ms | 44ms | 1.61× |

8 线程版（各 400 万次，总工作量相同）：串行 7051ms / 8 线程 1055ms = **6.68×**。

**验收**：`verify.sh` 103/103；`cargo test --release` 56 通过；corpus 40/45
（另 3 个是编译器产出的字节码让 HotSpot 自己崩掉，基线同样失败）；
`jdkms/thread` 16/16（含 T1 计数 = 400000、T3 wait/notifyAll、T4 死锁检出）；
`run_jdk M2` 与 java 逐字节一致；2M 对象并行分配峰值 355MB、结果正确。

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
