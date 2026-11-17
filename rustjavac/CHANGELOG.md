# 变更记录

格式：`## <版本>` + 「为什么 / 改了什么」。修 bug → patch，加能力或改默认行为 → minor，
不兼容的配置/接口变更 → major。纯文档/测试/重构不动版本号。

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
