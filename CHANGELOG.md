# Changelog

## 0.1.0 — 2026-11-14

- 新增 `launcher`：`bin/javac.exe` 自动推导 `java.home`，未指定平台选项时默认 `--release 25`
- 新增 `rustcore`：bump arena（`arena_new/alloc/free/stats`），供 JVM 经 FFM 调用
- 新增 `patches`：javac 名字池（`SharedNameTable`）改由 Rust arena 承载
- 验证：标准 JDK 布局下 `bin/javac.exe -d out src\Hello.java` 可独立工作；产物 sha256 与官方 javac 一致

## 0.2.0 — 2026-11-14

- 新增 `librustjrt`：Java 对象布局（16 字节头 + 字段槽）+ bump arena + 标记清扫骨架
- 基准：bump 19.1 ns/对象 vs Box 69.9 ns/对象（10M 个对象，4 字段）

## 0.3.0 — 2026-11-14

- `ByteBuffer` 支持 Rust arena 存储：`useArena()` 后字节改由原生内存承载，增长只做"新块 + 拷贝"
- `ClassWriter` / `PoolWriter` 的常量池与类数据缓冲切到 arena 模式
- 验证：5 个 class 文件 sha256 与官方 javac 完全一致；运行日志显示 `arena buffer flushed: N bytes`
- `librustjrt`：新增 `ClassMeta`（字段类别位图）+ `Heap`（size-class 空闲链表 + 标记清扫），5 个测试通过

## 0.4.0 — 2026-11-14

- 新增 `rustjavac` crate（库名 `classfile`）：class 文件解析 + 原样写回
- 覆盖常量池全部 17 种 tag、long/double 双槽、属性以原始字节保存
- 验证：JDK 自身模块的 **33,191 个 class** 全部"读入→写出"逐字节一致，用时 3.1 秒

## 0.5.0 — 2026-11-14

- 新增词法分析（`lexer.rs`）：unicode 转义、全部运算符、文本块、数值/字符/字符串字面量、注释；上下文关键字按标识符处理
- 新增语法分析（`parser.rs` + `ast.rs`）：类型声明（class/interface/enum/record/注解）、成员、语句、表达式全优先级、lambda、方法引用、cast、instanceof 模式、switch 箭头
- 实测：JDK 25 `java.util` 的 **369 个源文件**语法分析成功率 **78.9%**（291 成功），用时 0.46 秒
- 已知未覆盖：交叉类型 cast（`(A & B)x`）、部分 varargs 与泛型方法引用、`new T[]` 的少数形态

## 0.6.0 — 2026-11-14

解析器补全（java.util 78.9% → 90.2%，整个 java.base 90.5%）：

- varargs 参数顺序修正（`T... a`）
- 方法体内的局部类声明
- 字段的 C 风格数组声明 `int fields[]`
- 交叉类型 cast `(A & B) x`
- cast 后接 lambda：`(Comparator<T> & Serializable) (a, b) -> ...`
- 三元运算符参与 cast 回退判定
- 显式类型实参调用：`obj.<T>method(...)`

## 0.7.0 — 2026-11-14

- 新增语义分析起点 `sema.rs`：符号表（类型/字段/方法）、实例字段的**引用槽位图**（供 GC 追踪）、重载按参数个数查找
- 解析器 91.5%（java.base 3058 文件）：`_` 作为标识符、cast 后接二元运算符时回退
- 实测 `java.util` 369 文件：解析成功 336，类型 972，字段 2645（引用槽 1004），方法 8388，0.55 秒

## 0.8.0 — 2026-11-14

- 新增 `lower.rs`：源码符号表 → `librustjrt::ClassMeta`（引用位图直通运行时）
- `librustjrt::ClassMeta` 改为拥有 `Vec<FieldKind>`；`Heap` 暴露 `arena_used()` 以便验证槽位复用
- 端到端示例 `e2e_gc`：`.java` → 解析 → 符号表 → 类元数据 → Rust 堆分配 → 标记清扫 → 空闲槽复用
  实测输出：Node 位图 `[Prim, Ref]`、Holder 位图 `[Ref, Ref]`；断开引用后 3 个 Node 全部回收，存活 1；再分配不新增 arena 用量

