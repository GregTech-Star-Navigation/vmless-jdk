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

## 0.45.0 — 2026-11-17 · float 全链路、switch 表、数组初始化、栈指令

- **float 全链路**：`K_FLOAT`/`ITEM_Float`、`fconst_*`、`fadd/fsub/fmul/fdiv/frem/fneg`、
  `fcmpl/fcmpg`、`i2f/l2f/f2i/f2l/f2d/d2f`、`freturn`、`println(F)`、StringBuilder `append(F)`；
  解释器新增 `Slot::Fl(f32)`（float 与 double **必须分开**：槽宽不同，混用会让 `pop2`/局部变量落错槽）；
  JIT 用 Cranelift `F32`，字段/数组 4 字节（原来 `[F` 分配 4 字节按 8 字节访存）
- **float/double 打印**：算法从「Rust `{:e}`」改成「最小位数 + 正确舍入 + subnormal 多留一位」，
  与 JDK 25 **逐字符比对 160 万个位型，0 处不同**（旧实现在 60 万位型里差 1184 个）；
  顺手修 `Float/Double.MIN_VALUE` 取成最小正规数的老 bug
- **switch 表**：`≥6` 分支且空槽 ≤ 一半 → `tableswitch`，否则 `lookupswitch`，小 switch 仍用比较链
- **数组初始化**：`int[] a = {1,2,3}` 与 `new int[]{…}`（声明处与表达式两种来源）
- **栈指令**：`dup2/swap/dup_x1/dup_x2/dup2_x1/dup2_x2`、`i2b/i2c/i2s`、解释器 `iinc`
  —— 抓出一个真 bug：`dup2_x2` 形式 3（栈顶宽值）三处都判错

**opcode 覆盖：解释器 194/202、JIT 194/202**（基线 177/157）。剩 8 条：
`jsr ret wide goto_w jsr_w`（本编译器不产）、`invokedynamic`、`monitorenter/monitorexit`。

**验证**：`verify.sh` **103/103 三路一致**；`cargo test` 44 passed。

## 0.46.0 — 2026-11-17 · native 方法机制 + **真的执行 OpenJDK 的 java.base 字节码**

这是从「玩具 VM」到「真 VM」的分水岭。

- **native 机制**：`ACC_NATIVE` 方法查注册表（`(类,名,描述符) → fn(&mut Vm,&mut Frame)`），
  未命中报 `UnsatisfiedLinkError: <类>.<名><描述符> not registered` 并累积缺项清单；
  已注册 **102 条**（Object 协议 / Class / System / Unsafe 64 条 / String.intern / Throwable / 浮点位转换 / VM / CDS）
- **真字节码执行**：`VMLESS_JDK=<lib/modules|解包目录>` 把 java.base 挂到 classpath 末尾；
  分派顺序改为 `VMLESS_PREFER_BUILTIN` → **真字节码** → 内建替身（仅当类无字节码）→ 报错
- **`ldc Class`**：类对象是真对象（运行时类 `java/lang/Class`，`getName`/`isInstance` 可用），同类同句柄

### 里程碑（javac 25 编，输出与 HotSpot 逐字节一致）
| | 内容 | 结果 | 新增 native |
|---|---|---|---|
| **M1** | `new java.lang.Object()` | 7/7 | ~11 |
| **M2** | `String.<init>(char[])`、`length`、`charAt`、`hashCode`、`equals`、`substring` | **9/9** | ~20 |
| **M3** | `ArrayList add/get/size/remove` | 10/10 | **0** |
| **M4** | `HashMap put/get/size` | 8/8 | **0** |
| **M5** | `ldc Class` | 12/12 | 0 |

实测 M2：
```
ours: 5 h o hello true 99162322 99162322 false el   缺 native: 0
java: 5 h o hello true 99162322 99162322 false el
```

新增 `docs/java-base-native-清单.md`（231 行，按里程碑分组的 native 清单）。
**验证**：`verify.sh` 103/103；`cargo test` 44 passed。

**已知缺口**：`invokedynamic`(0xba) 解释器未实现（lambda 与字符串拼接死在第一处）、
非 ASCII 字符串（`JavaLangAccess` 由 JDK 启动流程注册，我们缺那段）。

## 0.47.0 — 2026-11-17 · 密码学栈 + jarsigner / keytool / ktab（生产级）

新增 `jdk-crypto/`（24 个文件 / 7959 行，**零外部依赖**）：

`SHA-1/256/512` + `MD5` + `HMAC` · 自写 `BigUint`（加/乘/幂模/求逆）· `RSA 2048/3072` +
PKCS#1 v1.5 · `ASN.1 DER` · `X.509`（解析 + 自签生成）· `PKCS#7 SignedData` ·
`PKCS#12`（PBES2/PBKDF2 + AES-CBC）· ZIP 读写 · JAR 签名/验签

