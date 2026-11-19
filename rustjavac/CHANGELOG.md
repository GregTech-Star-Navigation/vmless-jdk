# 变更记录

格式：`## <版本>` + 「为什么 / 改了什么」。修 bug → patch，加能力或改默认行为 → minor，
不兼容的配置/接口变更 → major。纯文档/测试/重构不动版本号。
## 0.79.0

**为什么**：hello 的峰值 RSS 里 85 MB 是类表（进程内探针实测：进 main 4.8 → 解析完 64.8 →
建表完 89.6 → initPhase1 105.2 MB）。其中常量池占大头，而 JDK 常量池 80%+ 是 Utf8。

**先纠正一个错的容量模型**（我自己估的，实测推翻了）：`size_of::<Option<CpEntry>>()` 是 **24 B**
不是 32（`Vec<u8>` 的非空指针给了 tag niche）；Utf8 跨类去重后只有 **84,618 条 / 3.15 MB**
（不是 9.8 MB）；建表总共 **8.3 MB**（不是 25）。所以「−40 MB」那个估算是错的。

**改了什么**

- **常量池 Utf8 换成 arena**：`CpEntry::Utf8(String)` → `Utf8(u32 off, u32 len)`，
  所有 Utf8 字节拼进一块 `Box<[u8]>`（`Utf8Arena`），`ClassFile.utf8: Arc<Utf8Arena>`。
  **`Option<CpEntry>` 24 → 16 B**（`Long/Double` 的 u64 把对齐钉在 8）。
  **Utf8 分配次数 433,765 → 16**（每个解析线程块共用一个 `Utf8Builder`，freeze 后回填同一 `Arc`）。
- **CDS 装载路径不再「全读 27 MB 字节再统一解析」**：改成常驻线程逐类「读 → 解析 → 放掉字节」，
  字节与结构不再同时存活。

**结果**（同窗口交错 A/B，25 ms 采样，中位）

| | 基线 | 新 |
| --- | --- | --- |
| hello 峰值 RSS | 90.7 MB | **69.0 MB（−24%）** |
| hello 启动 | 252 / 234 ms | **201 / 198 ms**（更快） |
| GcBench 峰值 | 162.6 MB | 157.9 MB |

**验证**：`verify.sh` 103/0、`cargo test` 124 passed、GC_STRESS 10 样例 × 15 次 = 150/150、
真程序样张 5/5、Cas1/Mt1/Mx 各 20/20、`VMLESS_CDS=0`（无缓存 BFS 路径）也通过、
CDS 往返字节一致。

**已知边界**：跨类全局 Utf8 去重还能省 6.6 MB，但要么 433,765 次串行查表（15–35 ms，启动预算吃不起），
要么上无锁 arena —— 都没做。再往 50 MB 要动属性 info（12.2 MB / 27 处读取点）、
建表（8.3 MB / 30+ 处读者）、`initPhase1`（7.9 MB），那是另一条线。

**另记**：`A02MergeSort` 有既有 VerifyError（HotSpot 与基线二进制报错逐字相同，非本次引入），
和 `docs/java-base-失败分布.md` 里那 60 个是同一类帧生成缺口。

## 0.78.0

**为什么**：每次 Java 调用的固定开销是 1486 ns —— 这是真实程序比 HotSpot 慢 6.5 倍的唯一来源
（native 体优化只值 5%，因为一次 `Integer.toString` 有 25 次调用）。

**先拆分**（rdtsc，`VMLESS_CALLPROF=1`）：

| 段 | ns | 占比 |
| --- | --- | --- |
| **`resolve_method`** | **907** | **61%** ← 每次 `chain()` 都做「每层 2 次 String 分配 + `method_matches` 再一次 `this_class_name()`」 |
| `Frame::new` | 136 | 9% |
| 返回路径 + Drop | ≈250 | 17%（2 次 free + Rc/Arc 析构） |
| `ensure_init` | 66 | 4% |
| 其余（site_ref / st-enter / CODE_SITE / ARG_SITE / Arc clone …） | ≈230 | 15% |