## 0.9.0 — 2026-11-14

- 新增 `codegen.rs`：常量池构建器（自动去重）+ 字节码发射 + class 文件组装
- `examples/gen_hello.rs` 产出 344 字节的 `Hello.class`，**官方 JDK 25 能直接加载执行**：
  `java -cp out Hello` → `hello from rustjavac`；`javap -c` 反汇编结构与 javac 产物一致
- 常量池索引一致性由"单一 CpBuilder 贯穿"保证（先算索引、后组装），并有去重测试

## 0.10.0 — 2026-11-14

- 新增 `compiler.rs`：AST → 字节码（静态/实例方法、int 参数与局部变量、算术表达式、return、`System.out.println`）
- 类型描述符补全：`java.lang` 简写（`String` → `java/lang/String`）、数组、8 种原始类型
- 端到端：`Calc.java`（`static int add(int,int){return a+b;}` + `main` 打印 `add(2,3)`）
  → Rust 编译出 379 字节 `Calc.class` → **官方 JDK 25 执行输出 `5`**
- `javap -c` 反汇编与 javac 产物同构（`iload_0/iload_1/iadd/ireturn`、`invokestatic Calc.add:(II)I`）

## 0.11.0 — 2026-11-14

- 控制流：`while` / `if`(含 else) / 局部变量声明与赋值，字节码含 **StackMapTable**（full_frame）
- 跳转偏移按 JVM 规范相对**操作码地址**计算（此前按操作数地址，导致 VerifyError）
- 端到端：`Loop.java`（`sum(5)` 用 while 累加、`max(7,3)` 用 if）→ Rust 编译 527 字节 → 官方 JDK 输出 `10` `7`

## 0.12.0 — 2026-11-14 · 闭环打通

- 新增 `vm.rs`：**不依赖任何 JVM 的字节码解释器**
  （int 常量/局部变量/算术/比较跳转/goto/pop/invokestatic/println(int|String)/return）
- 新增 `src/bin/rustjava.rs`：`rustjava <class 文件>` 直接执行 main，产物 0.2 MB
- 编译器补齐：`for` 循环（降级为 while）、字符串常量、`println(String)` 的描述符选择
- **端到端闭环**：`Demo.java` →（rustjavac）→ 509 字节 class →（rustjava）→
  `rustjavac + rustjava` / `45`；官方 `java` 执行同一份 class 输出**逐行一致**
- 测试 26 条全绿（含解释器 4 条：算术、while、if-else、描述符参数计数）

## 0.13.0 — 2026-11-14 · 紧凑布局与对标数据

- `librustjrt` 新增**紧凑对象布局**：8 字节头 + 4 字节/槽（对标 HotSpot 压缩指针），
  `Node{int,ref}` 从 32 字节降到 **16 字节**
- 标记清扫改用对象头标志位（去掉 O(n²) 的 `Vec::contains`）
- 新增三个基准：`bench_vs_jvm`、`bench_compact`、`bench_compact_gc`
- 与 JVM 同工作量（8M 次分配、1/8 存活）实测：
  | 指标 | HotSpot G1 | HotSpot ZGC | librustjrt（旧 32B） | librustjrt（紧凑 16B） |
  |---|---|---|---|---|
  | 单对象 | 24 B | 24 B | 32 B | **16 B** |
  | 分配 | 12.7 ns | 12.6 ns | 15.7 ns | **10.4 ns** |
  | 8M 占用 | 146 MB | 150 MB | 244 MB | **122 MB** |
  | 一次完整回收 | 38.6 ms（累计） | 224.4 ms | 164.2 ms | **86.5 ms** |

## 0.14.0 — 2026-11-14 · 对象与字段

