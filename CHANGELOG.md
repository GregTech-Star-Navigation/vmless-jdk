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

## 0.32.0 — 2026-11-16 · 异常机制（编译器 + 解释器 + JIT）

- **编译器**：`throw` → `athrow`；`try/catch/finally` 生成异常表，catch 入口帧的**栈上带一个异常对象**
  （本项目其它位置栈恒为空，这是唯一例外）；finally 复制两份（共享 + catch-all），
  `return` 在 try 内会先跑 finally 副本再返回
- **解释器**：`athrow`、按异常表匹配（含继承链）、跨方法向上抛、未捕获时
  `Exception in thread "main" <类>: <msg>` + 退出码 1
- **JIT**：哨兵路线 —— `exception_pending` 通道，调用点检查 pending 并跳到最近 handler；
  按类判定是否插检查点（不带异常的类零开销）；未捕获时打印 + exit(1)
- **关键坑**：异常表的「保护区结束」必须用只记偏移不产帧的 `place()`，
  否则它和 handler 入口同偏移，`resolve()` 的 dedup 会丢掉 handler 那份（栈上带异常对象）→
  HotSpot 报 `stack size doesn't match stackmap`
- 修 bug：`new RuntimeException("x")` 生成类名缺 `java/lang/` 前缀

**验证：7 个异常样例 + 26 个回归样例，HotSpot / 解释器 / JIT 三路逐字节一致**
（`TryThrow TryFinally TryNested TryCross TryCatch2 TryProbe TryProbe2` + 未捕获退出码 1/1/1）；
`cargo test --lib` 29 passed。

## 0.33.0 — 2026-11-16 · 前端模块化（纯结构重构）

- `lib.rs` 从 567 行降到 22 行，只剩模块声明与 re-export
- 拆出 `classfile/{mod,pool,attr,read,write}`、`front/{ast,lexer/,parser/{decl,stmt,expr}}`、
  `sema/{mod,symbols}`、`lower/`
- **零逻辑改动**：只新增 `pub mod`/`pub use`/`use` 与跨文件调用必需的 `pub(crate)`；
  逐行多重集合比对，丢失 0 行、新增 0 行逻辑
- 公开 API 全兼容，`examples/` 零改动；`cargo test` 29 passed；样例 **34/34 三路一致**
- 新增 `docs/模块结构.md`

## 0.34.0 — 2026-11-16 · 字节码生成器模块化 + 清理死代码

- `compiler.rs`(1974) + `codegen.rs`(373) 合并拆成 `src/codegen/`：
  `mod · error · emit · constpool · types · ctx · expr · stmt · method · class · simple`（2448 行 / 11 文件）
- `lib.rs` 用 `pub use crate::codegen as compiler;` 保住旧路径 `classfile::compiler::*`
- **清理**：删除零调用方的 `expr_is_long`、只写不读的 `Ctx::is_static`、
  重复的 `ACC_SUPER` 常量、拆分后暴露的 unused import
- 零逻辑改动（行多重集核对：除删除项与 import 裁剪外每行出现次数完全一致）
- `cargo test` 29 passed；样例 **34/34 三路一致**；`src/codegen` 零警告

另：仓库根新增 `verify.sh`（34 样例三路比对，带一次重试抗文件监视器干扰）。

## 0.35.0 — 2026-11-16 · 解释器与 JIT 模块化 + 清理死代码

- `vm.rs`(1346) → `src/vm/`：`mod · slot · frame · heap · class · exec · call · exception · natives`
- `jit.rs`(1985) → `src/jit/`：`mod · insn · rt · strtab · layout · translate · call · exc`
- **清理**：删 `unused import Signature/Variable`、从未展开的 `flush_to` 宏、
  7 处死赋值（5× `terminated = true` + 2× 重复 `cur_start = pc`）
- 零逻辑改动（新文件按原文件行号原样抽取，函数体一字未改；符号名单比对一致）
- `cargo test` 29 passed；样例 **34/34 三路一致**；vm/jit 警告归零

**遗留警告**：`front/parser/expr.rs` 3 处 `unreachable pattern`（保留待查，可能是真 bug 线索）。

## 0.36.0 — 2026-11-16 · Rust 内存层完整化