**硬判据是与真 JDK 双向互通**（实测）：
| 项 | 结果 |
|---|---|
| 我们签的 jar → 真 `jarsigner -verify` | `jar verified.` exit 0 |
| 真 `jarsigner` 签的 jar → 我们验 | exit 0（SHA-384/256/1 三种都过）|
| 我们生成的证书 → 真 `keytool -printcert` | Owner/Serial/有效期/SHA384withRSA/2048-bit/v3 全对 |
| PKCS#12 ↔ 真 keytool | 双向：真 jarsigner 能直接用我们的 `.p12` 签名 |
| keytab ↔ 真 `klist -k -e` | 2 entries，KVNO 3/1，type 18/17 |

**生产级指标**
- **fuzz 50 万例（DER/X.509/PKCS#12 各 10 万 + 结构化变异 20 万），0 panic**
- 常数时间比较用于 HMAC / PKCS#7 摘要 / PKCS#12 MAC / JAR 条目摘要；
  PKCS#1 v1.5 验签严格按 RFC 8017 §8.2.2 检查 `00 01 FF..FF 00`（不是只比尾部哈希）
- KAT 来自 FIPS 180-4 / RFC 1321 / 2202 / 4231 / 6070 / 7914 / FIPS-197 / SP800-38A / 7292
- 性能：SHA-256 **144 MB/s**、RSA-2048 签名 15.2ms / 验签 3.6ms、PKCS#12 加载 75.6ms

**已知限制**：无 TSA 时间戳、仅 RSA 签名块、单签名者、`.p12` 仅 PBES2/AES、不支持 ZIP64。

**踩到两个硬坑**：ZIP 的 data-descriptor 位（bit 3）不清会让 JDK `ZipFile` 报
`invalid entry size`；JDK 的 keytab 格式带 4 字节条目长度前缀，按 MIT 文档写会被 JDK `klist` 拒绝。

## 0.48.0 — 2026-11-17 · JDK 工具集 15 个真实现（+17 个能力缺口桩）

新增 `jdk-tools/`，**32 个 exe 全部实测可跑**：

| 工具 | 做了什么 | 实测 |
|---|---|---|
| **jimage** | list / info / extract / verify | 全量 **30482 条**；extract 与真 jimage **逐字节相同**；`verify` 0 问题；145MB 索引 4.27s、峰值 RSS 16.1MB |
| **jmod** | create / extract / list / describe / hash | `--compress zip-0/zip-6` → 30.9MB / 16.6MB；真 JDK 能 list/describe/extract 我们的 jmod |
| **serialver** | 按 `ObjectStreamClass` 规范算 | 20 个 `java.*` 类与真 serialver **全一致**（`String` = `-6849794470754667710L`）|
| **jnativescan** | 扫 `ACC_NATIVE` | `java.base` = **748**（与独立分析吻合）|
| **jdeprscan** | 内置表 + 输入自身 `@Deprecated` | 报出 `Thread::stop()V (forRemoval=true)` |
| **javaw** | GUI 子系统 exe，写 jps 登记 | 跑通 jar/class，jps 可见 |
| **jwebserver** | HTTP/1.1、Range、304、keep-alive | curl 逐项验过 |
| **jlink** | `module-info` 的 `requires` **传递闭包** + strip-debug | 镜像里跑出真类 |
| **jshell** | 编译 + VM，变量保留/命令 | `1+2`→`3`；`var x="hi"; x.length()`→`2` |
| **javadoc** | 类/方法/字段三张表 + 索引 + 交叉链接 | 4 个 HTML，`@author`/`-private` 生效 |
| **jpackage** | `--type app-image` 真做 | launcher 跑出 `packaged hi` |
| **jrunscript** | 自写极简 JS（变量/if/while/for/函数） | `2` / `5050` |
| **rmiregistry** | TCP 注册表（bind/lookup/list） | 8 条命令全对 |
| **jps** | 自建进程登记 + 存活剔除 | 列出运行中的 javaw |
| **jdb** | 记录-回放式调试器 | 断点停在第 6 行，`print`/`next`/`cont` 值随循环变化 |

**生产级**：20 组畸形输入全过（不 panic、非 0 退出码）；修了 EPIPE（`jmod list | head` 之前 panic）；
`jimage --help` 与真 JDK 逐行相同；`serialver`/`jimage list`/`jmod describe` 输出与真 JDK 逐行一致。

**17 个桩**（按能力归属分组，每个都打印「缺什么 + 归属 + 现有替代」并 exit 1）：
crypto(1) / kerberos(2) / attach+JFR(8) / GUI 无障碍(4) / 其它(2)。
其中 `jarsigner`/`keytool`/`ktab` 已由 `jdk-crypto` 提供真实现。

**已知边界**：`jlink` 写的是目录形式的 `lib/modules`（不写 jimage 容器，`--compress` 无效）；
`jdb` 是记录-回放（`Vm` 未暴露暂停原语）；`javadoc` 无 doclet 体系；`rmiregistry` 是行协议不是 JRMP。

## 0.49.0 — 2026-11-17 · attach 协议 + 性能计数器（jps/jcmd/jinfo/jmap/jstack/jstat/jstatd）

新增 `jdk-attach/`（14 个模块 + 9 个 bin，8239 行，零依赖，42 单测）。