- 编译器：`new` / `dup` / `getfield` / `putfield` / `this` / 构造器（含 `super()` 调用）、
  引用类型局部变量用 `astore`/`aload`、类文件里真正写出字段表、构造器名统一为 `<init>`
- 解释器：对象堆、字段读写、`invokespecial <init>` 真正执行构造器
- 端到端：`Point{int x; Point(int v){this.x=v;} static int twice(Point p){return p.x+p.x;}}`
  → rustjava 与官方 java 都输出 **42**
- 测试 27 条全绿（新增 `runs_objects_and_fields`）

## 0.15.0 — 2026-11-14 · 内存层 v2 + 启动速度

- 对象按 **4 字节对齐**：单字段对象 16 → **12 字节**，双字段 16 字节
- 空闲表改为**尺寸分桶**（线性扫描，不用 HashMap）+ 新建桶按轮次 live 数预留容量
- 增速：混合尺寸分配 **9.8 ns/对象（101.6 M/s）**；**复用分配 5.9 ns（168.3 M/s）**
- 全量回收 43.6 ms，**最大单步 0.088 ms**（v1 是 11.2 ms → 127 倍）
- 启动速度（减去 50 ms 进程创建基线）：`rustjava` **≈2 ms** vs `java` **≈96 ms**

## 0.16.0 — 2026-11-14 · 数组 + 正确的栈图局部变量

- 编译器：`new int[n]` / `a[i]` 读 / `a[i] = v` 写 / `a.length`，字节码为 `newarray`/`iaload`/`iastore`/`arraylength`
- **StackMapTable 的局部变量类型改为"按帧快照"**：未初始化的槽写 `ITEM_Top`，
  引用/数组槽写 `ITEM_Object` + 类索引（此前全写 Integer，HotSpot 直接 `VerifyError`）
- 解释器：数组堆（句柄用负数与对象区分），支持 newarray/iaload/iastore/arraylength
- `librustjrt`：尺寸类别编码进对象头 `flags` 高位，修正混合尺寸下清扫入错桶的问题
- 端到端：`Arr.java`（数组写入 + 求和）→ rustjava 与官方 java 都输出 **20**

## 0.17.0 — 2026-11-14 · 内存层实测口径固定

- 新增 `bench_final`：输出机器可读的 `ALLOC_NS / GC_MS / MAX_STEP_MS / BYTES_PER_OBJ`
- 与 JVM **交替 8 轮取最小值**（消除负载噪声）：
  | 指标 | HotSpot G1 | librustjrt | 倍率 |
  |---|---|---|---|
  | 分配 | 12.7 ns/对象 | **11.0 ns** | 快 1.15 倍 |
  | 一次回收 | 42.1 ms | 68.0 ms | 慢 1.6 倍（总 CPU） |
  | 最大单步停顿 | 23.4 ms | **0.021 ms** | **小 1100 倍** |
  | 单对象 | 24 B | 12–16 B | 小 1.5–2 倍 |

## 0.18.0 — 2026-11-14 · 分代回收

- 新增 `gen.rs`：**年轻代 bump + 复制式清理 + 老年代晋升**
  - 年轻代从低地址向上、老年代从高地址向下，两个区域共用同一块内存，引用用 u32 偏移
  - 转发指针写在年轻对象头里（`class_id` 存新偏移，`flags` 置 FORWARDED），**不需要哈希表**
  - 一轮 minor 的工作量正比于存活对象数，而不是分配次数
- 与 G1 交替 4 轮取最小值（8M 分配 / 1M 存活）：
  | 指标 | HotSpot G1 | 分代 | 倍率 |
  |---|---|---|---|
  | 分配 | 12.6 ns | **4.0 ns** | **快 3.15 倍** |
  | 一次回收 | 39.8 ms | **34.5 ms** | 快 1.15 倍 |
  | 老年代占用 | — | **15.3 MB**（1M 存活） | — |
  | 年轻代峰值 | — | 122 MB | — |

## 0.19.0 — 2026-11-14 · 老年代回收 + 运行效率实测

