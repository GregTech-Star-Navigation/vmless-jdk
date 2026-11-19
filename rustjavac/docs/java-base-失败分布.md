# java.base 编译失败分布

> 由 `examples/cover_dir.rs` 生成，改完代码重跑即可刷新。

- 源码根：`D:/vmless/jbsrc/java.base`
- classpath：`D:/vmless/patch/img/java.base`
- 生成时间：2026-11-20 00:18:24 +08:00

## 总览

| 文件总数 | 通过 | 编译失败 | 解析失败 | 通过率 |
|---|---|---|---|---|
| 3381 | 2880 | 436 | 65 | **85.2%** |

## 失败桶

| # | 次数 | 占失败比 | 错误骨架 | 样例文件 |
|---|---|---|---|---|
| 1 | 153 | 30.5% | `[compile] 未找到实例方法 X（X）（X）` | `com/sun/crypto/provider/PBES2Parameters.java`<br>`java/io/Console.java`<br>`java/io/OutputStreamWriter.java` |
| 2 | 44 | 8.8% | `[compile] 未定义的变量 X（X）` | `com/sun/crypto/provider/ChaCha20Cipher.java`<br>`java/io/ObjectOutputStream.java`<br>`java/lang/Class.java` |
| 3 | 32 | 6.4% | `[parse] X 期望 X，实际 标识符 X` | `java/lang/Module.java`<br>`java/lang/classfile/attribute/ConstantValueAttribute.java`<br>`java/lang/classfile/snippet-files/PackageSnippets.java` |
| 4 | 31 | 6.2% | `[compile] 未找到方法 X（X）` | `java/io/BufferedReader.java`<br>`java/lang/CharSequence.java`<br>`java/lang/Process.java` |
| 5 | 21 | 4.2% | `[compile] X/X 前面不能有未消费的操作数（X）（X）` | `java/lang/invoke/MemberName.java`<br>`java/lang/module/Resolver.java`<br>`java/time/LocalDate.java` |
| 6 | 20 | 4.0% | `[compile] 暂不支持的类型转换（X）（X）` | `java/nio/channels/Channels.java`<br>`java/time/chrono/IsoChronology.java`<br>`java/util/TreeMap.java` |
| 7 | 18 | 3.6% | `[compile] 比较结果作为表达式时，前面不能有未消费的操作数（X）（X）` | `java/io/ObjectInputStream.java`<br>`java/lang/invoke/DirectMethodHandle.java`<br>`java/math/BigDecimal.java` |
| 8 | 16 | 3.2% | `[compile] 只能把引用值转成引用类型（X）` | `java/security/KeyFactory.java`<br>`java/security/KeyPairGenerator.java`<br>`java/security/SecureRandom.java` |
| 9 | 14 | 2.8% | `[compile] 表达式嵌套过深：脏槽池只有 N 个（X）` | `java/lang/classfile/AnnotationValue.java`<br>`java/net/Inet6Address.java`<br>`java/net/URLConnection.java` |
| 10 | 13 | 2.6% | `[compile] 赋值作为表达式时只支持局部变量：X(X("X"), "X")（X）` | `java/lang/PublicMethods.java`<br>`java/lang/Thread.java`<br>`java/math/MutableBigInteger.java` |
| 11 | 13 | 2.6% | `[parse] X 期望标识符，实际 X` | `java/lang/invoke/MethodHandles.java`<br>`java/lang/reflect/Proxy.java`<br>`java/nio/DirectByteBuffer.java` |
| 12 | 11 | 2.2% | `[compile] 推断不出 X 的目标类型（X）（X）` | `java/io/ObjectInputFilter.java`<br>`java/lang/classfile/ClassTransform.java`<br>`java/lang/classfile/FieldTransform.java` |
| 13 | 10 | 2.0% | `[compile] 找不到类 X（X）` | `com/sun/crypto/provider/DHKEM.java`<br>`java/lang/LiveStackFrameInfo.java`<br>`java/security/DomainLoadStoreParameter.java` |
| 14 | 10 | 2.0% | `[compile] 暂不支持的表达式: X { X: X(X), X: X("X"), X: X(X("X/X/X/X/X"), "X") }（X）` | `jdk/internal/random/L128X1024MixRandom.java`<br>`jdk/internal/random/L128X128MixRandom.java`<br>`jdk/internal/random/L128X256MixRandom.java` |
| 15 | 8 | 1.6% | `[parse] X 无法解析的表达式，实际 X` | `java/io/ObjectStreamClass.java`<br>`java/lang/invoke/InvokerBytecodeGenerator.java`<br>`java/util/concurrent/Phaser.java` |
| 16 | 7 | 1.4% | `[compile] 整数字面量 X 解析失败` | `java/lang/Long.java`<br>`java/net/URI.java`<br>`java/util/BitSet.java` |
| 17 | 6 | 1.2% | `[compile] 暂不支持的表达式: X { X: X(X), X: X("X"), X: X("X") }（X）` | `java/lang/String.java`<br>`java/math/BigInteger.java`<br>`java/util/ArrayDeque.java` |
| 18 | 6 | 1.2% | `[compile] 未定义的变量 X` | `java/util/AbstractList.java`<br>`java/util/IdentityHashMap.java`<br>`javax/crypto/JceSecurityManager.java` |
| 19 | 5 | 1.0% | `[compile] X-X 的被迭代表达式不是数组，也找不到 X 方法（X）（X）` | `java/lang/ThreadLocal.java`<br>`java/lang/classfile/attribute/snippet-files/PackageSnippets.java`<br>`java/util/Calendar.java` |
| 20 | 5 | 1.0% | `[compile] 类 X 的构造器里 X(N)/X(N) 必须是第一条语句` | `java/lang/invoke/LambdaForm.java`<br>`sun/security/ssl/CertificateRequest.java`<br>`sun/security/ssl/CipherSuite.java` |
| 21 | 5 | 1.0% | `[parse] X 期望 X，实际 X` | `java/lang/invoke/BoundMethodHandle.java`<br>`java/util/function/Predicate.java`<br>`java/util/regex/CharPredicates.java` |
| 22 | 4 | 0.8% | `[compile] X 不是数组：接收者静态类型是 X/X/X（X）（X）` | `java/lang/ProcessImpl.java`<br>`java/text/DateFormatSymbols.java`<br>`java/util/Date.java` |
| 23 | 4 | 0.8% | `[compile] 匿名类暂时不支持捕获外层 X（X））：把要用的值先赋给一个局部变量再捕获，或改用 X` | `java/lang/invoke/ClassSpecializer.java`<br>`java/util/ReverseOrderSortedMapView.java`<br>`java/util/ReverseOrderSortedSetView.java` |
| 24 | 4 | 0.8% | `[compile] 整数字面量 X 解析失败（X）` | `com/sun/crypto/provider/GHASH.java`<br>`java/lang/FdLibm.java`<br>`java/util/SplittableRandom.java` |
| 25 | 4 | 0.8% | `[compile] 自增/自减作为表达式时只支持局部变量：X { X: X("X"), X: X("X") }（X）` | `com/sun/crypto/provider/CounterMode.java`<br>`com/sun/crypto/provider/GaloisCounterMode.java`<br>`sun/security/rsa/MGF1.java` |
| 26 | 3 | 0.6% | `[compile] 浮点字面量 X 解析失败` | `java/lang/Double.java`<br>`java/lang/Math.java`<br>`java/util/Random.java` |
| 27 | 3 | 0.6% | `[parse] X 期望标识符，实际 关键字 X` | `java/util/stream/FindOps.java`<br>`java/util/stream/Nodes.java`<br>`jdk/internal/foreign/AbstractMemorySegmentImpl.java` |
| 28 | 2 | 0.4% | `[compile] X 字面量 X 解析失败（X）` | `java/lang/Float.java`<br>`java/lang/runtime/ExactConversionsSupport.java` |
| 29 | 2 | 0.4% | `[compile] X 表达式只支持语句/赋值/返回位置（X）` | `java/util/stream/SliceOps.java`<br>`jdk/internal/vm/VMSupport.java` |
| 30 | 2 | 0.4% | `[compile] 引用类型只能比较 == / !=（X）` | `java/util/stream/StreamOpFlag.java`<br>`sun/util/locale/provider/BreakIteratorProviderImpl.java` |
| 31 | 2 | 0.4% | `[compile] 推断不出 X 的目标类型（X）` | `sun/security/provider/AbstractDrbg.java`<br>`sun/security/provider/certpath/ssl/SSLServerCertStore.java` |
| 32 | 2 | 0.4% | `[compile] 暂不支持的类型转换（X）` | `sun/net/InetAddressCachePolicy.java`<br>`sun/security/ssl/SSLConfiguration.java` |
| 33 | 2 | 0.4% | `[compile] 暂不支持的表达式: X { X: X(X), X: X("X"), X: X { X: X, X: X("X"), X: X("X") } }（X）` | `java/util/ArrayList.java`<br>`java/util/Vector.java` |
| 34 | 2 | 0.4% | `[compile] 暂不支持的表达式: X { X: X(X), X: X("X"), X: X("N") }（X）` | `java/util/DualPivotQuicksort.java`<br>`jdk/internal/math/FloatingDecimal.java` |
| 35 | 2 | 0.4% | `[parse] X 期望关键字 X，实际 X` | `java/text/MessageFormat.java`<br>`jdk/internal/classfile/impl/BytecodeHelpers.java` |
| 36 | 1 | 0.2% | `[compile] 推断不出方法引用的目标类型（X）` | `java/lang/constant/DynamicConstantDesc.java` |
| 37 | 1 | 0.2% | `[compile] 推断不出方法引用的目标类型（X）（X）` | `java/lang/VirtualThread.java` |
| 38 | 1 | 0.2% | `[compile] 暂不支持的表达式: X { X: X(X), X: X { X: X("X"), X: X("X") }, X: X { X: X("X"), X: X { X: X, X: X { X: X("X"…` | `sun/security/provider/MD2.java` |
| 39 | 1 | 0.2% | `[compile] 暂不支持的表达式: X { X: X(X), X: X("X"), X: X { X: X, X: "X", X: [], X: [] } }（X）` | `sun/text/RuleBasedBreakIterator.java` |
| 40 | 1 | 0.2% | `[compile] 暂不支持的表达式: X { X: X(X), X: X("X"), X: X(X("X"), "X") }（X）` | `java/text/CollationElementIterator.java` |
| 41 | 1 | 0.2% | `[compile] 未找到实例方法 X（X）` | `java/util/ReverseOrderListView.java` |
| 42 | 1 | 0.2% | `[compile] 未绑定实例方法引用要求 X 至少有一个参数（X）` | `sun/nio/ch/Poller.java` |
| 43 | 1 | 0.2% | `[compile] 浮点字面量 X 解析失败（X）` | `jdk/internal/event/EventHelper.java` |
| 44 | 1 | 0.2% | `[compile] 继承环: X/X/X → X/X/X` | `java/lang/Object.java` |
| 45 | 1 | 0.2% | `[compile] 自增/自减作为表达式时只支持局部变量：X { X: X("X"), X: X("N") }（X）` | `sun/invoke/util/BytecodeDescriptor.java` |
| 46 | 1 | 0.2% | `[compile] 自增/自减作为表达式时只支持局部变量：X(X("X"), "X")（X）` | `jdk/internal/icu/impl/NormalizerImpl.java` |
| 47 | 1 | 0.2% | `[compile] 自增/自减的目标不是局部变量也不是字段：X（X）` | `javax/crypto/Cipher.java` |
| 48 | 1 | 0.2% | `[compile] 赋值作为表达式时只支持局部变量：X(X { X: X(X("X"), "X"), X: X("N") }, "X")（X）` | `jdk/internal/icu/text/BidiBase.java` |
| 49 | 1 | 0.2% | `[parse] X 无法解析的表达式，实际 关键字 X` | `sun/security/util/Cache.java` |
| 50 | 1 | 0.2% | `[parse] X 期望类型声明，实际 标识符 X` | `module-info.java` |

共 50 个桶。