**关键决策：perfdata 用 HotSpot v2.0 的二进制格式**（`%TEMP%\hsperfdata_<用户>\<pid>`），
所以**真 JDK 的 `jps`/`jstat` 能直接读我们的 VM**。控制通道走回环 TCP + 8 字节头帧
（版本参与握手、1MiB 分块、128 位令牌）；端点信息写成 perfdata 的两个计数器，不留第二个文件。

**实测**
| 项 | 结果 |
|---|---|
| `jps -l` | `40696 com.example.Main`，与真 jps 同秒一致；`-q/-m/-v` 全对 |
| `jstat -gc` | **19 列表头逐字符相同、行宽同为 200**；`-gcutil/-class/-compiler` 亦一致 |
| `jcmd` | 14 条命令；`VM.version`→`VMless Server VM version 0.1.0`；`PerfCounter.print` 119 行 |
| `jmap -histo` | 表头与真 jmap 一致；`-dump` 20340 字节，`hprofcheck` 结构合法 |
| `jstack -l` | `Found one Java-level deadlock:` + 两条 `which is held by` |
| 错误矩阵 | pid 不存在=2 / 坏 hostid=1 / 错令牌=3 / 协议版本不符=3 / 半帧超时=4 / 改不了的标志=6 |
| **泄漏** | 1000 次 attach/detach 句柄数 **73 → 73（变化 0）** |

**踩到的 5 个坑**（写进 `docs/attach-design.md`）：HPROF 魔数大端、Windows 必须用**具名共享内存**
`hsperfdata_<user>_<pid>` 才能被真工具读、字符串条目 `vector_length` 必须 >0、
`generation.0.space.0` 是 Eden、**Windows accept 出来的 socket 继承非阻塞**（表现为"加了日志就好了"的竞态）。

**已知边界**：`jmap -dump` 的 `CLASS_DUMP` 常量池/静态字段段与 JDK 25 实际编码不完全一致
（"真 MAT 能打开"本机无法证明，`jhat` 在 JDK 9 已删除）；`jstatd` 用同语义 TCP 而非 JRMP。

## 0.50.0 — 2026-11-17 · Kerberos v5（kinit / klist / kdc）+ 修 ktab 的 string2key

新增 `jdk-krb/`（6463 行，41 单测）：

- **ASN.1**：`[APPLICATION n]` + EXPLICIT TAGS；`Ticket`/`KdcReq`/`KdcRep`/`EncKdcRepPart`/`EncTicketPart`/`ApReq`/`Authenticator`/`KrbError`
- **加密**：RFC 3961 `n-fold` / `DR`、RFC 3962 `aes256-cts-hmac-sha1-96`、string2key、
  CTS(CS3)、confounder+HMAC、checksum（向量双源：RFC 3962 附录 B + JDK 25 实测）
- **ccache**：MIT `FILE:` `0x0504` 格式，**JDK 的 `klist` 能读**
- **KDC**：AS / TGS、UDP+TCP 同端口、keytab 自动识别 JDK 与 MIT 两种格式

**互通验收 14/14**，硬判据全部达成（我亲自复现）：
```
我们的 ktab 写 keytab → 我们的 KDC 载入 → 标准口令客户端 kinit 拿到 TGT（19ms）
同一 keytab → 真 JDK 25 的 kinit 登进我们的 KDC → JDK klist 显示
  Default principal: testuser@VMKRB.LOCAL, 1 entry found.
  [1] Service Principal: krbtgt/VMKRB.LOCAL@VMKRB.LOCAL
```
错误路径与真 JDK 同码：错口令 `Pre-authentication information was invalid (24)`、
未知 principal `Client not found in Kerberos database (6)`、时钟偏移 `KRB_AP_ERR_SKEW(37)`

**顺带修了 `jdk-crypto` 的一个真 bug**：`ktab` 的 `string_to_key_aes` 只做了 PBKDF2，
缺 RFC 3962 §4 的 `DK(tkey, "kerberos")` 这一步 —— 于是同一口令派生的密钥与 JDK/MIT 不同，
外人用标准口令登不进以该 keytab 建库的 KDC。已补 `cipher::krb_nfold` / `krb_dk` 并接上。

**生产级**：600000 例 fuzz（kdc-req / as-rep / ccache）**0 panic**；AS 交换 19–26ms。

**已知限制**：仅 aes128/aes256-cts；无 rc4/des3/AES-SHA2、无跨 realm referral、无 replay cache、无 PAC。

## 0.51.0 — 2026-11-17 · invokedynamic + MethodHandles（解释器侧生产级）

**这是「能跑现代 Java 字节码」的最后一块字节码层拼图。**

- **常量池**：解析 `BootstrapMethods` 属性；`ldc` 支持 `MethodType`(16) / `MethodHandle`(15) / `Dynamic`(17)
- **执行**：`0xba` 按调用点描述符弹参（不看 bsm 描述符）；**调用点缓存** `(类,常量池索引) → 目标句柄`；
  `asType` 语义（宽化/装箱/拆箱/引用；`invokeExact` 完全一致、`asType` 窄化当场抛 `WrongMethodTypeException`）