**从「最小可用」改成真正完整的内存管理系统。** 原实现有 5 套互不相干的分配器、
全局只有一个类布局、没有根集管理、**没有写屏障**（`old → young` 的引用会让活对象被误回收，正确性 bug）。

新结构：`object · layout · alloc · heap · young · old · cards · roots · barrier · stats · tests`
（`compat · gen` 是冻结的旧 API 兼容层，8 个旧 example 仍能编能跑）

| 缺陷 | 修法 |
|---|---|
| 全局单类布局 | 对象头存 `class_id`，GC 按**对象自己的类**取引用位图 |
| 无根集 | `RootScope`（RAII，不借用 Heap）+ `with_roots()` 影子栈 |
| 无写屏障 | `set_ref` 判「老→年轻」标脏卡；minor 根集 = 显式根 + SATB + 脏卡解析出的对象 |
| 无大对象空间 | ≥64K 或 ≥eden 的分配直接进老年代独立链表，复制式 GC 不碰 |
| 无 TLAB | `tlab_alloc(&self, size)` 用 `Cell` 无锁 bump + `refill_tlab()` |
| 无增长收缩 | 尾部满先真回收再倍增（偏移不变，handle 不失效）；空闲率高则截断 |
| 无统计 | `Stats`：各代占用、停顿、晋升量、脏卡数、增长/收缩次数 + 环形历史 |

**对象头从 16 字节压到 8 字节**（字段数放 flags 高 16 位），对象 24B→16B，MINOR 从 ~60ms 回到 36ms。

**实测（同机同刻，8M 分配 / 1M 存活）**
| | 旧实现 | 新实现 |
|---|---|---|
| 分配 | 10.8 ns/对象（bump，单类，无屏障） | **10.2 ns/对象**（多类 + 屏障 + TLAB） |
| 回收 | 标记清扫 89.5 ms | **minor 40.5 ms** |
| 最大单步 | — | **0.918 ms** |

`cargo test`：28 passed + 1 doc-test（含随机图压力测试，逐对象校验 payload 与引用）；
rustjavac 重编后样例仍 **34/34 三路一致**。

## 0.37.0 — 2026-11-17 · 继承体系与跨类虚分派

- **`extends`**：`super_class` 写出真实父类；新增 `sema/resolve.rs` 做继承链/字段布局/方法表解析
  （同一编译单元从 AST 取，否则从 classpath 读 `.class`）
- **`super(...)` / `super.m()`**：parser 产出 `Call{recv:Super}`，codegen 发 `invokespecial` 到父类；
  构造器隐式 `super()` 自动补，父类无无参构造器则报错（与 javac 一致）
- **字段与方法继承**：父类字段在前、子类在后；子类没有的方法落到父类实现
- **虚分派**：解释器按运行时类沿链解析；JIT 用 `(类标签, 签名) → 实现` 的方法表 `vcall`
- **`instanceof`/`checkcast`/`catch` 匹配**：改成沿继承链判定（`is_subclass`）
- **跨编译单元**：`compile_with_path(cu, dirs)` 与 `jit.load_class_path()`，输出目录即 classpath
- 修两个既有 bug：`expr_kind` 不查字段表导致 double 运算插 `i2d` → VerifyError；
  隐式 `this` 字段 `getfield` 不记栈深 → `max_stack` 少 1

**验证：42 个样例三路一致**（新增 `InhBasic InhOverride InhChain InhSuperCall InhImplicitSuper
InhInstanceof InhWide` + 跨单元 `InhUnit`）；`cargo test` 29 passed。

**已知不支持**：接口（`implements` 仍忽略）、`this(...)` 委托、带包名的继承（现已明确报错）。

## 0.38.0 — 2026-11-17 · 接口、包名、this(...) 委托、多维数组、静态字段

- **接口**：`implements`/接口继承写进 class 文件、`invokeinterface`(0xB9) 用 InterfaceMethodref、
  抽象方法无 Code、`instanceof` 接口按实现闭包判定、接口常量
- **包名**：新增 `sema/qualify.rs`，进 codegen 前把同包简单名补成 `p/Name`；
  `compile_src` 按内部名建目录写盘（`out/pkg/C.class`）；`preload()` 预读 classpath