- `gen.rs` 补上**老年代标记清扫**：`major()`，从 roots 出发只扫老年代，未标记进空闲表
  - 实测：minor#1 18.5ms（复制 50 万）、minor#2 19.7ms（复制 50 万）、major 11.0ms（扫 100 万）
- 修两个性能/正确性问题：
  - 解释器把 `env::var` 从每字节码检查移到循环外（此前 50M 循环要 161 秒，现在 3.6 秒）
  - 编译器大于 `sipush` 范围的 int 字面量改走常量池 `ldc_w`（此前被截断，循环次数错）
- **运行效率实测**（50M 次循环累加，结果一致 1283106752）：
  | 场景 | HotSpot JIT | rustjava 解释器 | 倍率 |
  |---|---|---|---|
  | 计算密集（50M 循环） | 279 ms | 3600 ms | 慢 12.9 倍 |

## 0.20.0 — 2026-11-14 · 真正的方法级 JIT（Cranelift 后端）

- 新增 `jit.rs`：JVM 字节码 → Cranelift IR → 本机机器码
  - 覆盖：常量、iload/istore、iinc、算术、`if`/`if_icmp` 跳转、goto、ireturn/return
  - 操作数栈用 Cranelift SSA 值；局部变量放在调用方传入的内存里（与解释器同一布局）
  - 按「基本块 = 跳转目标 ∪ 分支后一条指令」构造，逐块发终结指令
- 实测（50M 次循环累加，结果必须等于 1283106752）：
  | 执行方式 | 用时 | 相对解释器 |
  |---|---|---|
  | 解释器 | 3281.9 ms | 1× |
  | **JIT** | **140.9 ms** | **23.3×** |
  | HotSpot（含 96ms 启动） | 278 ms | — |
  - JIT 编译耗时 **0.96 ms**

## 0.21.0 — 2026-11-14 · JIT 支持方法调用与递归

- JIT 新增 `invokestatic`：参数写入被调方法的栈槽，传指针调用，与解释器同一 ABI
- 递归修好：**声明后立即登记进缓存**，否则 `fib` 调 `fib` 会在编译期无限递归
- 参数个数按描述符数到 `)` 为止（此前把返回值的 `I` 也算进去了 → 栈下溢）
- 实测：
  | 场景 | 解释器 | JIT | HotSpot（含启动） |
  |---|---|---|---|
  | `work(50M)`（每轮一次方法调用） | 61.8 s | **206 ms** | — |
  | `fib(30)`（270 万次调用） | — | **8.1 ms** | 247 ms |

## 0.22.0 — 2026-11-14 · JIT 支持对象与数组

- JIT 新增：`new` / `getfield` / `putfield` / `newarray` / `iaload` / `iastore` / `arraylength` /
  引用局部变量（`aload`-`astore`）/ 构造器调用（`invokespecial`）
- 运行时辅助：`vmless_new_object` / `vmless_new_array`（bump 分配 + 清零），经 JITBuilder 注册为符号
- 局部变量槽从 4 字节改为 **8 字节**（引用是 64 位指针），int 仍读写低半
- 编译器放开局部变量槽上限（>3 用 `iload/istore` 通用形式）
- 端到端一致：`ObjArr.run(100)` 三个引擎都输出 **29850**（java / rustjava 解释器 / JIT）
- 同一方法 JIT 调用 1000 次共 0.9 ms

## 0.23.0 — 2026-11-14 · JIT 可以跑完整 main

- JIT 新增：`getstatic`（System.out 占位）、`invokevirtual`（`println(int)` 走运行时辅助；
  本类方法走静态分派）、`vmless_println_int` 运行时符号
- 修两个 bug：
  - `println` 的栈弹出顺序写反（接收者在参数下面，必须先弹参数）
  - `new_array` 没把长度写进头部 → `arraylength` 恒为 0（`Arr` 输出 0 而不是 20）