- **`java.lang.invoke.*`**：`Lookup.find*`、`MethodHandle.invoke*/asType/bindTo/insertArguments/constant/identity`、
  `CallSite`/`MutableCallSite`、`VarHandle`（get/set 全族 + `compareAndSet`/`getAndSet`/`getAndAdd`）
- **bsm 子集**：`StringConcatFactory.makeConcatWithConstants`（`\u0001`/`\u0002` 规范语义 + 常量折叠）、
  **`LambdaMetafactory.metafactory` 与 `altMetafactory`**（捕获/方法引用/bridge/marker/`FLAG_SERIALIZABLE`）、
  `ConstantBootstraps`

**实测（11 个样例 × 四路逐字节比对：HotSpot / 真字节码 / 独立模式 / JIT 降级）**
```
ours: a1b2.5 / n=42, s=a1b2.5 / big=1234567890123 z=true c=x
java: a1b2.5 / n=42, s=a1b2.5 / big=1234567890123 z=true c=x
```
`I1`–`I11` 全一致（含 record 模式匹配、switch 表达式、10 万次循环拼接、错误路径的异常类名）。
**调用点缓存**：`调用点解析 2 次，执行 100001 次，lambda 实例 1 个，缓存命中 99999 次`。

**JIT 侧明确拒绝**：`0xba` 在**任何分析之前**预扫并报「JIT 不支持 invokedynamic」，
`run_jit` 命中就整体退回解释器并在 stderr 明说，stdout 与解释器逐字节一致 —— 不假装支持。

**修了 3 个既有 bug**：虚分派没给字符串句柄/数组的运行时类（`String.contains` 静默返回 false）、
无目录 class 路径报「classpath 为空」、refmap 报错的 off-by-one。

**验证**：`verify.sh` 103/103；`cargo test` 50 passed。

**已知缺口**：`SwitchBootstraps.typeSwitch/enumSwitch`（`case null ->`）、
`ObjectMethods.bootstrap`（record 的 equals/hashCode/toString）、`Lookup.unreflect*`。

## 0.52.0 — 2026-11-17 · 合并 java.base 编译成果与 JDK API 面

两条分支（都基于 pre-float 的旧代码）合并进主干，**两边能力都不丢**。

**并入 ①：让 javac 编 java.base**
`i++`/`--n`/`x += e`、**实例字段初始化**（之前 `class W { long f = 5; }` 根本没编，读出来 0）、
静态字段 `i2l`、十六进制/二进制/八进制/下划线字面量、`(int) -3.99` 的 cast 解析、
隐式 `import java.lang.*`、空语句、`if ((n=next())>=0)` 赋值表达式、
classpath 世界快照进程级缓存 + 包扫描深度 4→12 + Windows 只按 `;` 切

**并入 ②：JDK API 面**
`jdk.rs` 追加 190 条（Throwable 家族、ArrayList/HashMap/Iterator、String.format/valueOf、
Math、Integer/Long 进制、Character、Arrays）；解释器与 JIT 侧的集合/映射/格式化实现；
`jit/translate.rs` 的 **`pc == 0` 映射到 entry block** 修复（任何「方法体第一条就是 while」都会崩的 JIT bug）

**合并冲突的取舍**（都留了更正确的那侧）：
`fastore` 取主干的 F32（jbcov 是 pre-float 权宜）· `refmap` 的 `dup2_x2` 保主干（被 160 万位型 + indy 套件验证过）·
`is_jdk_runtime` 与 `jdk_present` 保主干的后者（严格更强）· 数组初始化把两边语义合并（声明类型优先 + 无声明时取最宽）

**实测**
| 项 | 合并前 | 合并后 |
|---|---|---|
| `verify.sh` | 103/103 | **103/103** |
| **java.base 编译通过率** | 20.4%（无 cp）/ 40.9% | **1254/3058 = 41.0%** |
| corpus 三路一致 | 35/45 | **40/45** |
| corpus/napi | — | **7 一致 + 1 两路，失败 0** |
| 真 java.base M2 | 一致 | 一致 |
| `cargo test` | 50 | 50 |

## 0.53.0 — 2026-11-17 · jconsole / jaccess* / jabswitch（Win32 原生 GUI，不依赖 java.desktop）

我们没有实现 Swing/AWT，所以这四个工具用 **Win32 原生 GUI（Rust 手写 `extern "system"`）+ attach 协议**。
这是刻意的取舍：功能等价、体积小、不必先造一套 GUI 工具包。

新增 `jdk-gui/`（5729 行，依赖只有 `vmless-attach`）：
`win.rs` Win32 FFI（含 `oleacc`/`advapi32`）· `ui.rs` 窗口类/消息循环/双缓冲自绘 ·
`vm.rs` 采样线程 + 断线重连 · `msaa.rs` **手写 `IAccessible` vtable（28 槽）** · `pick.rs` 进程选择 · `reg.rs`