- **`this(...)`**：本类构造器委托 → `invokespecial` 到另一个 `<init>`
- **多维数组**：`multianewarray`(0xC5)、`new int[3][]` 锯齿、二维/三维读写、JIT 侧 `new_multi_array`
- **静态字段 + `<clinit>`**：`getstatic/putstatic`（含继承）、静态初始化块按源码顺序、
  首次使用触发且只执行一次（先父类）；JIT 侧懒初始化
- 修作用域 bug：循环内声明变量未 chop → `top is not assignable to integer`（javac 同样 chop）

**验证：50 个样例三路一致**（新增 `Iface Iface2 CtorThis Multi Multi2 Multi3 Static2` + 包名 `Pkg`）；
`cargo test` 31 passed。

**已知不支持**：接口 default/static 方法、`import`（跨包需写全限定名）、
`println(a == b)` 这类「比较结果当实参」（改动前就存在，栈图限制）。

## 0.39.0 — 2026-11-17 · JIT 分配接进分代 GC（影子栈 + 根集 + 写屏障）

JIT 原来分配走裸 bump 区，对象**永不回收**；现在全部走 `librustjrt` 的分代堆。

- **分配**：`new_object`/`new_array`/`new_multi_array` → `tlab_alloc_object`；
  TLAB 满先 refill、eden 满才 collect、超大走 LOS
- **布局注册**：每个 Java 类懒注册进 `LayoutTable`（每字段占值+填充两槽，引用字段进 GC 位图），
  `class_id` 与 JIT tag 双向映射，对象头存 `class_id`
- **写屏障**：`putfield`/`aastore` 存引用走卡表；引用型静态字段是永久根
- **影子栈**：新增 `jit/refmap.rs`（每个 pc 的「操作数栈哪一位是引用」）；
  每帧根区住引用型局部变量，GC 点（new/newarray/invoke*/`<clinit>`）push/pop 根；
  `roots_pop` 带平衡断言，不平衡直接报错而不是悄悄截断根
- **零回归**：`verify.sh` **50/50 三路一致**（`VMLESS_HEAP_MB=8` 强制回收再跑一遍同样 50/0）；
  `cargo test` 31 passed

**实测（`bench_jit_gc`，50 万次短命对象分配）**
```
ALLOC_NS=188.4  GC_COUNT=6  LIVE_AFTER=4026528  HEAP_MB=3.84
```
分配量翻四倍（4M）堆占用**一模一样**（3.84 MB）—— 旧实现按源码推算 16M 次要 245 MB 且只增不减。

`librustjrt` 新增只读访问器 `Heap::base()`。

## 0.40.0 — 2026-11-17 · 比较值化、import、枚举、接口 default/static

- **比较结果当实参**：新增脏槽池（`Emit::alloc_scratch`，位图式不假设 LIFO）+ `needs_scratch`/`spill`，
  会分叉的子表达式先算进脏槽 → 跳转点永远空栈。`println(a==b)`、`boolean f = x>y`、
  `a == b && c != d`、`!x`、短路求值成 0/1 全部可用
- **`import`**：解析顺序「本单元声明 → 单类型导入 → 同包 → 通配导入」；
  类加载改递归，带包名的主类从包根加载
- **枚举**：`sema/enums.rs` 把 `enum` 降成普通类（静态 final 常量 + `$name/$ordinal` +
  `name/ordinal/toString/values/valueOf`）；`switch (enum)` 按 ordinal 比较；支持带字段与实参
- **接口 default/static 方法**：接口方法带体就编出 Code；default 内部调用发 `invokeinterface`、
  接口静态方法用 InterfaceMethodref；方法解析在类链找不到时回落接口 default
- 顺带补：`break`/`continue`、方法末尾按返回类型默认 `xreturn`、
  `boolean/char/byte/short` 数组的 `ba*/ca*/sa*` 指令

**验证：60 个样例三路一致**（新增 `Cmp1-3 Enum1-3 IfaceDef1-2` + 多单元 `Import1/2`）；`cargo test` 34 passed。

**已知不支持**：嵌套枚举（明确报错）、带标签 break/continue、`?:`、for-each、`import static`。

