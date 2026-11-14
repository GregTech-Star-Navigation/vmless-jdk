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
