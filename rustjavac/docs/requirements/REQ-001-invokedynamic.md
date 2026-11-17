# REQ-001 · invokedynamic 与 java.lang.invoke

> 状态：已完成 · 2026-11-17
> 实现：`src/vm/indy.rs`（新）+ `src/classfile/attr.rs` + `src/vm/exec.rs` + `src/vm/class.rs`
> 设计说明：`docs/invokedynamic.md`

## 1. 要做什么

让解释器能执行**现代 javac 产出的字节码** —— 也就是 `invokedynamic`（0xba）与
`CONSTANT_Dynamic`（`ldc` 的 tag 17）。具体要能用：

1. `"a" + b`（`StringConcatFactory`，含 `\u0001`/`\u0002` 配方语义与常量折叠）；
2. lambda `() -> …`、带捕获的 lambda、方法引用 `Integer::parseInt` / `String::length`
   （`LambdaMetafactory.metafactory` / `altMetafactory`，含 marker / bridge / serializable）；
3. `ConstantBootstraps`（`nullConstant` / `primitiveClass` / `invoke` / `fieldVarHandle` /
   `staticFieldVarHandle` / `getStaticFinal` / `explicitCast`）；
4. `java.lang.invoke` 的直接使用：`MethodHandles.lookup()`、`findStatic/Virtual/Special/
   Constructor/Getter/Setter/VarHandle`、`insertArguments`、`constant`、`identity`、
   `asType`、`bindTo`、`invoke` / `invokeExact` / `invokeWithArguments`、`CallSite.getTarget` /
   `setTarget`、`MutableCallSite`、`VarHandle` 的 get/set/compareAndSet 族。

## 2. 不做什么

- **不做 JIT 侧支持**：`0xba` 在 JIT 里明确报错，`run_jit` 整体退回解释器（不假装支持）；
- 不做 `SwitchBootstraps.typeSwitch/enumSwitch`（`switch` 类型标签）—— 报明确的
  `BootstrapMethodError`；
- 不做 `ObjectMethods.bootstrap`（record 的 equals/hashCode/toString）—— 需要
  `MethodHandles.zero/dropArguments/permuteArguments/…` 一整套组合子，报明确错误；
- 不做隐藏类（`Lookup.defineHiddenClass`）、`MethodHandleInfo`、`unreflect*`、
  lambda 的真正序列化往返；
- 不改 java.base 的 native 覆盖（`CDS.*`、`Unsafe.objectFieldOffset1` 等），
  那是另一条工作线（`docs/java-base-native-清单.md`）；
- 不动 JIT / 编译器 / 其它子代理目录。

## 3. 已知约束

- 引导方法的**前三个形参**（Lookup / name / MethodType）不是静态参数；静态参数从第 4 个位置
  开始（这一条踩过一次：`makeConcatWithConstants` 的配方是 `typed[0]`）；
- 直接句柄的类型：`REF_invokeVirtual/Interface/Special` 把接收者算第一个参数，
  `REF_newInvokeSpecial` 返回类本身，字段句柄的 CP 描述符是字段类型；
- `invokeExact` 必须类型完全一致（否则 `WrongMethodTypeException`），`invoke` 才允许适配；
- 独立模式（没挂 `VMLESS_JDK`）下 `java.lang.invoke.*` 没有字节码，只能走 VM 的接管层；
  真字节码模式（`VMLESS_JDK`）下这些类**不执行字节码、也不跑 `<clinit>`**。

## 4. 方案

三层：常量与引导方法解析（缓存）→ 方法句柄执行与 asType 适配 → `java.lang.invoke` 的
字节码级接管。三个工厂（StringConcatFactory / LambdaMetafactory / ConstantBootstraps）
走 Rust 快路径（它们的 Java 实现依赖 VM 内部原语），**其它引导方法**走通用路径
（真执行那段 Java 字节码，取 `CallSite.getTarget()`）。详见 `docs/invokedynamic.md`。

## 5. 验收标准（可执行）