## 0.41.0 — 2026-11-17 · for-each、三元、标签跳转、嵌套类型、字符串拼接

- **for-each**：降糖成已有语句形态（数组走下标循环，`Iterable` 走 `iterator()/hasNext()/next()`）
- **三元 `?:`**：分叉 + 脏槽汇合（跳转点栈恒空）；顺带修了**栈深记账 bug**
  （按参数个数弹而不是槽数，`f(true,7,10L)` 之后深度永久少 1）
- **带标签 `break`/`continue`**：`Stmt::Labeled` + `JumpTarget` 标签栈
- **嵌套类型**：新增 `sema/nested.rs`，`Outer.Inner` → `Outer$Inner` 拍平（在枚举降级之前跑，
  嵌套枚举因此复用同一条 values/valueOf 路径）
- **字符串拼接**：走真 `java/lang/StringBuilder`（new/append/toString）；
  解释器新增 `vm/rt.rs`，JIT 侧 `sb_new/sb_append*/to_string`（缓冲在对象里，GC 搬动安全）

**验证：74 个样例三路一致**（新增 `ForEach1-3 Ternary1-2 Label1-2 Nest1-3 Concat1-3 Mix2`）；
`cargo test` 34 passed；GC 压测 `VMLESS_HEAP_MB=2` 下 20 万次拼接 `minor=1 major=1 回收 201319 对象`。

**已知不支持**：非 static 内部类（`this$0`）、`do-while`、跨单元的嵌套类型引用、
拼接非 String 的 Object（JVM 打 `Klass@hash`，我们没有 hashCode）。

## 0.42.0 — 2026-11-17 · 泛型擦除、变参、do-while、switch(String)、多catch、TWR

- **泛型擦除**：新增 `sema/erase.rs`（`type_params` 改成 `TypeParam{name,bound}`，上界进描述符）；
  调用点按声明取构造器/方法描述符；擦除后**生成桥接方法**（`Holder<String>.value()` 能调到 `String value()`）；
  `coerce_to` 在源静态类型是 `Object` 时插 `checkcast`
- **可变参数**：`T...` 的声明类型即 `T[]`；`emit_call_args`/`pack_tail` 在 8 个调用点统一打包；
  `main(String... args)` 正常当入口
- **do-while**：continue 落点是条件判断处
- **switch on String**：`equals` 链（语义同 javac）；顺带修箭头 `case ->` 不贯穿
- **多重 catch**：`Catch{types: Vec<Type>}`，同一 handler 多条异常表项
- **try-with-resources**：糖化成 try/finally + 反序 `close()`（原来 `skip_balanced` 直接丢弃资源，静默不 close）
- 修 If 的 then 分支已终结时仍发 goto 的不可达代码（HotSpot `Expecting a stack map frame`）

**验证：87 个样例三路一致**（新增 `Gen1-5 Vararg1-4 DoWhile1 SwitchStr1 MultiCatch1 Twr1`）；`cargo test` 38 passed。

**已知边界**：装箱/拆箱未做（`int n = box.get()` 会 VerifyError）、`new T[n]` 不支持、
有界类型参数的类（`Number`）运行时没有内建、多重 catch 变量静态类型统一 `Throwable`。

## 0.43.0 — 2026-11-17 · 装箱拆箱、JDK 内建、内存层生产级化

### 装箱 / 拆箱 + JDK 内建（rustjavac）
- **新增 `src/jdk.rs`**：String / 5 个包装类 / Object / System / Arrays 的方法与字段表，
  编译器与两个运行时**共用同一张表**（op 号 = 下标）
- **自动装箱拆箱**：`coerce_to` 扩成「数值拓宽 + 擦除后 checkcast + 自动装箱/拆箱」，
  `int n = box.get()`（Object→int）编成 `checkcast Integer` + `intValue`（原来是 `istore` 一个引用 → VerifyError）
- **String 15 个方法**（length/charAt/substring/indexOf/contains/startsWith/toUpperCase/trim/replace/
  split/compareTo/isEmpty/concat/hashCode…）、包装类 `valueOf/xxxValue/parseXxx/MAX_VALUE`、
  `Object` 的 `hashCode/equals/toString`、`Arrays.toString/sort`、`System.arraycopy/currentTimeMillis`