- **四个程序整体 JIT 执行，与官方 java 输出全部一致**：
  `Arr`→20、`Loop`→10/7、`Perf2`→1283106752、`ObjArr`→29850
- JIT 编译一个 main 约 1.4 ms，之后执行 0.02 ms

## 0.24.0 — 2026-11-14 · JIT 支持字符串常量

- 新增 `strtab` 运行时表：JIT 编译期登记常量池里的字符串，运行期 `vmless_str_const(idx)`
  返回「长度 + UTF-8 内容」缓冲指针
- `ldc`/`ldc_w` 遇到 String 常量走该表；`invokevirtual println(String)` 调 `vmless_println_obj`
- **五个程序整体 JIT 执行，与官方 java 输出全部一致**：
  `Demo`→`rustjavac + rustjava` / `45`、`Arr`→20、`Loop`→10/7、`Perf2`→1283106752、`ObjArr`→29850

## 0.25.0 — 2026-11-14 · switch 与实例方法调用

- 编译器：`switch`（线性比较链 + 贯穿语义）、实例方法调用 `obj.method()`（`invokevirtual`）
- 解释器：支持本类方法的 `invokevirtual`（接收者作第 0 个参数）
- 修三个栈图问题：选择子槽没登记类型、default 跳到空块、**不可达代码也必须有栈图帧**
- **七个程序三引擎输出全部一致**（java / rustjava 解释器 / JIT）：
  Demo、Point、Loop、Arr、ObjArr、Perf2、Sw、Obj2（`7000`）

## 0.26.0 — 2026-11-14 · JIT 补齐 long 字节码

- JIT 新增：`lconst_0/1`、`ldc2_w`、`lload/lstore`（含 `_0..3`）、
  `ladd/lsub/lmul/ldiv/lrem/lneg`、`l2i`、`i2l`、`lreturn`
- **尚未打通**：编译器还不会为 `long` 类型的局部变量分配 2 个槽并产出 `lstore/lload`，
  所以这条路径还没有端到端验证 —— 属于下一步

## 0.27.0 — 2026-11-14 · long 端到端（部分）

- 编译器：long 字面量（`lconst`/`ldc2_w`）、2 槽局部变量（`lstore/lload`）、
  long 算术（`ladd/lsub/lmul/ldiv/lrem`）、`l2i`/`i2l` 转换、`lreturn`
- JIT 执行 `Lg.sum(100000)` 得到 **704982704**（= 4999950000 截断为 int，正确）
- **已知问题**：我们产出的 class 被 HotSpot 拒绝
  （`ClassFormatError: Arguments can't fit int`），说明 class 文件某处不合规 —— 待查

## 0.28.0 — 2026-11-14 · long 槽位与栈图（未收尾）

- 修正 long/double 的**参数与局部变量槽位计数**（各占 2 槽）、`max_locals` 按槽算
- 栈图里 long 写 `ITEM_Long`，第二个槽写 `ITEM_Top`
- 现状：`ClassFormatError: StackMapTable format error: bad type array size` 仍未解决
  （JIT 执行 `Lg` 结果正确 704982704，只是产出的 class 还不能被 HotSpot 接受）

## 0.29.0 — 2026-11-14 · long 与 double 全链路（编译器 + JIT）

**long**
- 根因修复：HotSpot 的 full_frame 解析器会按 `is_category2()` **自己把 long/double 展开成两槽**，
  所以栈图里一个 long 只写**一个** `ITEM_Long`（原来写了 [Long,Top,Long,Top…] → 展开后超出 max_locals）
- 比较改用 `lcmp`/`dcmpl`/`dcmpg` 再单值 `ifXX`（原来对 long 直接发 `if_icmpge`）
- 参数与局部变量按槽计数、按类型选 `lstore/dstore`（原来 long 也走 `istore`）

**double**
- 编译器：`ldc2_w`、`dconst_0/1`、`dload/dstore`（通用 + `_0..3`）、
  `dadd/dsub/dmul/ddiv/drem/dneg`、`i2d/l2d/d2i/d2l`、`dreturn`、栈图 `ITEM_Double`