**实测**
| 项 | 结果 |
|---|---|
| `jconsole -p <pid>` | 窗口标题 `jconsole —— VMless 监控  pid 15072`；`EnumChildWindows` 数到 **18 个控件**（SysTabControl32 + 自绘页 + SysListView32 + SysHeader32）|
| 数据实时 | `采样 #4 · 堆 25 KB / 104.0 MB · 线程 8 · 类 10` → 2s 后 `采样 #6 · 堆 30 KB …` |
| 内存页 19 列 | 与真 `jstat -gc` 同一目标**逐列一致** |
| `jaccesswalker` | `共 479 个窗口 / 65 个进程`，按进程分组打印类名/标题/可见性/尺寸 |
| `jabswitch` | `-q`→禁用 → `-enable` → `-q`→启用 → `-disable` → `-q`→禁用；`reg query` 独立核对 `JavaAccessBridge REG_SZ 1/0` 同步 |
| 错误路径 | 坏 pid → exit **2**；坏参数 → exit **1** |
| 断线 | 杀掉目标后状态栏 `连接断开… 每 3 秒重试`，窗口不崩 |
| 体积 | jconsole 298KB · jaccessinspector 166KB · jaccesswalker 171KB · jabswitch 126KB |

**踩到的两个硬坑**（写进 `AGENTS.md`）：`wide("")` 的 `as_mut_ptr()` 是悬挂非空指针
（comctl32 按 NUL 读 → 插列表项直接访问违例）；`SysTabControl32` 在 `TCM_INSERTITEM` 后会把自己提到 Z 序最前，
页面窗口必须显式 `bring_to_top`，否则「WM_PAINT 跑了、BitBlt 返回 1、屏幕上却是标签控件的灰底」。

**边界**：无障碍真用了 `oleacc` + 手写 `IAccessible`（不是退回 Win32），但老式窗口的 OLEACC 代理不给
每个 child 的属性，所以子节点改成逐子窗口探 MSAA；无 comctl32 v6 manifest（v5 外观、不跟随深色模式）。

## 0.54.0 — 2026-11-17 · JFR（`.jfr` 格式读写）+ jhsdb

新增 `jdk-jfr/`（7123 行，依赖只有 `vmless-attach`）：
`format/{chunk,metadata,cp,event}` 二进制格式 · `reader` · `writer` · `recorder` · `model`
· `bin/{jfr,jfrfuzz,jhsdb}`

**双向都通（这是硬判据）**
| 项 | 结果 |
|---|---|
| 我们的 `jfr summary` vs 真 `jfr summary`（同一个真 JDK 录的文件）| **204 行 `cmp` 逐字节相同** |
| 我们的 `jfr print --events jdk.ExecutionSample` | **778 行 `cmp` 无差异** |
| **我们的 `jfr record` 写出的文件 → 真 `jfr summary`** | 读了：`jdk.ExecutionSample 34 / GCPhasePause 3 / JavaMonitorEnter 2 / ThreadEnd 1` |
| **同一文件 → 真 `jfr print`** | `sampledThread = "worker-0" (javaThreadId = 42)`，栈 `demo.Busy.work(long, String) line: 42` |
| `jfr record --pid <pid>` | 真采我们的 VM（`Thread.print`→ExecutionSample，perfdata→GCPhasePause）|
| 54.3MB / 5 chunk / 302 万事件 | `summary` 0.211s、峰值 RSS 59.6MB |
| fuzz 5 万例 | **0 panic**，11.65s |

**踩过的格式坑**（写进 `README`「格式要点」）：常量池链是**倒着**链的（delta 为负）·
`byte` 是裸字节不是 varint · `int` 要按 32 位截断 · **key 0 可能是真值** ·
`@Timespan` 有 5 种单位 · `@ContentType` 缺时间戳会打成 ISO-8601 ·
**`StackTrace.frames` 不能进常量池**（否则真 jfr 抛 ClassCastException）· `chunkSize` 必须等于文件长度 ·
真 jfr 的 summary 表头与表格行**行分隔符不同**

`jhsdb` 只服务 VMless VM（Windows 无 ptrace），提供 `jmap --histo` / `--heap` / `jstack` / `jinfo` 等价能力。

**边界**：全量 `print` 3.3M 事件占 2.6GB（未逐 chunk flush）；`--xml` 未实现；多 chunk 按结束时间统一出队。

## 0.55.0 — 2026-11-17 · 线程 + `monitorenter`/`monitorexit` + 真同步

**这是「Minecraft 起不来」的首要原因，也是 195/202 里最后两条字节码缺口。**

新增 `src/vm/thread.rs`（1091 行）：GIL + 线程表 + 监视器
- **票据锁（FIFO）** 做 GIL —— 非公平锁会让 safepoint 上「释放后重抢」把别的线程饿死
- `monitor_enter/exit/wait/notify`：**可重入、FIFO、归零移交**（防插队）、下溢抛 `IllegalMonitorStateException`、
  `wait` 完整释放并还原重入计数
