# java.base 编译失败分布（第一轮摸底）

日期：2026-11-17 · 工具：`examples/cover_dir.rs`（新增，批量跑整棵源码树并聚桶）
样本：`D:/openjdk/jdk25u-master/src/java.base/share/classes` 全部 **3058** 个 `.java`

## 怎么测的

```
cover_dir <源码目录> [classpath 目录...] [--detail <明细 TSV>]
```

每个文件走**完整流水线**（parse → flatten → erase → enum 降级 → qualify → Resolver
→ codegen），失败按「归一化后的错误消息」分桶（反引号里的具体名字、数字都抹掉），
明细落 TSV（`docs/.coverN.tsv`），可以按桶反查文件。

基线数据（只修了 `src/load/jar.rs` 的一处遮蔽编译错误，**未做任何功能改动**）：

```
文件 3058 个：完整编译通过 624，编译失败 2131，解析失败 303，通过率 20.4%
```

解析失败 303 = 9.9%，和 `examples/parse_dir` 的 91.5% 解析率一致 —— **语法层基本通了，
卡在 sema / codegen**。

## 基线分布（不带 classpath，2434 条失败）

| # | 错误类别 | 次数 | 占失败比 | 涉及文件示例 |
| --- | --- | --- | --- | --- |
| 1 | `找不到类 X（cannot find symbol: class …）` | 1137 | 46.7% | sun/security/pkcs/ContentInfo.java, sun/security/x509/BasicConstraintsExtension.java, jdk/internal/event/DeserializationEvent.java |
| 2 | `未找到实例方法 X` | 705 | 29.0% | com/sun/crypto/provider/BlockCipherParamsCore.java, java/lang/StringLatin1.java, java/nio/charset/Charset.java |
| 3 | `未定义的变量 X` | 102 | 4.2% | com/sun/crypto/provider/AESParameters.java, java/lang/ConditionalSpecialCasing.java, java/io/BufferedReader.java |
| 4 | parse `期望 Colon，实际 Semi` | 73 | 3.0% | java/io/ObjectInputStream.java:1696, java/lang/ConditionalSpecialCasing.java:174, java/lang/classfile/CodeBuilder.java:608 |
| 5 | parse `无法解析的表达式，实际 New` | 60 | 2.5% | java/io/Console.java, java/io/FilePermission.java, java/io/ObjectStreamClass.java |
| 6 | parse `期望标识符，实际 关键字 X` | 51 | 2.1% | java/lang/ClassLoader.java, java/lang/Package.java, java/lang/SecurityManager.java |
| 7 | `暂不支持的语句: Empty` | 22 | 0.9% | com/sun/crypto/provider/KWUtil.java, java/lang/CharSequence.java, java/lang/StringUTF16.java |
| 8 | `暂不支持 instanceof 模式绑定` | 21 | 0.9% | java/net/InterfaceAddress.java, java/security/cert/CertPath.java, java/security/spec/ECPoint.java |
| 9 | `整数字面量 X 解析失败`（十六进制等） | 21 | 0.9% | com/sun/crypto/provider/ML_KEM.java, java/io/ObjectStreamConstants.java |
| 10 | parse `期望 Colon，实际 标识符 X` | 20 | 0.8% | java/lang/Module.java, java/lang/classfile/attribute/ConstantValueAttribute.java |
| 11 | `暂不支持的表达式: Unary { op: PostInc }` | 16 | 0.7% | java/lang/invoke/ConstantGroup.java, java/nio/charset/Charset.java |
| 12 | `暂不支持的表达式: Assign { op: NonZero }` | 16 | 0.7% | （`x += e` 复合赋值） |
| 13 | `暂不支持的语句: Expr(Assign …)` | 5 | 0.2% | java/io/ObjectOutputStream.java |
| 14 | `暂不支持的表达式: MethodRef` | 6 | 0.2% | java/io/SerializationMisdeclarationChecker.java |
| 15 | `暂不支持的表达式: Lambda` | 5 | 0.2% | （lambda 作为实参/返回值） |

**关键结论**：第 1、2 名合计 1842 条 = 76% 的失败来自「**跨编译单元的类型/方法解析不到**」，
不是语法也不是 codegen 能力 —— 单文件模式没有 classpath，任何 `java/io/IOException`
之外的类都查不到，连带它上面的方法全部「找不到」。所以修 3 种代码写法只能拿到零头，
**真正的杠杆是 classpath**。

## 第二轮后（带 java.base classpath）的分布

classpath 来源：`lib/modules`（jimage）解出来的 7384 个 java.base `.class`，见
`examples/dump_jimage.rs`。

| # | 错误类别 | 次数 | 样例 |
| --- | --- | --- | --- |
| 1 | `未找到实例方法 X` | 529 | `clone` 65、`getClass` 22、`of` 19、`getName` 13、`requireNonNull` 11 —— **Object 的 protected/内建方法没在表里**是主因 |
| 2 | `未定义的变量 X` | 385 | 接口常量继承（`class A implements Constants { … SEMI_BLKSIZE … }`）、同包类名当限定符（单文件模式查不到 `.class`） |
| 3 | `暂不支持 instanceof 模式绑定` | 86 | `if (o instanceof Foo f)` |
| 4 | parse `期望 Colon，实际 Semi` | 73 | **`switch` 表达式 / 箭头 case**（`return switch (x) { case A -> … }`） |
| 5 | `找不到类 X` | 68 | 同包类型（单文件模式无解，要批量编） |
| 6 | `未找到方法 X` | 65 | 同上（无接收者的调用落到 `this`） |
| 7 | `暂不支持的表达式: MethodRef` | 65 | `Foo::bar` 方法引用 |
| 8 | parse `无法解析的表达式，实际 New` | 60 | `new` 出现在特殊位置（数组泛型/注解值/匿名类） |
| 9 | parse `期望标识符，实际 关键字 X` | 51 | `record`/`var`/`yield` 等受限标识符当变量名 |
| 10 | `暂不支持的表达式: Lambda` | 46 | lambda 实参 / 变量初始化 |
| 11 | `X/X 前面不能有未消费的操作数` | 44 | 栈深记账：`needs_scratch` 漏判的表达式形状 |
| 12 | `字段 X 在父类与子类中重复声明` | 32 | 字段隐藏（field hiding）不支持 |

## 复现

```bash
export CARGO_TARGET_DIR='D:\vmless\rt_jb2'
cargo build --release --example cover_dir --example dump_jimage
# 一次性把 java.base 的 .class 解出来当 classpath
dump_jimage.exe "C:/Program Files/Eclipse Adoptium/jdk-25.0.4.101-hotspot/lib/modules" \
                "D:/vmless/jbcov/.cp/jb" java.base
cover_dir.exe "D:/openjdk/jdk25u-master/src/java.base/share/classes" \
              "D:/vmless/jbcov/.cp/jb" --detail docs/.cover5.tsv
```
