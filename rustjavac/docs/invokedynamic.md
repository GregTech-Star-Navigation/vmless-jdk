# invokedynamic / CONSTANT_Dynamic / java.lang.invoke

> 状态：已完成 · 2026-11-17 · 解释器侧完整（JIT 侧明确不支持，见 §6）
> 代码：`src/vm/indy.rs`（新）、`src/classfile/attr.rs`（BootstrapMethods）、
> `src/vm/exec.rs`（`0xba` / `ldc` / invoke* 拦截）、`src/vm/class.rs`（继承链与 instance 判定）
> 验收：`jdkms/indy/cmp.sh` 四路一致 11/11 · `verify.sh` 103 / 失败 0 · `cargo test --release` 50/50

## 1. 为什么必须做

javac 从 9 起把三件日常事编译成 `invokedynamic`：

| 源码 | 引导方法 |
| --- | --- |
| `"a" + b` | `StringConcatFactory.makeConcatWithConstants` |
| `() -> x` / `Foo::bar` | `LambdaMetafactory.metafactory` / `altMetafactory` |
| `switch` 里的类型/枚举标签 | `SwitchBootstraps.typeSwitch` / `enumSwitch`（**未实现**，见 §5） |
| record 的 `equals/hashCode/toString` | `java/lang/runtime/ObjectMethods.bootstrap`（**未实现**，见 §5） |

没有这一层，任何现代 `.class` 一碰字符串拼接就死 —— 这是「跑真实程序」的硬门槛。

## 2. 三层结构（以及为什么这么分）

### 2.1 引导方法解析（`run_bootstrap`）

`invokedynamic` 的操作数是常量池里的 `CONSTANT_InvokeDynamic`：
`bootstrap_method_attr_index` + `name_and_type`。引导方法表在类的 `BootstrapMethods`
属性里（`classfile::attr::bootstrap_methods`）。首次执行时：

```
静态参数（常量池逐项重建：数值/String/Class/MethodType/MethodHandle/Dynamic）
  → 调引导方法
  → 结果必须是 CallSite
  → 取 getTarget() 得到目标 MethodHandle
  → 按 (类下标, 常量池索引) 缓存
```

**只解析一次**：`-Dvmless.indy.stats=1` 会打出「解析 N 次 / 执行 M 次 / 命中 M-N 次」
（I6：解析 2 次、执行 100001 次、命中 99999 次）。

### 2.2 快路径 vs 通用路径（关键设计决定）

`StringConcatFactory` / `LambdaMetafactory` / `ConstantBootstraps` 的 **Java 实现本身**
就在用 `MethodHandles.Lookup.defineHiddenClass`、`invokeBasic` 这类 VM 内部原语
（OpenJDK 里它们是 `@IntrinsicCandidate`/native）。任何 VM 都不可能靠执行它们的字节码
跑通 —— HotSpot 自己也不是那么做的（它有 `linkTo*` 系列 VM 原语）。

所以这三个工厂在 Rust 里**直接算出等价的 CallSite**（规范没规定必须执行它们的 Java 代码，
结果一致即可）。**其它任何引导方法**（用户自己写的）走通用路径：真的用解释器执行那个
Java 方法，取它返回的 `CallSite.getTarget()`。

### 2.3 `java.lang.invoke` 的字节码级调用（`invoke_api`）

`MethodHandles.lookup().findStatic(...)`、`mh.invokeExact(x)`、`vh.get()` 这些在字节码里
就是普通的 invoke* 指令，但它们的目标方法要么是 `native`（`invokeExact`/`invokeBasic`），
要么整份实现都建立在 VM 原语上。所以 `exec` 在**方法解析之前**按类名前缀
（`java/lang/invoke/`）把它们交给 `indy::invoke_api`。

代价与边界：
- 这些类**不执行字节码，也不跑 `<clinit>`**（`MethodType.<clinit>` 就会去碰 `MemberName`）。
- 不认识的成员**报明确的错**，不静默放过：`VM 未实现的 MethodHandles.zero(...)`。

## 3. 两处最容易写错的地方（都踩过）

### 3.1 静态参数 ≠ 引导方法的形参

引导方法调用时 JVM 先塞三个**形参**：`Lookup`、`name`（调用点名）、`MethodType`（调用点描述符），
然后才是**静态参数**（常量池里那串）。所以：

- `LambdaMetafactory.metafactory` 的静态参数只有 3 个：`samMethodType` / `implMethod` /
  `instantiatedMethodType`（第 4 个才是 flags，altMetafactory）；
- `ConstantBootstraps.invoke` 的静态参数第 1 个就是 `MethodHandle`；
- `makeConcatWithConstants` 的静态参数第 1 个就是拼接配方。

写错的表现是「第 4 个静态参数应该是 String 配方，实际是 None」——第一版就是这么错的。

### 3.2 直接方法句柄的**类型**不等于成员描述符

- `REF_invokeVirtual/Interface/Special` 的句柄**把接收者算作第一个参数**：
  `String::length` 的句柄类型是 `(Ljava/lang/String;)I`（不是 `()I`）；
- `REF_newInvokeSpecial` 的返回类型是**类本身**，不是构造器描述符的 `V`；
- `REF_getField/putField` 的 CP 描述符是**字段类型**，不是方法描述符。