| # | 标准 | 命令 |
| --- | --- | --- |
| 1 | `I1..I11` **四路**输出逐字节一致（HotSpot / 真字节码 / 独立模式 / JIT 降级） | `cd /d/vmless/jdkms/indy && ./cmp.sh` |
| 2 | 真字节码模式缺 native = 0 | `run_jdk . I<n>` 输出里的「缺 native: 0」 |
| 3 | 调用点只解析一次 | `rustjava -Dvmless.indy.stats=1 -cp . I6` |
| 4 | 回归零退化 | `cd /d/vmless && ./verify.sh /d/vmless/bindy` → 失败 0 |
| 5 | 单测全绿 | `cargo test --release` |

## 6. 边界（三节都不空）

见 §1 / §2 / §5。

## 7. 实现记录

- `src/classfile/attr.rs::bootstrap_methods` —— 解析 `BootstrapMethods` 属性；
- `src/vm/indy.rs` —— 本层全部逻辑（约 1.9k 行，含单测）：
  常量重建（`reify_const`）、引导方法（`run_bootstrap`）、句柄类型（`handle_type`）、
  执行与适配（`invoke_mh_as` / `coerce`）、拼接配方（`parse_recipe` / `concat_eval`）、
  lambda（`bsm_lambda` / `lambda_dispatch`）、ConstantBootstraps、VarHandle、
  `java.lang.invoke` 接管（`invoke_api`）；
- `src/vm/exec.rs` —— `0xba` 分派、`ldc` 的 MethodHandle/MethodType/Dynamic、
  invoke* 三处对 `java/lang/invoke/` 的接管（static 的那处**必须在 `<clinit>` 之前**）；
- `src/vm/class.rs` —— `java.lang.invoke` 的继承链（独立模式没有字节码）、
  合成 lambda 类的 `instanceof` 判定；
- `src/vm/load.rs` —— `resolve_method` 跳过抽象方法（否则接口方法调用会拿到没有 Code 的声明）；
- `src/jit/{refmap,translate}.rs` —— `0xba` 的明确报错；`examples/run_jit.rs` 退回解释器；
- `src/jdk.rs` / `src/vm/natives.rs` —— `String.valueOf` 七个重载、`Class.getName()`、
  `Object.getClass()`、包装类 `TYPE` 字段（独立模式）；`Runtime.availableProcessors` native。

## 8. 验收记录（实际跑过的命令 + 实际输出）

```
$ cd /d/vmless/jdkms/indy && ./cmp.sh
  I1  OK     I2  OK     I3  OK     I4  OK     I5  OK
  I6  OK     I7  OK     I8  OK     I9  OK     I10 OK
  I11 OK
四路一致 11  失败 0
```

```
$ for c in I1..I9: run_jdk.exe . $c        （VMLESS_JDK=D:/vmless/patch/img）
I1  退出值 = 0 缺 native: 0
I2  退出值 = 0 缺 native: 0
I3  退出值 = 0 缺 native: 0
I4  退出值 = 0 缺 native: 0
I5  退出值 = 0 缺 native: 0
I6  退出值 = 0 缺 native: 0
I7  退出值 = 0 缺 native: 0
I8  退出值 = 0 缺 native: 0
I9  退出值 = 0 缺 native: 0
I10 退出值 = 0 缺 native: 0
I11 退出值 = 0 缺 native: 0
```

```
$ VMLESS_JDK=… rustjava.exe -Dvmless.indy.stats=1 -cp . I6
[indy] 调用点解析 2 次，执行 100001 次，lambda 实例 1 个，缓存命中 99999 次
788890
450000
```

```
$ /d/vmless/bindy/run_jit.exe I2.class
JIT 跳过：JIT 不支持 invokedynamic（0xba@0，I2 常量池 #7）：lambda / 字符串拼接只能由解释器执行（JIT 未接入调用点缓存）
→ 退回解释器执行（invokedynamic 由 vm::indy 完整实现）
hi
hi2
hi
false
true
```

```
$ cd /d/vmless && ./verify.sh /d/vmless/bindy
==========================================
三路一致: 103    失败: 0    缺样例: 0
==========================================
```

```
$ cd /d/vmless/rustjavac && cargo test --release
test result: ok. 50 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
```

## 9. 略过未修

- `P2 · src/vm/indy.rs · Lookup.unreflect* 与 MethodHandleInfo 未实现 · 需要 java.lang.reflect 的
  Method/Constructor 层，VM 只有 Field 的雏形`（略过，报明确错误）
- `P2 · src/vm/indy.rs · VarHandle.accessModeType / isAccessModeSupported 未实现 · 用得少，
  报明确错误`
