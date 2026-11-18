# java.base 编译失败分布

> 由 `examples/cover_dir.rs` 生成，改完代码重跑即可刷新。

- 源码根：`D:/vmless/jbsrc/java.base`
- classpath：`D:/vmless/patch/img/java.base`
- 生成时间：2026-11-18 21:33:27 +08:00

## 总览

| 文件总数 | 通过 | 编译失败 | 解析失败 | 通过率 |
|---|---|---|---|---|
| 3381 | 2714 | 588 | 79 | **80.3%** |

## 失败桶

| # | 次数 | 占失败比 | 错误骨架 | 样例文件 |
|---|---|---|---|---|
| 1 | 198 | 29.7% | `[compile] 未找到实例方法 X（X）（X）` | `com/sun/security/ntlm/Client.java`<br>`com/sun/security/ntlm/Server.java`<br>`java/io/Console.java` |
| 2 | 123 | 18.4% | `[compile] 未定义的变量 X（X）` | `com/sun/crypto/provider/JceKeyStore.java`<br>`com/sun/crypto/provider/PBKDF2KeyImpl.java`<br>`com/sun/crypto/provider/SealedObjectForKeyProtector.java` |
| 3 | 33 | 4.9% | `[compile] 比较结果作为表达式时，前面不能有未消费的操作数（X）（X）` | `com/sun/crypto/provider/AESCrypt.java`<br>`com/sun/crypto/provider/ChaCha20Cipher.java`<br>`java/io/FilePermission.java` |
| 4 | 31 | 4.6% | `[parse] X 期望 X，实际 标识符 X` | `java/lang/Module.java`<br>`java/lang/classfile/attribute/ConstantValueAttribute.java`<br>`java/lang/classfile/snippet-files/PackageSnippets.java` |
| 5 | 30 | 4.5% | `[compile] 暂不支持的类型转换（X）（X）` | `java/lang/classfile/CodeBuilder.java`<br>`java/lang/invoke/BootstrapMethodInvoker.java`<br>`java/lang/invoke/LambdaForm.java` |
| 6 | 29 | 4.3% | `[compile] X/X 前面不能有未消费的操作数（X）（X）` | `com/sun/crypto/provider/CipherCore.java`<br>`com/sun/crypto/provider/GaloisCounterMode.java`<br>`java/io/FileInputStream.java` |
| 7 | 28 | 4.2% | `[compile] 未找到方法 X（X）` | `java/io/BufferedReader.java`<br>`java/lang/CharSequence.java`<br>`java/lang/Process.java` |
| 8 | 20 | 3.0% | `[parse] X 期望标识符，实际 X` | `java/io/FileDescriptor.java`<br>`java/lang/invoke/MethodHandles.java`<br>`java/lang/reflect/Proxy.java` |
| 9 | 15 | 2.2% | `[compile] 只能把引用值转成引用类型（X）` | `java/security/KeyFactory.java`<br>`java/security/KeyPairGenerator.java`<br>`java/security/SecureRandom.java` |
| 10 | 13 | 1.9% | `[compile] X 不是数组：接收者静态类型是 ?（X）（X）` | `java/io/LineNumberReader.java`<br>`java/io/PipedInputStream.java`<br>`java/util/Calendar.java` |
| 11 | 12 | 1.8% | `[compile] 表达式嵌套过深：脏槽池只有 N 个（X）` | `java/lang/classfile/AnnotationValue.java`<br>`java/net/Inet6Address.java`<br>`java/net/URLConnection.java` |
| 12 | 10 | 1.5% | `[compile] 推断不出 X 的目标类型（X）（X）` | `java/io/ObjectInputFilter.java`<br>`java/lang/classfile/ClassTransform.java`<br>`java/lang/classfile/FieldTransform.java` |
| 13 | 10 | 1.5% | `[compile] 暂不支持的表达式: X { X: X(X), X: X("X"), X: X(X("X/X/X/X/X"), "X") }（X）` | `jdk/internal/random/L128X1024MixRandom.java`<br>`jdk/internal/random/L128X128MixRandom.java`<br>`jdk/internal/random/L128X256MixRandom.java` |
| 14 | 10 | 1.5% | `[compile] 赋值作为表达式时只支持局部变量：X(X("X"), "X")（X）` | `java/lang/PublicMethods.java`<br>`java/lang/Thread.java`<br>`java/math/MutableBigInteger.java` |
| 15 | 9 | 1.3% | `[compile] 找不到类 X（X）` | `com/sun/crypto/provider/DHKEM.java`<br>`java/lang/LiveStackFrameInfo.java`<br>`java/lang/ThreadGroup.java` |
| 16 | 8 | 1.2% | `[parse] X 意外的字符 X` | `com/sun/crypto/provider/PBES2Parameters.java`<br>`java/lang/reflect/ProxyGenerator.java`<br>`sun/security/pkcs/SignerInfo.java` |
| 17 | 8 | 1.2% | `[parse] X 无法解析的表达式，实际 X` | `java/io/ObjectStreamClass.java`<br>`java/lang/invoke/InvokerBytecodeGenerator.java`<br>`java/util/concurrent/Phaser.java` |
| 18 | 6 | 0.9% | `[compile] X 不是数组：接收者静态类型是 X/X/X（X）（X）` | `java/lang/ProcessImpl.java`<br>`java/text/DateFormatSymbols.java`<br>`java/util/Date.java` |
| 19 | 6 | 0.9% | `[compile] 整数字面量 X 解析失败` | `java/lang/Long.java`<br>`java/net/URI.java`<br>`jdk/internal/foreign/StringSupport.java` |
| 20 | 6 | 0.9% | `[compile] 未定义的变量 X` | `java/lang/constant/ConstantDescs.java`<br>`java/util/AbstractList.java`<br>`javax/crypto/JceSecurityManager.java` |
| 21 | 5 | 0.7% | `[compile] 暂不支持的表达式: X { X: X(X), X: X("X"), X: X("X") }（X）` | `java/lang/String.java`<br>`java/math/BigInteger.java`<br>`java/util/ArrayDeque.java` |
| 22 | 5 | 0.7% | `[parse] X 期望 X，实际 X` | `java/lang/invoke/BoundMethodHandle.java`<br>`java/util/function/Predicate.java`<br>`java/util/regex/CharPredicates.java` |
| 23 | 4 | 0.6% | `[compile] X-X 的被迭代表达式不是数组，也找不到 X 方法（X）（X）` | `java/lang/ThreadLocal.java`<br>`java/lang/classfile/attribute/snippet-files/PackageSnippets.java`<br>`jdk/internal/misc/CDS.java` |
| 24 | 4 | 0.6% | `[compile] 整数字面量 X 解析失败（X）` | `com/sun/crypto/provider/GHASH.java`<br>`java/lang/FdLibm.java`<br>`java/util/SplittableRandom.java` |
| 25 | 4 | 0.6% | `[compile] 类 X 的构造器里 X(N)/X(N) 必须是第一条语句` | `sun/security/ssl/CertificateRequest.java`<br>`sun/security/ssl/CipherSuite.java`<br>`sun/security/ssl/SSLHandshake.java` |
| 26 | 3 | 0.4% | `[compile] X 不是数组：接收者静态类型是 X/X/X/X/X（X）（X）` | `jdk/internal/classfile/impl/RawBytecodeHelper.java`<br>`jdk/internal/icu/text/BidiLine.java`<br>`jdk/internal/icu/text/BidiWriter.java` |
| 27 | 3 | 0.4% | `[compile] 自增/自减作为表达式时只支持局部变量：X { X: X("X"), X: X("X") }（X）` | `com/sun/crypto/provider/CounterMode.java`<br>`sun/security/rsa/MGF1.java`<br>`sun/security/ssl/Authenticator.java` |
| 28 | 3 | 0.4% | `[parse] X 期望标识符，实际 关键字 X` | `java/util/stream/FindOps.java`<br>`java/util/stream/Nodes.java`<br>`jdk/internal/foreign/AbstractMemorySegmentImpl.java` |
| 29 | 2 | 0.3% | `[compile] X 字面量 X 解析失败（X）` | `java/lang/Float.java`<br>`java/lang/runtime/ExactConversionsSupport.java` |
| 30 | 2 | 0.3% | `[compile] 引用类型只能比较 == / !=（X）` | `java/util/stream/StreamOpFlag.java`<br>`sun/util/locale/provider/BreakIteratorProviderImpl.java` |
| 31 | 2 | 0.3% | `[compile] 推断不出 X 的目标类型（X）` | `sun/security/provider/AbstractDrbg.java`<br>`sun/security/provider/certpath/ssl/SSLServerCertStore.java` |
| 32 | 2 | 0.3% | `[compile] 暂不支持的类型转换（X）` | `sun/net/InetAddressCachePolicy.java`<br>`sun/security/ssl/SSLConfiguration.java` |
| 33 | 2 | 0.3% | `[compile] 暂不支持的表达式: X { X: X(X), X: X("X"), X: X { X: X, X: X("X"), X: X("X") } }（X）` | `java/util/ArrayList.java`<br>`java/util/Vector.java` |
| 34 | 2 | 0.3% | `[compile] 未找到实例方法 X（X）` | `java/lang/invoke/NativeMethodHandle.java`<br>`java/util/ReverseOrderListView.java` |
| 35 | 2 | 0.3% | `[compile] 浮点字面量 X 解析失败` | `java/lang/Double.java`<br>`java/lang/Math.java` |
| 36 | 2 | 0.3% | `[parse] X 期望关键字 X，实际 X` | `java/text/MessageFormat.java`<br>`jdk/internal/classfile/impl/BytecodeHelpers.java` |
| 37 | 1 | 0.1% | `[compile] X 不是数组：接收者静态类型是 X/X/X/X（X）（X）` | `jdk/internal/foreign/SegmentBulkOperations.java` |
| 38 | 1 | 0.1% | `[compile] X 表达式只支持语句/赋值/返回位置（X）` | `java/util/stream/SliceOps.java` |
| 39 | 1 | 0.1% | `[compile] 推断不出方法引用的目标类型（X）` | `java/lang/constant/DynamicConstantDesc.java` |
| 40 | 1 | 0.1% | `[compile] 推断不出方法引用的目标类型（X）（X）` | `java/lang/VirtualThread.java` |
| 41 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: X(X), X: X { X: X("X"), X: X("X") }, X: X { X: X("X"), X: X { X: X, X: X { X: X("X"…` | `sun/security/provider/MD2.java` |
| 42 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: X(X), X: X("X"), X: X { X: X, X: "X", X: [], X: [] } }（X）` | `sun/text/RuleBasedBreakIterator.java` |
| 43 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: X(X), X: X("X"), X: X("N") }（X）` | `java/util/DualPivotQuicksort.java` |
| 44 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: X(X), X: X("X"), X: X(X("X"), "X") }（X）` | `java/text/CollationElementIterator.java` |
| 45 | 1 | 0.1% | `[compile] 未绑定实例方法引用要求 X 至少有一个参数（X）` | `sun/nio/ch/Poller.java` |
| 46 | 1 | 0.1% | `[compile] 继承环: X/X/X → X/X/X` | `java/lang/Object.java` |
| 47 | 1 | 0.1% | `[compile] 自增/自减作为表达式时只支持局部变量：X { X: X("X"), X: X("N") }（X）` | `sun/invoke/util/BytecodeDescriptor.java` |
| 48 | 1 | 0.1% | `[compile] 自增/自减的目标不是局部变量也不是字段：X（X）` | `javax/crypto/Cipher.java` |
| 49 | 1 | 0.1% | `[compile] 赋值作为表达式时只支持局部变量：X(X { X: X(X("X"), "X"), X: X("N") }, "X")（X）` | `jdk/internal/icu/text/BidiBase.java` |
| 50 | 1 | 0.1% | `[parse] X 无法解析的表达式，实际 关键字 X` | `sun/security/util/Cache.java` |
| 51 | 1 | 0.1% | `[parse] X 期望类型声明，实际 标识符 X` | `module-info.java` |

共 51 个桶。