**改了什么**

- **方法解析按调用点缓存**（JVMS 5.4.3.3 允许）：`resolve_method` **907 → 16 ns**。
- **虚调用用 `heap.class_of` 取代 `ref_kind`**（后者每次分配 String 类名）+ 单态缓存
  （键是类名 `Arc` 指针，并攥住该 Arc 防地址复用）：虚调用 1.3 μs → **0.44 μs**；
  `s.length()` 5187 → 1686 ns、`toString` 267 → 177 ns。
- **`Frame` 的槽位改内联 8 槽 + 溢出堆**（`frame.rs::Slots`）：**不池化**，每帧自己清零。
  `Frame::new` 136 → ~0，析构零 free。
- **`getfield`/`putfield` 偏移按调用点缓存**，带**「类表代数」守卫**（`classes.len()` 一变全部作废），
  只缓存解析成功的；**槽数绝不缓存**：`pt.x` 929 → **145 ns**。
- **`NewSite` 接上**（类名 `Box::leak` 成 `&'static str` + 每站点 inited 标志；**槽数仍每次查**）：
  `new Object()` 1750 → **905 ns**。
- **invoke 参数改栈上小缓冲**（`argc+1 ≤ 8`）：虚调用从 3 次分配降到 0。

**结果**：空静态方法 **1486 → 333 ns**、带参 1663 → 502、native 1576 → 300。
Mb4：`toString` 431 → **170 ms**、`concat` 1437 → **481 ms**、`newStr` 181 → 68。

真程序（同 harness，含启动）：R1 2238 → **1073**、R2 633 → **515**、R3 2406 → **1187**、R5 431 → **401**；
扣掉启动后 R1 **902** / R2 **344** / R3 **1016** / R5 **230** ms。

**验证**：真程序 5/5、`verify.sh` 103/0、**GC_STRESS 10 样例 × 15 次 = 150/150**、
`cargo test` 124、Cas1/Mt1/Mx 各 20/20、JIT 持平（P1 17–18 / P2 64–65 / R4-JIT 22.7）。

**剩下的**：`getstatic` 355 ns / `putstatic` 1150 ns（要改 `shared.rs` 的表的键类型才根治）、
`verify` 的 thread-local 缓存只有 1 项（两种方法交替就每次掉全局锁）、
解释器本身 ~28 ns/字节码（再上一档要动主循环里每条指令一次的 `frame.pc = pc`）。

## 0.77.0

**为什么**：rdtsc 打点实测出四条 native/分配热点，其中 `Unsafe.putByte` 的 native 体要 678 ns。

**改了什么**

- **`Unsafe` 标量读写去掉 `Vm::ref_kind`**（`native_reg.rs`）。`ref_kind` **每次分配一个 String 类名**，
  一次 `putByte` 调了 4 次。新增 `ARR_W`（class_id → 元素宽度，无锁直连表）+ `arr_kind()`；
  数组句柄是负数，一次符号判断就分流；`unsafe_put_impl` 加数组快路，跳过 `class_obj_name`
  的互斥量 + clone。宽度 ≤2 时直接走 `read_mem/write_mem`，跳过 `field_type`（那也要分配 String + 走继承链）。
  **native 体 678 → 45.6 ns（15×）**。
- **`System.nanoTime`** 换成裸 `QueryPerformanceCounter` + 一次乘法（频率整除 1e9 时 ns = 刻度 × 100）。
  **47.6 → 28.3 ns**，且由「系统时间、可回拨」改成**单调**。
- **`Reference.waitForReferencePendingList`** 从 `sleep(1ms)` 死循环改成
  `Condvar::wait_timeout(1s)`。6 秒内 **4096+ 次 → <512 次**（原来白烧一个核）。

**结果**：Mb4 `toString` 462→429 ms、`valueOf` 491→446 ms；真程序 R3 2030→1911 ms。
真程序样张 5/5、`cargo test` 124 passed、GC_STRESS 10 样例 × 15 次 = 150/150。