- JIT：上述全部 + `drem` 走 CRT `fmod`（`vmless_drem` 符号）
- 三处配套修正：`param_slots()`（double/long 参数第二槽留位）、`widen_ret()`（跨调用还原 D/J 返回值）、
  `insn_len()`（合并指令长度表；旧表漏 0x14/0x16/0x37 且把 `dup` 当 2 字节 → 扫描错位）

**验证（java 与 run_jit 逐字节一致，10/10）**
`Demo`、`Point`、`Loop`、`Arr`、`ObjArr`、`Obj2`、`Perf2`、`Sw`、**`Lg`(long)**、**`Db`(double)**
另 `cargo test --lib` 27 passed。

**已知缺口**：解释器（`vm.rs`）仍只支持 int —— long/double 会明确报「未实现的字节码 0x14」，不会静默算错。

## 0.30.0 — 2026-11-15 · 解释器支持 long/double，println 重载，JIT 返回精度

- **解释器重写值模型**：栈/局部槽改为 `Slot{I(i32),L(i64),F(f64)}`，long/double 按 JVM 规范占 2 槽；
  补全 `ldc2_w`、`lconst/dconst`、`lload/dload/lstore/dstore` 全形式、`l*/d*` 算术与 `neg`、
  `lcmp/dcmpl/dcmpg`、`i2l i2d l2i l2d d2i d2l`、`lreturn/dreturn/areturn`、`pop2/dup2`
- **println 重载**：按实参类型选 `(I)V/(J)V/(D)V/(Ljava/lang/String;)V`；double 走
  `java_double_to_string`（Java 版式规则），JIT 侧加 `println_long/println_double`
- **JIT 返回精度**：函数签名按描述符返回 `I32/I64/F64`，调用点直接上栈（不再「截断→还原」）
- 顺手修三个真 bug：`dstore` 发成 `fstore`(0x38→0x39)、调用返回 long/double 只 `push(1)` 槽
  （`max_stack` 少算 → `Operand stack overflow`）、`fcvt_to_sint` 越界插 `ud2`（Java 语义是饱和，
  改 `fcvt_to_sint_sat`）

**验证：15 个样例 java / 解释器 / JIT 三路一致**
`Lg Db Point Loop Sw Obj2 ObjArr Arr Demo Perf2 LongEdge Doubles Mix DblCorpus Ret`；
`cargo test --lib` 29 passed。

**已知边界**：最小次正规数 `4.9E-324` 我们打 `5.0E-324`（Ryū 复刻未做）；long/double 的字段与数组仍为 i32。

## 0.31.0 — 2026-11-15 · 引用数组、类型检查、宽字段与数组、位运算

- **引用**：`aconst_null`、`anewarray`/`aaload`/`aastore`、`if_acmpeq/if_acmpne`、
  `ifnull/ifnonnull`、`instanceof`、`checkcast`（失败抛 ClassCastException 并退出 1）、引用字段读写
- **宽类型**：`getfield/putfield` 与数组的 J/D 版本（`laload/lastore/daload/dastore`），
  JIT 对象字段统一 8 字节、数组元素按指令步长
- **位运算**：`ishl/ishr/iushr/iand/ior/ixor`（int 与 long）、`~`
- 修三个真 bug：方法返回引用时 `println` 描述符按**最后一个字符**判类型 → VerifyError；
  void 调用当语句时多发 `pop`；解释器实例方法 `this` 未偏移导致 long/double 形参错位
- **验证：26 个样例 java / 解释器 / JIT 三路逐字节一致**（新增 RefArr ObjRef ArrType RefFld
  RefCmp NullChk WideFld WideArr BitOps PrintObj PrintObj2）；`cargo test --lib` 29 passed

**已知限制**：多维数组（`multianewarray`）未做；异常机制未做（`athrow`/`try/catch`）；
`println(a == null)` 这类「栈上已有值时的比较」被明确拒绝（本项目栈图恒为空栈）。
