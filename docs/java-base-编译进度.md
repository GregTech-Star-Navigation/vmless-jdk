# java.base 编译进度

目标：让 vmless 的 javac 能编译 OpenJDK 自己的 `java.base` 源码（3058 个 `.java`）。

口径：`examples/cover_dir.rs` 走完整流水线（parse → sema → codegen），**落 `.class` 文件**
才算通过；「找不到方法就不报错」这类放宽一律不做。

| 日期 | 构建 | java.lang | java.base 全量 | 主要失败类别前 3 |
| --- | --- | --- | --- | --- |
| 2026-11-17 基线 | 修 jar.rs 遮蔽错误后 | 117/456 = 25.7% | 624/3058 = 20.4% | 找不到类 1137、未找到实例方法 705、未定义的变量 102 |
| 2026-11-17 第二轮 | rt_jb2 | 117/456 = 25.7%（无 cp）<br>252/456 = 55.3%（带 cp） | 649/3058 = 21.2%（无 cp）<br>**1250/3058 = 40.9%（带 cp）** | 未找到实例方法 529、未定义的变量 385、instanceof 模式绑定 86 |

（"带 cp" = classpath 放的是从本机 JDK 25 的 `lib/modules` 解出来的 7384 个 java.base
`.class`，即 javac 编 java.base 时本来就有的「平台类可见」前提。解包工具：
`examples/dump_jimage.rs`。）

## 第二轮改了什么（每条都影响一整类）

| 改动 | 位置 | 一句话 |
| --- | --- | --- |
| 自增/自减 + 复合赋值 | `src/codegen/stmt.rs:14`（语句位置降糖）、`src/codegen/expr.rs` `compile_inc_dec` | `i++`/`--n`/`x += e` 在语句位置改写成等价赋值；作为**值**用时（`arr[i++]`）走局部变量的 dup 路径；lvalue 不可重算时明确报错 |
| 实例字段初始化 | `src/codegen/class.rs` `emit_instance_field_inits` / `compile_ctor_body` | 以前**根本没编**：`class W { long f = 5; }` 读出来是 0 且无任何报错。现在在 `super(...)` 之后、构造器体之前按源码顺序 putfield（隐式默认构造器也走同一条路） |
| 静态字段初始化拓宽 | `src/codegen/class.rs`（`<clinit>`） | `static long X = 1;` 补 `i2l`：以前写出 `bipush 1; putstatic long` → HotSpot VerifyError |
| 数组初始化器 | `src/codegen/expr.rs` `emit_array_init` / `emit_init` | `int[] a = {1,2,3}` / `new int[]{…}`：元素类型优先取声明类型，取不到按字面量推断；支持 8 种基本类型元素 + 引用类型 |
| 十六进制/二进制/八进制字面量 | `src/codegen/expr.rs` `parse_int_literal` | `0xFF`、`0b1010`、`012`、`1_000` 全部按 Java 语义解析（以前 `parse::<i32>()` 直接失败） |
| `(int) -3.99` | `src/front/parser/expr.rs` `try_parse_cast` | JLS 15.15：**基本类型**的 cast 后面允许跟 `+`/`-`（以前一律回退成括号表达式 → ClassLit） |
| 隐式 `import java.lang.*` | `src/sema/qualify.rs` `package_of` | `Appendable`/`Readable` 这类不在白名单里的 java.lang 类型，classpath 上有就补成 `java/lang/X` |
| 空语句 | `src/codegen/stmt.rs` | `Stmt::Empty`（`;`、`assert`、块内局部类）不再报「暂不支持的语句」 |
| 赋值当表达式 | `src/codegen/expr.rs` | `if ((n = next()) >= 0)` 支持局部变量目标；字段/数组元素目标明确报错 |
| classpath 世界快照缓存 | `src/sema/resolve.rs` `World` / `platform_cache` / `world_cache` | 7384 个类的成员表**整进程只建一次**，之后每文件只克隆（`Arc` 共享内层表）。批量跑 90 个文件从 56s 降到 14s |
| classpath 深目录 | `src/sema/resolve.rs` `collect_class_files` | 深度上限 4 → 12：`jdk/internal/foreign/abi/aarch64/linux` 这类包以前**静默漏类** |
| Windows classpath 分隔符 | `src/load/mod.rs` `from_spec` | 只在 `;` 上切：`D:/x/classes` 按 `:` 切会把绝对路径切成 `D` + `/x/classes`（`打开 D 失败`） |
| 常量池悬空 Class 项 | `src/vm/load.rs` 预扫描 | codegen 留下的 `<SELF>` 占位 Class 项解不开时跳过（JVM 也是用到才加载），否则整个执行失败 |