**一条重要的归因纠正**（推翻 0.72–0.75 期间的假设）：这四条热点合起来只占 Mb4/R1 的**个位数百分比**。
真正的瓶颈是 **`execute`/`execute_body` 的每次调用固定开销 ≈1.45 µs**。证据链：
空循环 102 ns；**空静态 Java 方法 1523 ns**；带参静态调用 1659 ns；
**native 调用 1534 ns 而 native 体只有 47.6 ns** → 派发 + 帧 = 1486 ns。
把 `putByte` 体砍 15 倍，Mb4 `toString` 只动 5% —— 因为一次 `toString` 有 ~25 次调用。

**另发现一处死代码**：`exec.rs:142` 声明了 `NewSite`（`new` 调用点缓存）但从未接上；
`exec.rs:1218` 的 `new` 仍每次都做 `class_name_of`（String 分配）+ `ensure_init` + `field_count` + `vec!`。

## 0.76.0

**为什么**：自研 javac 编 `java.base` 停在 81.5%，剩下的都是「名字/类型解析」这一层不够。

**改了什么**（每批都全量重测 3381 个文件）

- **字段赋值的 RHS 带跳转时先落脏槽**（`codegen/stmt.rs`、`expr.rs`）：
  `flag = (n >= 12);` 之前要先压 `aload_0`，而跳转点上栈必须为空。
- **裸名补齐「成员类型作用域」**（`codegen/types.rs::member_type_candidates`）：
  外层类链 `Outer$X`、父类型 `Super$X`、`import static p.Outer.T` 里的类型。
- **`Iface.super.m(...)`** → invokespecial 到 `InterfaceMethodref`。
- **标识符允许 `$`**（`front/lexer/mod.rs`）；**try-with-resources 支持已声明变量**
  （`try (in)`、`try (out; var w = …)`）。
- **匿名类支持外层实例 `this$0`**（`sema/capture.rs::add_outer_this`）：外层实例字段、
  外层实例方法、`Outer.this` 都能解析。
- **引用→基本类型的 cast 补拆箱**（`codegen/expr.rs::unbox_to`，描述符按 `()B/()S/()C`），
  以及 `System.Logger` 这类 java.lang 简写 + 嵌套类型。

**结果**：81.5% → **85.2%**（2880/3381）。真程序 5/5、`verify.sh` 103/0、`cargo test` 124 passed。
同一批 24 个文件里两边都能编的 67 个 class 中 **66 个与基线逐字节相同**。

**一条必须记住的既有缺陷**：用真 JVM `-Xverify:all` 装载 386 个新编出的类，
**60 个被拒载**（`Inconsistent stackmap frames at branch target` 一类）。用改动前的基线二进制
编同样的文件**复现同一批错误** —— 也就是说「编过了 ≠ class 是对的」在这个仓库里是
**系统性**的（栈图/帧生成缺口），不是这批改动引入的。需要单开一条线修。

**剩余桶**：未找到实例方法 153（泛型返回不代回）、未定义的变量 44（非匿名内层类的 `this$0`）、
parse 32+13+8（`case Type var ->`、record、`Class<?>[]::new`）、栈深泄漏 21+18
（某条语句漏记账后，其后所有带跳转的表达式全红 —— 已加 `VMLESS_STMT_DEPTH=1` 可定位）。

## 0.75.0

**为什么**：真实程序 R1/R3 比 HotSpot 慢 10 倍，而瓶颈不是解释器循环（实测 8.5 ns/字节码，正常水平），
是**每次 Java 方法调用的固定开销**——`System.initPhase1` 一项就是 9823 次调用。

**先拆分**（`VMLESS_CALLPROF=1`，rdtsc，ns/调用）：`execute_body` 序言共 **1255 ns**，其中
`var_os("VMLESS_TRACE")` 287 + `var_os("VMLESS_HANG_DUMP")` 294 = **581（46%）**；
`place_args` 285（每次重算 param_slots + clone 槽位表 = 2 次分配）；`Frame::new` 119；其余 ~200。