- Integer 小整数缓存（-128..127）与 Boolean 单例照 Java 语义实现

### 内存层生产级化（librustjrt 0.2.0）
- **压缩式老年代**：mark-compact 滑动压缩 + 重映射表；新增 `HandleTable`（压缩安全句柄）
- **增量标记**：`GcPhase` 状态机 + `step(budget)`，SATB + 增量更新双屏障
- **并发分配**：`SharedHeap` + `ThreadLocalTlab`（CAS 取块，块内纯 bump）
- **弱引用 / finalization 队列**
- 压力测试 200 轮随机图（随机增删点/边 + 6 种 GC 轮转），逐对象校验
- 修两个真 bug：压缩漏改写年轻对象里的老年代引用（悬垂）、不压缩的 major 之后句柄不失效

### 修掉的既有 bug（重要）
- **`!=` 一直被词法器当成 `==`**（`lexer/mod.rs:221` 发的是 `Tok::EqEq`）。三路跑同一份字节码所以一直"绿"，
  但语义一直是错的 —— 这条影响所有既有代码
- `CharLit` 完全不支持；`println(char)` 按 `(I)V` 打码点

**验证**：`rustjavac` 94 个样例三路一致、单测 42；`librustjrt` 单测 51 + doc-test 2。
**基准**：压缩后碎片率 0.500→0.000、跨度 15.3MB→7.6MB；增量最大单步 **0.683 ms**（全停顿 26.9 ms，≈39×）。

## 0.44.0 — 2026-11-17 · jimage / jar 类来源、ClassLoader 骨架、float

### jimage / jar / 类路径（「跑真实 Java 程序」的地基）
- 新增 `src/load/`：`ClassSource` 三态（目录 / jar / **jimage 真二进制格式**），统一 `find(内部名)`
- **zip 自己解**（中央目录 + 本地头 + stored/**deflate 自己写的 inflate**，零依赖）；
  `src.zip` 52MB / 15058 条目全量解出与 `jar xf` 零差异
- `jimage` 解真格式：30482 条索引，读 `java/lang/Object`/`String`/`HashMap`/`Unsafe` 的结构
  **与 `javap -v` 逐项相等**，字节与 `jimage extract` 一致
- `vm/load.rs`：从主类出发沿常量池 Class 项建引用闭包按需加载；`define_class` 运行期加类
- `rustjava -cp <a.jar;b.jar;dir> <主类>` / `-jar <jar>`（读 MANIFEST `Main-Class`）

### float
- 字面量、`fconst_*`、`f*` 算术、`fcmpl/fcmpg`、转换、`freturn`、`println(float)`、栈图 `ITEM_Float`

**验证**：`verify.sh` **100/100 三路一致**；`cargo test` 44 passed。

**已知边界**：jimage 的压缩条目（`jlink --compress`）明确报错（本机 JDK 的 30482 条 compressed=0，不受影响）；
zip64/加密不支持；`-jar` 暂不支持给 main 传参；**`java.*` 故意不进执行闭包**（真字节码会盖掉内建实现）。

## 0.44.1 — 2026-11-17 · Minecraft 缺口量化清单

新增 `docs/MINECRAFT-需要什么.md`（分析文档，不含实现）：

- **MC 本体 0 个 native、0 处 `java.lang.foreign`**；native 债务全在
  **`java.base` 自己（748 个 native / 107 个类）** 和 **LWJGL（~5,345 个 JNI 入口）** 上
- **`invokedynamic` 命中 5,665/11,383 个 MC 类**；`tableswitch` 321 类、`fconst_*` 942 类、`monitorenter` 22 类
- **FFM 替代 JNI 已证伪**：`GL11C` 仍是 62 个 `native`，opengl 模块 2,278 个 native、`java/lang/foreign` 引用 0 处
- P0 四块（反射 / `java.lang.invoke` / JNI / 线程 + 类加载模块）合计 **2.0–2.8 万行 / 32–48 人周**
- 推荐路线：补 9 条指令 → **先跑服务端**（7,762 类、零 LWJGL）→ 客户端再上 JNI+GL