## 进度是怎么涨的

- 20.4% → 21.2%：只做了语言层修复（自增、数组初始化、字面量、空语句…）。
  **单文件模式的天花板就在这里** —— 76% 的失败是「跨编译单元解析不到」，不是代码写法。
- 21.2% → 40.9%：把 java.base 自己的 `.class` 放进 classpath（等价于真实 javac 的
  「平台类可见」），`找不到类` 从 1137 掉到 68，`未找到实例方法` 从 705 掉到 529。

## 第三轮（批量模式）进度

**未完成**。现状与缺口：

- 已有 `Resolver::world()` + 进程级缓存，**同一 classpath 下多次编译可以共享符号表**；
  但 `compile_with_path` 仍是「一个 CU 一个 Resolver」，**没有一次吃多个 CU 的入口**。
- 批量模式要补的入口：`compile_units(&[CompilationUnit], cp) -> Vec<(name, bytes)>`，
  Resolver 用全部 CU 建一次 `own`，再逐类型 codegen。`cover_dir` 已经按文件顺序跑，
  改动点集中在 `src/codegen/mod.rs:48` 与 `src/sema/resolve.rs::new`。
- 批量模式的收益上限：`找不到类 68` + `未定义的变量 385`（同包限定符）+ `未找到方法 65`
  里的一大半都是「同包另一个源文件没有被编进来」，这正是批量模式要解决的。

## 回归基线（每轮必跑）

```bash
./verify.sh /d/vmless/bjb5      # 三路一致 96 / 失败 7
REPS=1 BIN=/d/vmless/bjb5 bash /d/vmless/corpus/run.sh   # 编译通过 43/45，三路一致 32/43
```

7 个 verify 失败**全部是 JIT 侧 float/double 没实现**（`run_jit` 输出为空），与编译器
改动无关；基线 `bin_l7v` 是 88/12（且它连 `ArrInit1/2`、`SwitchTable` 都编不过）。
| 2026-11-18 R1 | rt_jbc1 | — | **750/3381 = 22.2%**（源码集换成 3381 个，src.zip 解出） | 未找到实例方法 755、找不到类 ~430、未定义的变量 129 |
| 2026-11-18 R1' | rt_jbc1 | — | **1653/3381 = 48.9%** | **调用点压根没用 classpath**（改进程级共享 `Arc`）+ 类名判断改大小写敏感 |
| 2026-11-18 R2 | rt_jbc2 | — | 1754/3381 = 51.9% | 内建 `Object` 占位挡住真的 `Object.class`；嵌套类型静态访问 |
| 2026-11-18 R3 | rt_jbc3 | — | 1903/3381 = 56.3% | **同名重载只留第一个描述符** → 实参按错类型转换；class 字面量 |
| 2026-11-18 R4 | rt_jbc3 | — | 1991/3381 = 58.9% | cast 回溯漏比较运算符；**类型位置多段名按 `$` 拼**（`Map.Entry` 生成不存在的类名） |
| 2026-11-18 R5 | rt_jbc4 | — | **2096/3381 = 62.0%** | `instanceof` 模式绑定、`import static`、数组 `clone()` |
| 2026-11-18 R6 | rt_jbc5 | — | **2271/3381 = 67.2%** | `var` 推导、通配静态导入、字段隐藏、嵌套类看外层静态成员、switch 表达式 + `yield` |



> **口径提醒**：这份表里的 3058 是早期的源码集；2026-11-18 起换成 `C:/Program Files/Eclipse Adoptium/jdk-25.0.4.101-hotspot/lib/src.zip`
> 解出的 **3381** 个文件，所以 40.9% 与 48.9% **不能直接比**。

### R7–R9（2026-11-18，同上口径 3381 文件 + classpath）

| 轮 | 改动 | 通过率 |
| --- | --- | --- |
| R7 | `case CONST ->` 的 lambda 误判、`private record X(...)` 成员识别 | 70.9%（2398） |
| R8 | 名字解析顺序改成「本类嵌套 → 同包 → `java.lang`」；嵌套类型 class 文件按 `$` 找 | 72.3%（2444） |
| R9 | **VarHandle/MethodHandle 签名多态调用**（`(int) vh.getVolatile(o,i)`、`MethodHandle.linkToStatic`） | 72.7%（2459） |
| R10 | 模式绑定的 `&&` 链（`if (a && P && b)`、三元条件） | **73.9%（2497）** |

**当前：73.9%（2497 / 3381）**。最新桶表见 [`rustjavac/docs/java-base-失败分布.md`](../rustjavac/docs/java-base-失败分布.md)。