**改了什么**

- **环境变量提到 `OnceLock`**：−560 ns/调用（Windows 上 `var_os` 是全局 Mutex + 哈希 + OsString 比较，约 295 ns/次）。
- **`CodeMeta` 缓存**（按 `(ClassFile, 方法)`）：把「线性扫常量池找 `Code`」+ 异常表 + 引用位图
  从每次调用变成一次。
- **`ARG_SITE` 参数槽表 + `copy_from_slice` 落位**：285 → 10 ns。
- **`SITE_REFS` 方法引用解析缓存**：每次 `invoke` 省 3 次 `String` 分配。
- **`NAT_SITE` native 派发缓存**：645 → **85 ns/次**（JNI 跳板不缓存）。
- `frame.rs::place_args_slots`：落位规则搬到 `Frame`，`exec` 与 `native_reg` 共用。

**结果**：MCall 每调用 **2350 → 1630 ns**；Mb4 `toString` 654→456 ms、`concat` 2200→1546 ms。

**两条「缓存改变了语义」的红线**（二分到底，都回退了，写在这里防止后人重犯）

1. **按调用点缓存 `new` 的 `field_count`（nslots）→ `System.out` 变 null**（H.class 20/20 全红）。
   并行类加载下「类在表里」**不等于**「布局已定稿」，第一次读到的槽数不能当永久答案。
2. **帧的 locals/stack 缓冲区池化 → `Str1` 偶发丢根**（20 次里 1 次）。GC 的保守栈扫描
   （`scan_stack_range` + `conservative_frame`）依赖**栈上残留的指针**，复用缓冲区就改变了它看到的根集。

**可安全缓存的**：只读且来自 class 文件的东西 —— 字节码位置/异常表、引用位图、参数槽表、
native 的 fn 指针、常量池引用的解析结果。

**验证**：GC_STRESS 下 8 个样例 × 15 次 = 120/120；真程序 5/5；Cas1/Mt1/Mx 各 20/20；
`cargo test` 124 passed；JIT 不退化（P1 18 / P2 64.7 / R4-JIT 22.0 ms）。


## 0.70.0

**为什么**：VM 一直没有**字节码验证器**（`src/vm/` 下 30 个模块全是执行/GC/native），
于是我们自己编译器产出的**非法 class 会被我们自己的 VM 照跑**：

```
java -cp out R4       → VerifyError: Operand stack overflow @49: i2l
rustjava R4.class     → same=true min=38 max=999947 sum=15046571407   ← "正确"输出
```

三条验收腿（java / 解释器 / JIT）里后两条对非法 class 完全免疫，只有真 java 会报 ——
编译器出错时我们**看不见**。`R4` 这类最危险：错的字节码碰巧算出了对的答案，换个输入就崩。

**改了什么**（新增 `src/vm/verify.rs`，≈1900 行；JVMS 4.10.1 方法级校验）

- 进入方法体**之前**跑一次，结论按 `(ClassFile, 方法下标)` 缓存；热路径只剩一次
  线程本地比对（16M 次虚调用实测无退化）。放在 `exec::execute` 而不是 `execute_body`，
  这样 JIT 快路（`try_call_jit`）也挡得住；`run_jit` 那条完全不建 `Vm` 的入口
  另走 `vm::verify_class(&cf)`。
- 覆盖：指令解码（含 `wide`/`tableswitch`/`lookupswitch`）、跳转目标必须是**指令边界**、
  每条指令的净栈效应（`long`/`double` 占 2 槽）、`max_stack`/`max_locals` 越界、
  局部变量与操作数栈的**类型汇合**（前驱求最小上界）、`xload`/`xstore` 槽位类型、
  `invoke*` 与描述符一致、`getfield`/`putfield` 接收者非 `Top`、`aaload`/`aastore`
  元素类型、`new` → `<init>` 之前不许用、`StackMapTable` 帧与数据流核对。