统一在 `indy::handle_type(ref_kind, class, desc)` 算。写错的表现是
`WrongMethodTypeException: 句柄是 ()I，调用点是 (I)I`。

## 4. 适配（asType）与错误

`invoke_mh_as(mh, want_desc, args)` = 按**调用点描述符**把实参逐个 `coerce` 到句柄的参数
类型 → 分派 → 再把返回值 `coerce` 回来。覆盖：基本类型宽化/窄化、装箱、拆箱、
引用之间直接放行（VM 不做强转）。

- `invokeExact` 要求类型**完全一致**，不一致抛 `WrongMethodTypeException`（规范行为）；
- `invoke` 允许适配；`asType` **只许宽化/装箱/拆箱/引用转换**，窄化当场抛 `WrongMethodTypeException`；
- `Lookup.find*` 找不到时抛 `NoSuchMethodException`（引导方法里抛出时会按规范包成
  `BootstrapMethodError`）；`ConstantCallSite.setTarget` 抛 `UnsupportedOperationException`；
- 引导方法失败、返回的不是 CallSite、描述符对不上 → `BootstrapMethodError`，
  消息里带**引导方法类名/方法名/描述符**与调用点（`类.方法名`）。

错误路径有专门的样例 `I11`（invokeExact 类型不符 / asType 窄化 / 找不到方法 /
ConstantCallSite.setTarget），输出与 HotSpot 一致。

## 5. 没做到的（确切边界）

| 项 | 状态 | 确切原因 |
| --- | --- | --- |
| `SwitchBootstraps.typeSwitch` / `enumSwitch` | 未实现 | javac 25 对 `switch(t){ case null -> ... }` 会发 `typeSwitch`（探针：`switch(t){case null -> …}` 的 javac 25 产物里 BootstrapMethods 指向它）；它的 label 协议（`Class` / 常量 / 可续排的 `(Object,int)` 目标）要按 JDK 版本对齐，本轮未做 → 报 `BootstrapMethodError`，消息里给出引导方法全名 |
| `ObjectMethods.bootstrap`（record） | 未实现 | 它的 Java 实现要 `MethodHandles.zero` / `dropArguments` / `permuteArguments` / `filterArguments` / `filterReturnValue` 一整套组合子。实测报 `VM 未实现的 MethodHandles.zero(Ljava/lang/Class;)`。补它的正路是给 record 做原生快路径（toString/equals/hashCode 三形），或补齐组合子 |
| `MethodHandles.{zero,dropArguments,permuteArguments,filterArguments,filterReturnValue,collectArguments,exactInvoker,...}` | 未实现 | 组合子引擎，按需再加；目前报明确错误 |
| `Lookup.unreflect*` / `MethodHandleInfo` | 未实现 | 需要 `java.lang.reflect.Method/Constructor` 那层（VM 现在只有 `Field` 的雏形） |
| VarHandle 的 `getAndAdd` 之外的内存序变体 | 部分 | `get/set(±Volatile/Acquire/Release/Opaque/Plain)`、`compareAndSet/compareAndExchange/getAndSet/getAndAdd/getAndIncrement/getAndDecrement` 已实现；`accessModeType`/`isAccessModeSupported` 等未实现 |
| lambda 的隐式字段/序列化往返 | 未实现 | 我们只把「实现 Serializable」当成 `instanceof` 成立（`FLAG_SERIALIZABLE`），不做真正的序列化 |

## 6. 与 JIT 的关系

JIT **不支持** `0xba`，且**不假装支持**：

- `jit/refmap.rs` 与 `jit/translate.rs` 各有一条 `0xba` 分支，报错文本里带
  `invokedynamic` 字样 + 类/方法/描述符 + `pc`；
- `run_jit` 看到这个错误就**整体退回解释器**执行，并在 stderr 明说
  `JIT 跳过：… → 退回解释器执行`（stdout 与解释器逐字节一致，所以 `verify.sh` 的三路
  比对不受影响）。

要真正接进 JIT，得把调用点缓存做成机器码里的可替换跳板（HotSpot 的
`invokedynamic` → `MutableCallSite` 思路），这是独立的一件事。

## 7. 顺带补的两处（不属于 indy 层，但不补就跑不到验收）

- `String.valueOf(...)` 七个重载 + `Class.getName()` 的独立模式内建：
  **javac 21+ 会把拼接里的 Object 实参先在调用点外求 `String.valueOf(o)`**
  （见 I1 字节码），所以它不是可选件；
- `Runtime.availableProcessors()` native：`list.sort(...)` 的路径会问到 CPU 数。

## 8. 已知的、与本层无关的缺口（真实程序会先撞这些）

跑 `jdkms/indy3` 里那几个「真实写法」的样例时会先撞 java.base 的覆盖缺口，报错原文：

- `List.of(...)` → `UnsatisfiedLinkError: jdk/internal/misc/CDS.getRandomSeedForDumping()J`
- `text.split(...)` → `System.arraycopy 参数非法`（`array_copy` 对 `[C`/`[B` 的边界处理，早有）
- `String.join(...)` → `java.lang.IllegalArgumentException: Component type is not primitive`

这些都不是 indy：`indy` 侧的日志显示调用点已解析、拼接已执行。