- 每 Java 线程一个 OS 线程；`ACC_SYNCHRONIZED` 放在**每个调用点都必经的那一层**（异常也解锁）
- `safepoint`：每 512 字节码交还一次 GIL；单线程零成本
- `<clinit>` 按 JVMS 5.5 **串行化** —— 多线程下不串行会读到半初始化的静态字段（最隐蔽的一个 bug）
- `debug_threads` / `debug_deadlocks`：锁图 + `jstack` 风格死锁报告

**JIT 侧明确拒绝**：`synchronized`/`monitorenter`/`monitorexit` 一律报错并整体退回解释器，
`run_jit` 的降级判据已含这三条（**不许静默不加锁**）。

**实测（16 个用例 × 3 轮，与 JDK 25 逐字节一致）**
| 用例 | 结果 |
|---|---|
| T1 4 线程 × 10 万 `synchronized` 计数 | `400000 true` |
| T3 `wait`/`notifyAll` 1 万元素 | `0 0`（无丢失无重复）|
| T5 线程状态 | `TIMED_WAITING / BLOCKED / RUNNABLE / TERMINATED` |
| T6/T6b daemon 与非 daemon | `true main done` / `false main done worker finished` |
| T7 `volatile` 可见性 100 万次 | `saw ready 42 1000000 true` |
| T4 故意死锁 | 报 `Found 1 deadlock.`，环与谁等谁一致 |

**opcode 覆盖：解释器 197/202**（`monitorenter`/`monitorexit` 转 ✅），JIT 195/202
（监视器不实现，明确拒绝）。只剩 `jsr ret wide goto_w jsr_w`（本编译器不产）。

**回归**：`verify.sh` 103/103、`cargo test` 50 passed、corpus 40/45，全部无退化。

**边界**：**不并行执行字节码**（一把 GIL，代价是多核用不上）；解释器堆没有 GC（只增不减，但不会回收错）；
`jstack` 只给锁图不给帧列表；虚拟线程不做。

## 0.56.0 — 2026-11-17 · 反射 + 运行期注解

**「跑 Minecraft」四块硬骨头里的第三块。**

新增 `src/vm/reflect.rs`（2364 行）+ `src/classfile/annot.rs`（211 行）：

- **`Class`**：`forName`、`getDeclared/Fields/Methods/Constructors`（含继承）、按名+签名查找、
  `getSuperclass`/`getInterfaces`/`isAssignableFrom`/`isInstance`/`cast`/`getModifiers`/`getSimpleName`/`getName`
- **`Field`/`Method`/`Constructor`**：全 18 个 `get/set` 类型变体、`setAccessible`、
  `Method.invoke`（参数转换、可变参数、**`InvocationTargetException.getCause`**）、`Constructor.newInstance`
- **关键取舍**：反射对象用 **java.base 自己的构造器**造（填 `clazz/slot/name/type/modifiers/signature`），
  于是 `getName/getType/getModifiers/equals/toString/Modifier.toString` 这些**纯字节码方法一行没写**
- **注解**：`RuntimeVisibleAnnotations` / `ParameterAnnotations` / `AnnotationDefault` / `Signature` 解析，
  `getAnnotation`/`getAnnotations`/`isAnnotationPresent`、默认值、嵌套注解、数组、枚举、Class 字面量
- **泛型签名**：`ParameterizedType`/`TypeVariable`/`GenericArrayType`/`WildcardType`
- 运行期按 classpath 加载闭包（`Class.forName` 命中未加载类时）

**实测 R1–R8 与 JDK 25 逐字节一致**（含迷你 DI 容器：扫注解、注入字段、调用）：
```
R2  ours=[type: int mods: private | declaring: R2$Holder name: value | no exception]
    java=[type: int mods: private | declaring: R2$Holder name: value | no exception]
R8  ours=[inject counter <- Counter | inject greeter <- Greeter | value config | call ...]
```

**顺带**：`array_copy` 按规范放宽（元素级 `ArrayStoreException`，之前 `Object[] ← TypeVariable[]` 一律拒绝，
泛型签名直接卡死）；`type_assignable` 支持数组可赋值与字符串真继承链。

**回归**：`verify.sh` 103/103、`cargo test` **56 passed**、corpus 40/45。

**已知缺口**：注解 `toString()` 的成员顺序（HotSpot 会重排方法表，无法复现）、
`getAnnotationsByType`、`AnnotatedType`、`Proxy` + 运行期字节码生成、`Method.getParameters()`。

**顺手发现、没修**：数组读写越界**不抛异常**、静默返回 0（修它会改到 JIT 与三路一致行为，另开一件）。

## 0.57.0 — 2026-11-22 · 拆掉 GIL：字节码真并行

原来是一把大锁（CPython 式：`AtomicPtr` 持锁时造 `&mut Vm`）。实测**线程越多越慢**：

| | 串行 | 4 线程 | |
|---|---|---|---|
| 旧（GIL）| 7024 ms | 15583 ms | **0.45×（劣化）** |
| **新** | 6704 ms | **1732 ms** | **3.81×** |
| HotSpot 25 | 71 ms | 44 ms | 1.61× |