- 失败抛 `java.lang.VerifyError`，消息 `类.方法 @bci: 原因`。
- **宁可放过不可误杀**：只有两边类层次都能解析时才判「不可赋值」，解析不到（独立模式下
  `java.*` 没有 class 文件）当未知放过；`jsr`/`ret` 直接跳过整个方法。
- 判据（实测）：`R4`/`R2` 三条腿全部 `VerifyError`（`R2` 在 `run_jdk` 下与 HotSpot 同因：
  `@3 invokevirtual` 的 `java.lang.String` 不能赋给 `java.net.URI`）；`verify.sh` 103/0、
  `cargo test` 117 passed、`Smoke1` 输出正常、`GcBench` 跑完、`P1`/`P2` 不退化（19 / 64 ms）。

## 0.69.0

**为什么**：改写堆的对象布局与回收策略。旧实现是「56 字节对象头 + 独立槽位数组」，
且**每次回收都是全量标记-清扫**、停顿与整个活集成正比 —— 8M 次分配 / 100 万存活的负载下
峰值 RSS **396 MB**（G1 是 175 MB）、最大停顿 12.5–16.9ms 与 G1 持平但压不下去。
目标是「停动、内存、分配吞吐全部压过 G1」。

**改了什么**（GcBench 8M 分配/1M 存活，5 次中位数）

| 指标 | 改前 | 改后 |
| --- | --- | --- |
| 峰值 RSS | 396.2 MB [395.4..397.2] | **166.8 MB [166.1..167.5]** |
| 最大单次停顿 | 16.81 ms [12.79..16.95] | **14.49 ms [13.58..14.79]** |
| 2M 短命对象峰值 RSS | 67.5 MB | **32.8 MB** |
| 1M/2M/4M 分配 RSS | 56.6 / 67.1 / 82.3 | **25.9 / 32.4 / 48.4** |
| 对象头 | 56 B | **24 B**（编译期断言） |

- **对象布局**（`shared.rs`）：`Arc<str>` 类名 → `u32 class_id`（全局类名表）；
  `Arc<RefBits>` → 80 字节域里内联位图（≤64 槽）或静态指针；`Box<[Cell]>` 槽位数组
  **内联在对象头后面**（一次分配，去掉 16 字节 malloc 块头 + 8 字节指针）。
  56 B → **24 B**，且「页表 → 头 → 槽位」三次访存变成两次。
- **分代回收**（写屏障 + 年轻代）：老对象里被写过引用的按**精确对象位图**记账
  （`DirtyMap`，1 bit/对象、按需分页），年轻代回收只扫新生段并把老对象的脏槽当根；
  扫槽时**直接丢掉老目标**（省掉一次页表查找）。eden 按存活率自适应 8–96 MB。
- **全量回收**：并行判据改为「上一轮**全量**标了多少」—— 与年轻代共用一份
  `last_marked` 会让每次全量都退化成单线程（实测 47ms → 13.7ms）；默认线程数 8 → 16，
  批次 1024 → 16384（锁竞争）；首轮回收固定为全量（冷启动时「下界之前都活着」不成立）。
- **分配路径**：清零与写头合并成一次内存往返；全零数组走 `memset`；大对象按
  `eden/8`（G1 的 region 思路）单独计 eden，避免一个 10MB 数组把 eden 撑爆。
- **触发判据改成字节口径**：旧的「分配个数 ≥ 存活×2」对 `new int[3]` 与 `new long[64]`
  一视同仁；且索引不复用时按页表高水位判会让第一次全量之后**每个检查点都做全量回收**。
- `heap.set` 也接写屏障（按槽位位图判断）：全仓几十处是 `heap.set` 直接写引用槽
  （反射 / JNI / indy / natives），逐个改成 `set_ref` 做不到。
- 删掉死代码：`Tlab` 结构（一次都没被调用）、`intern_class`、旧 `bytes()` 口径。

