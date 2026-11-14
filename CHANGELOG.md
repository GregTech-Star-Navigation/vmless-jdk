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