8 线程（各 4M，总工作量相同）：串行 7051 / 8 线程 **1055 = 6.68×**。

### 改动
新增三个文件，`Vm` 拆成三层：

- **`src/vm/shared.rs`** —— `Arc<Shared>` 跨线程根 + 无锁堆：
  对象 = `ObjHdr{class: Arc<str>, len, lock: AtomicU32, cells: Box<[Cell]>}`；
  句柄仍是 `u32`，但下标查到的是**永不搬动的指针**（页表只追加）；
  槽位 = `AtomicU64` + `AtomicU8` tag → **字段/元素读写无锁**；分配 = 页表 + 原子序号
- **`src/vm/monitors.rs`** —— thin lock：`0` / `tid<<8|count` / `1`=INFLATED；
  未竞争只付一次 CAS，真竞争**单向膨胀**成 `WaitSet`（先进先出 + hand-off 移交）
- **`src/vm/gc.rs`** —— 停靠点（轮次 + 到达计数 + park/unpark）+ 卡表写屏障接线
- `mod.rs` `Vm` = `Arc<Shared>` + 每线程 `Vm`（depth/tick/held）+ 栈上 Frame；`Vm::fork()`
- `thread.rs` 删掉全部 GIL（`gil_lock/gil_unlock/vm_ref/VM_PTR`）
- `load.rs` `Arc<ClassStore>` 快照（COW）
- `heap.rs/exec.rs/natives.rs/reflect.rs/indy.rs/native_reg.rs` 84 处直接下标改走堆访问器

### 验收
`verify.sh` **103/103** · `cargo test` **56 passed** · corpus **40/45**（与基线同）·
`thread/` 16 个用例全过（T1 = 400000、T3 无丢失、T4 死锁检出）· `run_jdk M2` 与 java 一致 ·
2M 对象并行分配峰值 355MB、结果正确

### 没做到的（诚实）
- **没有 GC**：`Frame.locals` 是 `Vec<Slot>`，引用与 int 无类型标签 → 追溯扫描分不清句柄和整数。
  停靠点协议可用，缺的是**每帧引用位图**。所以堆只增不减（线性于分配总量，不是失控）。
- `librustjrt::SharedHeap` 没接进来：它槽宽 4 字节（装不下 16 字节 `Slot`），`get_i32/set_i32` 要 `&mut Heap`
  （写锁会让字段访问全局串行），年轻代还是搬动式（需要精确根集）。已改用自建无锁池。
- 死锁报告只列膨胀过的监视器（thin 持有者的锁在对象头里，没有全局索引）。

## jdk-tools 0.1.1 — 2026-11-18 · 工具加固：退出码、真镜像 extract、`--help` 一致性

对 15 个工具做了一轮对抗性体检（15 × 8 类坏输入 = 77 个用例，`scripts/tool-audit.sh`），
**0 次 panic**。查出并修掉两个真 bug，另修一个把真镜像 extract 拖慢 97 倍的排序热点。

### 修

- **`jshell`：片段文件读不到时打印错误但退出 0**（静默失败，脚本会把失败当成功）。
  改成记 `io_failed` 标志，退出前 `exit(EXIT_FAIL)`。回归：`tests/bad_inputs.rs`。
  症状举例：`jshell missing.jsh; echo $?` 之前是 0，现在是 1。
- **`jimage extract` 在真 JDK 镜像上跑 301 个条目就断**（`写 …\sun 失败: 拒绝访问`）。
  根因：镜像里有**目录节点**（`/modules`、`/java.base/sun`、`/modules/java.base/sun` 这类，
  有些还带着数据），原实现把每一条都当文件写 → 撞上同名目录 → Windows 报 os error 5。
  改成「有子条目的条目按目录建」。实测：30482/30482 条目、143 MB、14.9 秒。
- **`jimage list` 25.9 秒 → 0.27 秒**（145 MB 镜像、30629 条目）。
  根因在排序比较函数里调 `rel_path()`：每比较一次 2 次 `format!` + `to_string`，
  30k 条目约 45 万次比较。改成先算好 `(module, rel)` 键再排（输出逐字节不变）。

### 验证

- `cargo test --release --test bad_inputs` —— 4 个测试全过（坏路径/非法选项/选项缺值/`--help` 干净）
- `scripts/tool-audit.sh` —— 77 个用例：0 panic、退出码只有 0/1/2（+ 服务类工具被 timeout 掐的 124）
- `docs/工具退出码.md` —— 15 工具 × 正常/参数错/IO 错 的完整矩阵 + 逐用例明细
- `docs/工具体积.md` —— 32 个 exe 的体积表（只有 `jdb` 2.24 MB、`jshell` 2.23 MB 超过 2 MB，
  原因是静态链接了编译器前端 + 解释器，其余 144–400 KB）