**回归**：`verify.sh` 103 一致 0 失败；`VMLESS_GC_STRESS=1` 下同样 103/0；
`cargo test --release` 110 通过 0 失败。

**已知未做**：全量回收的标记仍是 stop-the-world，停顿仍与活集成正比（100 万活对象
≈ 13.7ms）。做成增量标记需要在 `set_ref` 上加「已标黑对象被写时记灰」的
incremental-update 屏障 + 标记期间新对象按上一世代出生 + 终局 STW 重扫根与脏集；
本轮没做，因为标记阶段实测是**内存延迟/带宽受限**（8→32 线程只从 16.8ms 降到 13.3ms），
增量化的收益要等这一步落地后才能体现。
## 0.68.0

**为什么**：编译 java.base 的通过率停在 **73.9%（2497/3381）**，而且产出的 class 只是
“javap 读得出来”—— 真 JVM 会因为访问标志不对直接拒载（`native` 方法被标成 `abstract`、
接口字段不是 `public static final`），lambda / 局部类 / 方法引用整片没实现，
块体 lambda 的 `{ … }` 被塞进占位节点，跳转点上也不能压着操作数。

**改了什么**（通过率 **73.9% → 81.5%（2757/3381）**，真 JVM `-Xverify:all` 装载
**2486 个类 0 拒绝**，`cargo test` 110 通过、`verify.sh` 103 一致 0 失败）

- **局部类**：方法体里的 `class X {…}` 不再被丢掉，收成外层类型的嵌套类型，
  `sema::flatten` 拍平成 `Outer$Inner` 并改写使用点（`static Arena global() { class Holder {…}… }`
  这类写法以前一律报“未定义的变量 Holder”）。
- **lambda / 方法引用**：合成 `lambda$m$N` + `invokedynamic`（`LambdaMetafactory.metafactory`），
  含实例捕获（`REF_invokeVirtual` 把 `this` 当第一个捕获实参）、块体 lambda、
  `Foo::new` / `File[]::new`、未绑定实例引用（`String::length`，接收者补 checkcast）。
  新增常量池 `MethodType` / `MethodHandle` / `InvokeDynamic` 与 `BootstrapMethods` 属性生成。
- **SAM 类型实参代回**：`Consumer<String>` 擦成 `accept(Object)`，把目标类型写的实参代回
  形参，lambda 体里的接收者才不是 `Object`（以前 `s.isEmpty()` 全报“未找到实例方法”）。
  返回值**故意不代**（代错会 VerifyError，返回 `Object` 永远合法）。
- **跳转点操作数栈**：数组初始化 / 算术 / 比较把“会分叉的子表达式”提前算进脏槽，
  跳转点上永远不会压着别的值 —— 等价于放宽旧约束，且不需要非空栈帧。
- **访问修饰符**：不再一律写 `public`，按声明映射；`native` 不再标成 `abstract`；
  接口字段强制 `public static final`；有抽象方法的类自动补 `ACC_ABSTRACT`。
  这一条直接决定真 JVM 能不能装载。
- **长尾系统性根因**：继承来的静态方法（`invokestatic` 的 owner 必须是声明类）、
  裸名 `getClass`/`hashCode`/`toString`（Object 协议兜底）、静态导入指向嵌套类
  （`a.b.Outer.Inner.*` 要补 `# 变更记录

格式：`## <版本>` + 「为什么 / 改了什么」。修 bug → patch，加能力或改默认行为 → minor，
不兼容的配置/接口变更 → major。纯文档/测试/重构不动版本号。

 形态）、多声明符字段（`private int a, b;`）、文本块。
- **测量脚手架**：`emit_src` 加 `--dir` 批量模式（`platform()` 每进程只反解一次 classpath，
  逐文件起进程会把 7350 个 `.class` 反解几千遍）。

需求与验收记录：`docs/requirements/REQ-004-java-base覆盖率.md`。

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