- 大输入：`jimage` 145 MB 镜像（info 70 ms / list 267 ms / verify 915 ms / extract 14.9 s）、
  `jmod` 41 MB `java.base.jmod` 全量 7450 条目（list 116 ms / extract 5.29 s，真 jmod 6.31 s）、
  `jfr` 50 MB 多 chunk 录制（summary 602 ms / print 11.9 s，峰值 775 MB —— 已知短板：print 会把全部事件解码进内存）
- 并发：`jwebserver` 50 个并发连接拉同一个 20 MB 文件，50/50 sha256 一致、无串包、服务不崩

### 没做

- 仓库里**没有 `jar` 工具**（只有 `jarsigner` 与 launcher 的 `-jar` 运行能力），
  所以「500 MB 的 jar」这一项测不了；zip 层的大输入用 `jmod`（同一条 zip 实现）覆盖。
- `jps` 会静默忽略未知位置参数（`jps abc` 退出 0）。改成报错会让 `jps -l $(hostname)` 变红，
  属于「有意保留」，已写进 `docs/工具退出码.md` §4。

## 0.58.0 — 2026-11-18 · JIT 按方法回退 + invokedynamic、JPMS、基准与工具体检

### JIT 按方法回退（把「一处不支持就整程序作废」修掉）
原来任何一个方法编不了，整个程序退回解释器。现在逐方法尝试，编不了的只废那个方法，
调用点发「解释器桩」（参数拍成 i64 数组 → `vmless_interp_call`）。

| 程序 | 解释器 | **JIT(新)** | HotSpot 25 |
|---|---|---|---|
| P1 4 线程 × 800 万 | 1782 ms | **185 ms** | 45 ms |
| P2 4×800 万 + 字符串拼接 | 6679 ms | **393 ms**（**17×**）| 71 ms |

`P2` 原来因为一句 `"sum=" + s`（invokedynamic）整程序作废，现在是「编译 2 个方法，解释 0 个」。

### 修的严重 bug：`synchronized` 跨线程吃 18 GB
根因是 JIT↔解释器通道：解释器在**生成代码的回调里懒建**（`Vm::open` 要加载 5421 类/95MB），
且**没做线程登记**（`cur_tid()`=0，监视器用非法 tid 空转）→ 每次调用重建解释器，95MB/次 ≈ 90MB/s 涨到 18.8 GB。
已改成预建 + 补登记 + 只对「描述符无引用」的方法开桩。
回归脚本 `scripts/verify-sync.sh`，**断言峰值内存 < 200MB、墙钟 < 30s**。

### invokedynamic
`src/jit/indy.rs`：编译期分类调用点，`StringConcatFactory` 的 recipe 解成「拼接计划」
（`\u0001`=栈上参数、`\u0002`=引导常量）；`0xba` 就地展开成一次 `indy_concat(plan, i64[])`。

### JPMS 模块系统
`classfile/attr.rs` 解析 `Module` 全字段；`load/{modgraph,modules,jhash,cli}.rs`；
`vm/jpms.rs`。实测：真 `lib/modules` 解析出 **70 个模块**（对齐 `java --list-modules`）+
拓扑序 + requires 树；`--describe-module java.base` 的 exports/contains **139 行逐行一致**；
`--add-opens` 不加抛 `InaccessibleObjectException`、加了 `setAccessible OK`；
`--add-exports` 与真 java **逐字相同**；split package 检测对齐。
**顺手修了冷启动 36–44 秒**：不是扫整棵树，是扁平挂载的 80 个来源逐个试开（27425 次 open），
加「包→上次命中来源」提示后降到 **1116 次（24.6×）**。

### 基准（`bench/`，8 维、30 次交错、min 口径）
**赢**：启动 133.9 vs 727.3 ms（**5.43×**）· 常驻 RSS 5.5 vs 42.7 MB（**7.76×**）·
空对象 8 vs 16 B（**2×**）· 8 线程并行加速 **7.12× vs 3.32×**（线性度 89% vs 41%）
**输**：解释吞吐慢 96.9× · JIT 慢 5.73× · **GC 最大停顿 46.3 vs 12.7 ms（慢 3.64×）** ·
GC 总停顿慢 4.39× · 分配峰值 RSS 慢 2.63×
> ⚠️ **纠正之前的一个说法**：我们此前的「GC 停顿 0.021ms vs 23.4ms」是 `librustjrt` 独立基准的数字，
> **不是 VM 实际运行时的表现**。VM 现在的 GC 是 STW 复制 + 标记清扫，实测**输给 G1 3.64×**。

### 工具体检（15 工具 × 8 类坏输入 = 77 用例，0 panic）
修了 3 个真问题：`jshell` 读不到片段文件却 exit 0（静默失败）·
`jimage extract` 在真镜像跑 301 个条目就断（目录节点被当文件写）→ 修复后 **30482/30482 条目**·
`jimage list` **25.9s → 0.27s**（排序比较里每次调 `rel_path()`）。产出 `docs/工具退出码.md`。

### 新发现未修（高价值）
**`Thread` 子类 override 的 `run()` 不被派发** —— 线程体不执行，`v=42` 静默变 `0`（lambda/Runnable 版正常）。
这会静默给出错误结果，下一轮优先修。
