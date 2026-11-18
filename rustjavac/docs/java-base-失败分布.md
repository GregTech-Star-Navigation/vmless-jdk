# java.base 编译失败分布

> 由 `examples/cover_dir.rs` 生成，改完代码重跑即可刷新。

- 源码根：`D:/vmless/jbsrc/java.base`
- classpath：`D:/vmless/patch/img/java.base`
- 生成时间：2026-11-18 13:39:01 +08:00

## 总览

| 文件总数 | 通过 | 编译失败 | 解析失败 | 通过率 |
|---|---|---|---|---|
| 3381 | 2497 | 805 | 79 | **73.9%** |

## 失败桶

| # | 次数 | 占失败比 | 错误骨架 | 样例文件 |
|---|---|---|---|---|
| 1 | 137 | 15.5% | `[compile] 未定义的变量 X（X）` | `com/sun/crypto/provider/ARCFOURCipher.java`<br>`com/sun/crypto/provider/JceKeyStore.java`<br>`com/sun/crypto/provider/SealedObjectForKeyProtector.java` |
| 2 | 127 | 14.4% | `[compile] 未找到实例方法 X（X）（X）` | `com/sun/security/ntlm/Server.java`<br>`java/io/Console.java`<br>`java/io/OutputStreamWriter.java` |
| 3 | 86 | 9.7% | `[compile] 未找到方法 X（X）` | `com/sun/crypto/provider/PKCS12PBECipherCore.java`<br>`com/sun/security/ntlm/Client.java`<br>`java/lang/Character.java` |
| 4 | 49 | 5.5% | `[compile] 暂不支持的表达式: X { X: X("X"), X: "X" }（X）` | `com/sun/crypto/provider/DHKEM.java`<br>`com/sun/crypto/provider/ML_KEM_Impls.java`<br>`java/lang/Class.java` |
| 5 | 32 | 3.6% | `[compile] 比较结果作为表达式时，前面不能有未消费的操作数（X）（X）` | `com/sun/crypto/provider/AESCrypt.java`<br>`com/sun/crypto/provider/ChaCha20Cipher.java`<br>`java/io/FilePermission.java` |
| 6 | 31 | 3.5% | `[parse] X 期望 X，实际 标识符 X` | `java/lang/Module.java`<br>`java/lang/classfile/attribute/ConstantValueAttribute.java`<br>`java/lang/classfile/snippet-files/PackageSnippets.java` |
| 7 | 22 | 2.5% | `[compile] X/X 前面不能有未消费的操作数（X）（X）` | `com/sun/crypto/provider/CipherCore.java`<br>`com/sun/crypto/provider/GaloisCounterMode.java`<br>`java/io/FileInputStream.java` |
| 8 | 21 | 2.4% | `[compile] 暂不支持的类型转换（X）（X）` | `java/lang/classfile/CodeBuilder.java`<br>`java/net/HttpConnectSocketImpl.java`<br>`java/net/SocksSocketImpl.java` |
| 9 | 20 | 2.3% | `[parse] X 期望标识符，实际 X` | `java/io/FileDescriptor.java`<br>`java/lang/invoke/MethodHandles.java`<br>`java/lang/reflect/Proxy.java` |
| 10 | 15 | 1.7% | `[compile] 只能把引用值转成引用类型（X）` | `java/security/KeyFactory.java`<br>`java/security/KeyPairGenerator.java`<br>`java/security/SecureRandom.java` |
| 11 | 11 | 1.2% | `[compile] 暂不支持的表达式: X { X: X("X/X/X"), X: "%X-X" }（X）` | `sun/security/ec/ECPrivateKeyImpl.java`<br>`sun/security/ec/XDHKeyAgreement.java`<br>`sun/security/ec/XDHKeyPairGenerator.java` |
| 12 | 10 | 1.1% | `[compile] 暂不支持的表达式: X { X: X(X), X: X("X"), X: X(X("X/X/X/X/X"), "X") }（X）` | `jdk/internal/random/L128X1024MixRandom.java`<br>`jdk/internal/random/L128X128MixRandom.java`<br>`jdk/internal/random/L128X256MixRandom.java` |
| 13 | 10 | 1.1% | `[compile] 未找到方法 X` | `java/lang/Integer.java`<br>`javax/net/ssl/SSLSocketFactory.java`<br>`jdk/internal/foreign/abi/aarch64/AArch64Architecture.java` |
| 14 | 10 | 1.1% | `[compile] 表达式嵌套过深：脏槽池只有 N 个（X）` | `java/lang/classfile/AnnotationValue.java`<br>`java/net/URLConnection.java`<br>`java/text/DecimalFormatSymbols.java` |
| 15 | 10 | 1.1% | `[compile] 赋值作为表达式时只支持局部变量：X(X("X"), "X")（X）` | `java/lang/PublicMethods.java`<br>`java/lang/Thread.java`<br>`java/math/MutableBigInteger.java` |
| 16 | 9 | 1.0% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X("X")), X: "X", X: [X("X")], X: [] } }（X）` | `java/util/PriorityQueue.java`<br>`java/util/Vector.java`<br>`java/util/concurrent/ArrayBlockingQueue.java` |
| 17 | 8 | 0.9% | `[compile] 暂不支持的表达式: X { X: X("X/X/X/X"), X: "X" }（X）` | `java/lang/invoke/MethodHandleProxies.java`<br>`java/lang/reflect/AnnotatedElement.java`<br>`java/net/SocketPermission.java` |
| 18 | 8 | 0.9% | `[parse] X 意外的字符 X` | `com/sun/crypto/provider/PBES2Parameters.java`<br>`java/lang/reflect/ProxyGenerator.java`<br>`sun/security/pkcs/SignerInfo.java` |
| 19 | 8 | 0.9% | `[parse] X 无法解析的表达式，实际 X` | `java/io/ObjectStreamClass.java`<br>`java/lang/invoke/InvokerBytecodeGenerator.java`<br>`java/util/concurrent/Phaser.java` |
| 20 | 7 | 0.8% | `[compile] 暂不支持的表达式: X { X: X("X"), X: "%X-X" }（X）` | `java/lang/Package.java`<br>`java/net/DatagramPacket.java`<br>`java/util/stream/GathererOp.java` |
| 21 | 7 | 0.8% | `[compile] 暂不支持的表达式: X { X: X, X: "X" }（X）` | `java/lang/Process.java`<br>`java/lang/VirtualThread.java`<br>`java/util/Scanner.java` |
| 22 | 6 | 0.7% | `[compile] X 目前只支持一个参数（X）` | `java/lang/IO.java`<br>`jdk/internal/vm/vector/Utils.java`<br>`sun/security/ssl/SSLLogger.java` |
| 23 | 6 | 0.7% | `[compile] 找不到类 X（X）` | `java/lang/LiveStackFrameInfo.java`<br>`java/lang/ThreadGroup.java`<br>`java/security/DomainLoadStoreParameter.java` |
| 24 | 6 | 0.7% | `[compile] 暂不支持的表达式: X { X: X("X/X/X"), X: "X" }（X）` | `java/time/format/DateTimeFormatter.java`<br>`jdk/internal/foreign/abi/CallingSequence.java`<br>`sun/net/ftp/impl/FtpClient.java` |
| 25 | 6 | 0.7% | `[compile] 暂不支持的表达式: X { X: X("X/X/X/X/X"), X: "X" }（X）` | `java/lang/classfile/attribute/EnclosingMethodAttribute.java`<br>`java/lang/classfile/attribute/InnerClassInfo.java`<br>`java/lang/classfile/attribute/MethodParameterInfo.java` |
| 26 | 6 | 0.7% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X(X("X([X { X: X { X: [\"X\"], X: [] }, X: \"X\", X: X(X { X: X(X(\"X\"…` | `java/lang/ProcessHandleImpl.java`<br>`java/lang/module/Resolver.java`<br>`java/lang/ref/FinalizerHistogram.java` |
| 27 | 5 | 0.6% | `[compile] 整数字面量 X 解析失败` | `java/lang/Long.java`<br>`java/net/URI.java`<br>`sun/net/www/ParseUtil.java` |
| 28 | 5 | 0.6% | `[compile] 暂不支持的表达式: X { X: X("X"), X: "X" }` | `java/security/spec/EncodedKeySpec.java`<br>`javax/crypto/SealedObject.java`<br>`javax/crypto/spec/SecretKeySpec.java` |
| 29 | 5 | 0.6% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X("X")), X: "X", X: [X("X"), X("X")], X: [] } }（X）` | `java/lang/Enum.java`<br>`java/time/zone/ZoneRulesProvider.java`<br>`jdk/internal/classfile/components/ClassRemapper.java` |
| 30 | 5 | 0.6% | `[compile] 未定义的变量 X` | `java/lang/classfile/TypeAnnotation.java`<br>`java/util/AbstractList.java`<br>`jdk/internal/foreign/abi/aarch64/windows/WindowsAArch64CallArranger.java` |
| 31 | 5 | 0.6% | `[parse] X 期望 X，实际 X` | `java/lang/invoke/BoundMethodHandle.java`<br>`java/util/function/Predicate.java`<br>`java/util/regex/CharPredicates.java` |
| 32 | 4 | 0.5% | `[compile] X-X 的被迭代表达式不是数组，也找不到 X 方法（X）（X）` | `java/net/CookieManager.java`<br>`jdk/internal/misc/TerminatingThreadLocal.java`<br>`sun/security/provider/DomainKeyStore.java` |
| 33 | 4 | 0.5% | `[compile] 整数字面量 X 解析失败（X）` | `com/sun/crypto/provider/GHASH.java`<br>`java/lang/FdLibm.java`<br>`java/util/SplittableRandom.java` |
| 34 | 4 | 0.5% | `[compile] 暂不支持的表达式: X { X: X(X), X: X("X"), X: X("X") }（X）` | `java/lang/String.java`<br>`java/math/BigInteger.java`<br>`java/util/ArrayDeque.java` |
| 35 | 4 | 0.5% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: "X", X: [X { X: X(X("X")), X: "X", X: [X("X")], X: [] }], X:…` | `java/util/function/DoubleUnaryOperator.java`<br>`java/util/function/Function.java`<br>`java/util/function/IntUnaryOperator.java` |
| 36 | 4 | 0.5% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X(X("X([X(X { X: X, X: \"X\", X: [X(\"X\")], X: [] }), X(X { X: X(X(\"X…` | `java/util/function/Consumer.java`<br>`java/util/function/DoubleConsumer.java`<br>`java/util/function/IntConsumer.java` |
| 37 | 4 | 0.5% | `[compile] 类 X 的构造器里 X(N)/X(N) 必须是第一条语句` | `sun/security/ssl/CertificateRequest.java`<br>`sun/security/ssl/CipherSuite.java`<br>`sun/security/ssl/SSLHandshake.java` |
| 38 | 3 | 0.3% | `[compile] 引用类型只能比较 == / !=（X）` | `java/text/ChoiceFormat.java`<br>`java/util/stream/StreamOpFlag.java`<br>`sun/util/locale/provider/BreakIteratorProviderImpl.java` |
| 39 | 3 | 0.3% | `[compile] 暂不支持的表达式: X { X: X { X: X("X/X/X/X"), X: "X" }, X: "X" }（X）` | `jdk/internal/foreign/abi/aarch64/CallArranger.java`<br>`jdk/internal/foreign/abi/fallback/FFIType.java`<br>`jdk/internal/foreign/abi/riscv64/linux/LinuxRISCV64CallArranger.java` |
| 40 | 3 | 0.3% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X(X(X("X([X { X: X { X: X, X: X { X: X(X(\"X\")), X: \"X\", X: [X(\"…` | `java/lang/classfile/ClassTransform.java`<br>`java/lang/classfile/FieldTransform.java`<br>`java/lang/classfile/MethodTransform.java` |
| 41 | 3 | 0.3% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X(X(X("X([X { X: X(\"X\"), X: \"X\", X: X(X { X: X(X(\"X\")), X: \"X…` | `java/time/chrono/ChronoLocalDateTime.java`<br>`java/time/chrono/ChronoZonedDateTime.java`<br>`java/util/PropertyPermission.java` |
| 42 | 3 | 0.3% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: X { X: X, X: "X", X: [X("X")], X: [] }, X: X { X: X(X("X")),…` | `java/util/function/DoublePredicate.java`<br>`java/util/function/IntPredicate.java`<br>`java/util/function/LongPredicate.java` |
| 43 | 3 | 0.3% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X("X") }（X）` | `java/lang/ModuleLayer.java`<br>`java/util/function/UnaryOperator.java`<br>`jdk/internal/vm/ThreadContainer.java` |
| 44 | 3 | 0.3% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X(X("X([X { X: X { X: [\"X\", \"X\"], X: [] }, X: \"X\", X: X(X { X: X …` | `java/util/stream/DoubleStream.java`<br>`java/util/stream/IntStream.java`<br>`java/util/stream/LongStream.java` |
| 45 | 3 | 0.3% | `[compile] 暂不支持的表达式: X { X: [], X: X { X: X(X("X/X/X")), X: "X", X: [X("X"), X { X: X("X"), X: X("X") }], X: []…` | `com/sun/crypto/provider/DESKey.java`<br>`com/sun/crypto/provider/DESedeKey.java`<br>`com/sun/crypto/provider/PBEKey.java` |
| 46 | 3 | 0.3% | `[compile] 自增/自减作为表达式时只支持局部变量：X { X: X("X"), X: X("X") }（X）` | `com/sun/crypto/provider/CounterMode.java`<br>`sun/security/rsa/MGF1.java`<br>`sun/security/ssl/Authenticator.java` |
| 47 | 3 | 0.3% | `[parse] X 期望标识符，实际 关键字 X` | `java/util/stream/FindOps.java`<br>`java/util/stream/Nodes.java`<br>`jdk/internal/foreign/AbstractMemorySegmentImpl.java` |
| 48 | 2 | 0.2% | `[compile] X 字面量 X 解析失败（X）` | `java/lang/Float.java`<br>`java/lang/runtime/ExactConversionsSupport.java` |
| 49 | 2 | 0.2% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X("X"), X: X { X: ["X/X/X", "X"], X: [] }, X: X } }（X）` | `jdk/internal/foreign/FunctionDescriptorImpl.java`<br>`jdk/internal/foreign/abi/s390/linux/LinuxS390CallArranger.java` |
| 50 | 2 | 0.2% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X { X: X(X("X")), X: "X", X: [], X: [] }), X: "X", X: [], X: [] …` | `java/time/chrono/HijrahChronology.java`<br>`jdk/internal/classfile/impl/InterfacesImpl.java` |
| 51 | 2 | 0.2% | `[compile] 暂不支持的表达式: X("\X \"X\": [\X {N}\X ]")（X）` | `sun/security/ssl/CertificateMessage.java`<br>`sun/security/ssl/EncryptedExtensions.java` |
| 52 | 2 | 0.2% | `[compile] 暂不支持的表达式: X("\X \"{N} ({N})\": '{'\X {N}\X '}'")（X）` | `sun/security/ssl/SSLExtension.java`<br>`sun/security/ssl/SSLExtensions.java` |
| 53 | 2 | 0.2% | `[compile] 浮点字面量 X 解析失败` | `java/lang/Double.java`<br>`java/lang/Math.java` |
| 54 | 2 | 0.2% | `[compile] 赋值作为表达式时目标既不是局部变量也不是字段：X（X）` | `java/util/ArrayPrefixHelpers.java`<br>`sun/security/provider/SeedGenerator.java` |
| 55 | 2 | 0.2% | `[parse] X 期望关键字 X，实际 X` | `java/text/MessageFormat.java`<br>`jdk/internal/classfile/impl/BytecodeHelpers.java` |
| 56 | 1 | 0.1% | `[compile] X 表达式只支持语句/赋值/返回位置（X）` | `java/util/stream/SliceOps.java` |
| 57 | 1 | 0.1% | `[compile] 暂不支持的类型转换（X）` | `sun/net/InetAddressCachePolicy.java` |
| 58 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: X(X), X: X { X: X("X"), X: X("X") }, X: X { X: X("X"), X: X { X: X, X: X { X: X("X"…` | `sun/security/provider/MD2.java` |
| 59 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: X(X), X: X("X"), X: X { X: X, X: "X", X: [], X: [] } }（X）` | `sun/text/RuleBasedBreakIterator.java` |
| 60 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: X(X), X: X("X"), X: X { X: X, X: X("X"), X: X("X") } }（X）` | `java/util/ArrayList.java` |
| 61 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: X(X), X: X("X"), X: X("N") }（X）` | `java/util/DualPivotQuicksort.java` |
| 62 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: X(X), X: X("X"), X: X(X("X"), "X") }（X）` | `java/text/CollationElementIterator.java` |
| 63 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X { X: X { X: X, X: X { X: X(X("X")), X: "X", X: [X("X"), X("X")], X…` | `java/util/function/BinaryOperator.java` |
| 64 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X { X: X { X: X, X: X("X"), X: X }, X: X, X: X { X: X(X), X: "X", X:…` | `java/util/concurrent/CompletionStage.java` |
| 65 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X { X: X(X { X: X(X("X")), X: "X", X: [], X: [] }), X: "X", X: [X { …` | `java/util/Map.java` |
| 66 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X { X: X(X("X")), X: "X", X: [X { X: X, X: "X", X: [X("X"), X("X")],…` | `java/util/function/BiFunction.java` |
| 67 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X { X: X(X("X")), X: "X", X: [X("X"), X { X: ["X"], X: X { X: X(X("X…` | `jdk/internal/classfile/components/CodeRelabeler.java` |
| 68 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X { X: X(X("X")), X: "X", X: [X("X")], X: [] } }` | `sun/net/www/protocol/http/DigestAuthentication.java` |
| 69 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X { X: X, X: X { X: X, X: "X", X: [X("X"), X("X")], X: [] }, X: X { …` | `java/util/function/BiPredicate.java` |
| 70 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X(X(X("X([X { X: X { X: X(\"X\"), X: X { X: [\"X\"], X: [X { X: [\"?…` | `jdk/internal/classfile/impl/AbstractUnboundModel.java` |
| 71 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X(X(X("X([X { X: X { X: X, X: X { X: X, X: \"X\", X: [X(\"X\"), X(\"…` | `java/util/concurrent/ConcurrentMap.java` |
| 72 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X(X(X("X([X { X: X { X: X, X: X(\"X\"), X: X }, X: X([X { X: X { X: …` | `java/security/UnresolvedPermissionCollection.java` |
| 73 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X(X(X("X([X { X: X { X: [\"X\", \"X\"], X: [X { X: [\"X\"], X: [] },…` | `java/util/stream/Collectors.java` |
| 74 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X(X(X("X([X { X: X(\"X\"), X: \"X\", X: X(X { X: X, X: X { X: X(X { …` | `java/lang/invoke/VarHandles.java` |
| 75 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X(X(X("X([X { X: X(\"X\"), X: \"X\", X: X(X { X: X, X: \"X\", X: [X(…` | `java/util/Comparator.java` |
| 76 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X(X(X("X([X(X { X: X, X: X(\"X\"), X: X { X: X, X: \"X\", X: [X(\"X\…` | `jdk/internal/foreign/abi/SharedUtils.java` |
| 77 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X(X(X("X([X(X { X: X, X: \"X\", X: [X(\"X\"), X(\"X\")], X: [] }), X…` | `java/util/function/BiConsumer.java` |
| 78 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X(X(X("X([X(X(X { X: X(X(\"X\")), X: \"X\", X: [X { X: X(X(\"X\")), …` | `java/time/chrono/ChronoLocalDate.java` |
| 79 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X(X) }` | `sun/security/provider/certpath/ssl/SSLServerCertStore.java` |
| 80 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X { X: ["X"], X: [] }, X: [] } }（X）` | `jdk/internal/foreign/abi/SoftReferenceCache.java` |
| 81 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X { X: ["X/X/X", "X"], X: [] }, X: [X("X"), X { X: X(X("X")), X: "…` | `sun/security/ec/ECDSAOperations.java` |
| 82 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X { X: ["X/X/X", "X"], X: [] }, X: [X("X"), X("X")] } }（X）` | `java/nio/charset/CoderResult.java` |
| 83 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X { X: ["X/X/X", "X"], X: [] }, X: [] } }（X）` | `sun/security/rsa/RSACore.java` |
| 84 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X { X: ["X/X/X/X/X", "X"], X: [] }, X: [] } }（X）` | `sun/net/www/protocol/http/AuthCacheImpl.java` |
| 85 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X { X: X(X { X: X(X { X: X(X { X: X(X("X")), X: "X", X: [X { X: …` | `javax/crypto/JceSecurityManager.java` |
| 86 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X { X: X(X { X: X(X { X: X(X { X: X(X("X")), X: "X", X: [X("[")]…` | `jdk/internal/vm/JcmdVThreadCommands.java` |
| 87 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X { X: X(X { X: X(X("X")), X: "X", X: [X { X: X("X"), X: "X" }],…` | `java/lang/SecurityManager.java` |
| 88 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X { X: X(X("X")), X: "X", X: [X { X: X("X"), X: "X" }], X: [] })…` | `jdk/internal/vm/Continuation.java` |
| 89 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X { X: X(X("X")), X: "X", X: [X { X: X(X("X")), X: "X", X: [], X…` | `jdk/internal/jrtfs/JrtFileSystem.java` |
| 90 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X { X: X(X("X")), X: "X", X: [X("N")], X: [] }), X: "X", X: [X {…` | `java/lang/ThreadLocal.java` |
| 91 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X { X: X(X("X")), X: "X", X: [], X: [] }), X: "X", X: [X("X")], …` | `java/util/spi/ToolProvider.java` |
| 92 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X { X: X, X: "X", X: [X("X")], X: [] }), X: "X", X: [X { X: [], …` | `java/lang/foreign/SymbolLookup.java` |
| 93 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X("X")), X: "X", X: [X { X: X(X { X: X(X("X")), X: "X", X: [], X…` | `java/lang/runtime/Carriers.java` |
| 94 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X("X")), X: "X", X: [X { X: X(X("X")), X: "X", X: [], X: [] }, X…` | `java/lang/WeakPairMap.java` |
| 95 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X("X")), X: "X", X: [X { X: X, X: X { X: X, X: X("X"), X: X { X:…` | `sun/launcher/SecuritySettings.java` |
| 96 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X("X")), X: "X", X: [X(" ")], X: [] } }（X）` | `java/lang/invoke/GenerateJLIClassesHelper.java` |
| 97 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X(X("X"), "X")), X: "X", X: [X("X"), X("X")], X: [] } }（X）` | `sun/security/provider/certpath/URICertStore.java` |
| 98 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X(X("X"), "X")), X: "X", X: [X("X")], X: [] } }（X）` | `java/lang/module/Configuration.java` |
| 99 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X(X("X/X/X/X/X"), "X")), X: "X", X: [X("X")], X: [] } }（X）` | `java/lang/classfile/attribute/ModuleRequireInfo.java` |
| 100 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: "X", X: [X { X: X(X("X")), X: "X", X: [X("/"), X("N")], X: […` | `jdk/internal/loader/BootLoader.java` |
| 101 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: "X", X: [X { X: X, X: "X", X: [X("X")], X: [] }, X("X")], X:…` | `java/util/zip/ZipFile.java` |
| 102 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: "X", X: [X("X"), X("X")], X: [] } }（X）` | `jdk/internal/foreign/abi/fallback/LibFallback.java` |
| 103 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: "X", X: [X("X"), X, X("X"), X("X")], X: [] } }（X）` | `java/util/Locale.java` |
| 104 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: "X", X: [X("X")], X: [] } }（X）` | `sun/security/util/DomainName.java` |
| 105 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: X { X: X(X { X: X, X: "X", X: [X("X")], X: [] }), X: "X", X:…` | `sun/util/calendar/ZoneInfoFile.java` |
| 106 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: X { X: X(X("X")), X: "X", X: [X("X"), X("-X")], X: [] }, X: …` | `sun/security/tools/keytool/Main.java` |
| 107 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: X { X: X(X("X")), X: "X", X: [X("X")], X: [] } } }（X）` | `jdk/internal/module/DefaultRoots.java` |
| 108 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: X { X: X(X("X")), X: "X", X: [X("X")], X: [] }, X: X } }（X）` | `jdk/internal/vm/SharedThreadContainer.java` |
| 109 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: X { X: X(X("X")), X: "X", X: [], X: [] }, X: X { X: X, X: X …` | `java/util/random/RandomGeneratorFactory.java` |
| 110 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: X { X: X(X("X")), X: "X", X: [], X: [] }, X: X("X") } }（X）` | `java/util/ResourceBundle.java` |
| 111 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: X { X: X(X("X")), X: "X", X: [], X: [] }, X: X(X("X/X/X/X/X"…` | `jdk/internal/module/ModulePath.java` |
| 112 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: X { X: X(X("X")), X: "X", X: [], X: [] }, X: X(X(X("X"), "X"…` | `sun/security/util/HostnameChecker.java` |
| 113 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: X { X: X, X: X { X: X, X: X { X: X(X("X")), X: "X", X: [X("X…` | `sun/util/locale/provider/LocaleResources.java` |
| 114 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: X { X: X, X: X { X: X, X: X { X: X, X: X { X: X, X: X { X: X…` | `jdk/internal/classfile/impl/ClassImpl.java` |
| 115 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: X("X"), X: X } }（X）` | `java/nio/channels/spi/AbstractSelectableChannel.java` |
| 116 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: X("X"), X: X("X") } }（X）` | `sun/security/util/DerInputStream.java` |
| 117 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X { X: X("X"), X: [X { X: [X(X("X-X-X"))], X: [X(X(X("X"), "X"))], X: X…` | `sun/util/locale/provider/LocaleProviderAdapter.java` |
| 118 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X(X("X([X { X: X { X: X, X: X { X: X(X { X: X(X(\"X\")), X: \"X\", X: […` | `java/security/Provider.java` |
| 119 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X(X("X([X { X: X { X: [\"X\"], X: [X { X: [\"X\"], X: [] }] }, X: \"X\"…` | `java/util/stream/Stream.java` |
| 120 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X(X("X([X { X: X { X: [\"X\"], X: [] }, X: \"X\", X: X(X { X: X(X { X: …` | `sun/security/ssl/StatusResponseManager.java` |
| 121 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X(X("X([X { X: X([X(X(X { X: X, X: \"X\", X: [X(\"X\"), X { X: X, X: \"…` | `sun/security/x509/X509CertImpl.java` |
| 122 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X(X("X([X { X: X(\"X\"), X: \"X\", X: X(X { X: X, X: \"X\", X: [X(\"X\"…` | `jdk/internal/foreign/abi/NativeEntryPoint.java` |
| 123 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X(X("X([X { X: X, X: \"X\", X: X(X { X: X { X: [\"X\"], X: [X { X: [\"X…` | `java/lang/runtime/SwitchBootstraps.java` |
| 124 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X(X("X([X(X { X: X(X(\"X\")), X: \"X\", X: [X { X: X(X(\"X\")), X: \"X\…` | `jdk/internal/module/ModuleHashes.java` |
| 125 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X(X("X([X(X { X: X(X(\"X\")), X: \"X\", X: [X(\"X\")], X: [] }), X { X:…` | `java/nio/channels/Selector.java` |
| 126 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X(X("X([X(X { X: X(X(\"X\")), X: \"X\", X: [X(\"X\")], X: [] }), X(X(X)…` | `java/lang/StackWalker.java` |
| 127 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X(X("X([X(X { X: X, X: X { X: X(\"X\"), X: X(\"X\") }, X: X { X: X(X(\"…` | `java/util/Arrays.java` |
| 128 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X(X("X([])"))) }（X）` | `java/lang/classfile/ClassFile.java` |
| 129 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X) }（X）` | `sun/nio/fs/WindowsFileCopy.java` |
| 130 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X { X: X { X: ["X"], X: [] }, X: [X("X"), X("X")] } }（X）` | `jdk/internal/module/ModuleReferences.java` |
| 131 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X { X: X { X: ["X/X", "X"], X: [] }, X: [X { X: X, X: X("X X X: "), X: X("X"…` | `sun/security/ec/ECDSASignature.java` |
| 132 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X { X: X { X: ["X/X/X/X", "X"], X: [] }, X: [X("X"), X("X"), X { X: X, X: "X…` | `jdk/internal/foreign/layout/SequenceLayoutImpl.java` |
| 133 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X { X: X(X("X")), X: "X", X: [X("[%X %X] %X"), X("X"), X("X"), X { X: X(X("X…` | `jdk/internal/logger/AbstractLoggerWrapper.java` |
| 134 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X { X: X(X("X/X/X")), X: "X", X: [X { X: X { X: ["X"], X: [] }, X: [] }, X {…` | `java/lang/CharSequence.java` |
| 135 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X { X: X(X("X/X/X")), X: "X", X: [X("X"), X(" ")], X: [] } }（X）` | `javax/security/auth/callback/PasswordCallback.java` |
| 136 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X { X: X(X("X/X/X/X/X")), X: "X", X: [X("X"), X("X")], X: [] } }（X）` | `java/lang/foreign/MemoryLayout.java` |
| 137 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X { X: X, X: "X", X: [X("X"), X("X"), X("X")], X: [] } }（X）` | `sun/reflect/annotation/AnnotationParser.java` |
| 138 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X("N") }（X）` | `sun/nio/ch/SocketInputStream.java` |
| 139 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X("X") }（X）` | `sun/nio/ch/SocketAdaptor.java` |
| 140 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X(X(X("X([X { X: X { X: X, X: X { X: X(X(\"X\")), X: \"X\", X: [], X: [] } }…` | `java/util/concurrent/Executors.java` |
| 141 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X(X(X("X([X { X: X { X: X, X: X { X: X(X(\"X\")), X: \"X\", X: [], X: [] }, …` | `jdk/internal/io/JdkConsoleImpl.java` |
| 142 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X(X(X("X([X { X: X(X(\"X\")), X: \"X\", X: X(X(X, \"X\")) }, X { X: X(\"X\")…` | `java/lang/AbstractStringBuilder.java` |
| 143 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X(X(X("X([X { X: X([X { X: X { X: X, X: X(\"X\"), X: X(\"X\") }, X: X([X { X…` | `sun/nio/fs/WindowsSecurity.java` |
| 144 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X(X(X("X([X { X: X([X(X { X: X(X(\"X\")), X: \"X\", X: [], X: [] })]), X: [X…` | `java/nio/file/Files.java` |
| 145 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X(X(X("X([X { X: X([X(X(X { X: X(X(\"X\")), X: \"X\", X: [X { X: X(X { X: X,…` | `java/lang/ClassLoader.java` |
| 146 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X(X(X("X([X(X { X: X(X(\"X\")), X: \"X\", X: [X(\"X\"), X { X: X(\"X\"), X: …` | `com/sun/crypto/provider/PBKDF2KeyImpl.java` |
| 147 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X(X(X("X([X(X { X: X(X(\"X\")), X: \"X\", X: [], X: [] }), X(X { X: X(X(\"X\…` | `jdk/internal/classfile/impl/TransformImpl.java` |
| 148 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X(X(X("X([X([X { X: X { X: [\"X\"], X: [] }, X: \"X\", X: X(X { X: X { X: [\…` | `jdk/internal/misc/CDS.java` |
| 149 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X(X) }（X）` | `sun/nio/ch/Poller.java` |
| 150 | 1 | 0.1% | `[compile] 暂不支持的表达式: X("\X \"X X X\": {N}\X \"X X X\": '{'\X {N}\X '}'")（X）` | `sun/security/ssl/CertStatusExtension.java` |
| 151 | 1 | 0.1% | `[compile] 暂不支持的表达式: X("\X \"X X\": '{'\X \"X X\": '{'\X {N}\X '}',\X '}'")（X）` | `sun/security/ssl/ECDHClientKeyExchange.java` |
| 152 | 1 | 0.1% | `[compile] 暂不支持的表达式: X("\X \"X X\": '{'\X \"X\": '{'\X \"X X\": \"{N}\"\X \"X X\": '{'\X {N}\X '}',\X '}',\X \"…` | `sun/security/ssl/ECDHServerKeyExchange.java` |
| 153 | 1 | 0.1% | `[compile] 暂不支持的表达式: X("\X \"X X\": '{'\X \"X\": '{'\X \"X\": '{'\X {N}\X '}',\X '}'\X '}'")（X）` | `sun/security/ssl/DHClientKeyExchange.java` |
| 154 | 1 | 0.1% | `[compile] 暂不支持的表达式: X("\X \"X X\": '{'\X \"X\": '{'\X \"X\": '{'\X {N}\X '}',\X \"X\": '{'\X {N}\X '}',\X \"X\…` | `sun/security/ssl/DHServerKeyExchange.java` |
| 155 | 1 | 0.1% | `[compile] 暂不支持的表达式: X("\X \"X X\": '{'\X \"X\": '{'\X \"X\": '{'\X {N}\X '}',\X \"X\": '{'\X {N}\X '}'\X '}',\…` | `sun/security/ssl/RSAServerKeyExchange.java` |
| 156 | 1 | 0.1% | `[compile] 暂不支持的表达式: X("\X \"X X\": '{'\X \"X\": {N}\X \"X\": '{'\X {N}\X '}'\X '}'")（X）` | `sun/security/ssl/RSAClientKeyExchange.java` |
| 157 | 1 | 0.1% | `[compile] 暂不支持的表达式: X("\X \"X\" : '{'\X {N}\X '}'")（X）` | `sun/security/ssl/SessionTicketExtension.java` |
| 158 | 1 | 0.1% | `[compile] 暂不支持的表达式: X("\X \"X\": '{'\X \"X X\" : \"{N}\",\X \"X\" : \"{N}\",\X '}'")（X）` | `sun/security/ssl/HelloVerifyRequest.java` |
| 159 | 1 | 0.1% | `[compile] 暂不支持的表达式: X("\X \"X\": '{'\X \"X X\" : \"{N}\",\X \"X\" : \"{N}\",\X \"X X\" : \"{N}\",\X \"X\" : \"…` | `sun/security/ssl/ClientHello.java` |
| 160 | 1 | 0.1% | `[compile] 暂不支持的表达式: X("\X \"X\": '{'\X \"X X\": '{'\X {N}\X '}'\X '}'")（X）` | `sun/security/ssl/Finished.java` |
| 161 | 1 | 0.1% | `[compile] 暂不支持的表达式: X("\X \"X\": '{'\X \"X\" : \"{N}\",\X \"X\" : '{'\X {N}\X '}''}'")（X）` | `sun/security/ssl/NewSessionTicket.java` |
| 162 | 1 | 0.1% | `[compile] 暂不支持的表达式: X("\X \"X\": '{'\X \"X\" : \"{N}\",\X \"X\": \"{N}\"\X '}'")（X）` | `sun/security/ssl/Alert.java` |
| 163 | 1 | 0.1% | `[compile] 暂不支持的表达式: X("\X \"X\": '{'\X \"X\": '{'\X {N}\X '}' \"X\": \"{N}\",\X '}'")（X）` | `sun/security/ssl/PreSharedKeyExtension.java` |
| 164 | 1 | 0.1% | `[compile] 暂不支持的表达式: X("\X \"X\": '{'\X \"X\": '{'\X {N}\X '}'\X '}'")（X）` | `sun/security/ssl/CertificateVerify.java` |
| 165 | 1 | 0.1% | `[compile] 暂不支持的表达式: X("\X \"X\": '{'\X \"X\": {N}\X '}'")（X）` | `sun/security/ssl/KeyUpdate.java` |
| 166 | 1 | 0.1% | `[compile] 暂不支持的表达式: X("\X \"X\": '{'\X {N}\X '}',")（X）` | `sun/security/ssl/CookieExtension.java` |
| 167 | 1 | 0.1% | `[compile] 暂不支持的表达式: X("\X \"{N}\": '{'\X \"X X\" : \"{N}\",\X \"X\" : \"{N}\",\X \"X X\" : \"{N}\",\X \"X X\" …` | `sun/security/ssl/ServerHello.java` |
| 168 | 1 | 0.1% | `[compile] 暂不支持的表达式: X("\X\X '{'\X \"X X\": {N}\X \"X\": '{'\X {N}\X '}'\X '}',")（X）` | `sun/security/ssl/KeyShareExtension.java` |
| 169 | 1 | 0.1% | `[compile] 未找到实例方法 X（X）` | `java/util/ReverseOrderListView.java` |
| 170 | 1 | 0.1% | `[compile] 继承环: X/X/X → X/X/X` | `java/lang/Object.java` |
| 171 | 1 | 0.1% | `[compile] 自增/自减作为表达式时只支持局部变量：X { X: X("X"), X: X("N") }（X）` | `sun/invoke/util/BytecodeDescriptor.java` |
| 172 | 1 | 0.1% | `[compile] 自增/自减作为表达式时只支持局部变量：X(X("X"), "X")（X）` | `jdk/internal/icu/impl/NormalizerImpl.java` |
| 173 | 1 | 0.1% | `[compile] 自增/自减的目标不是局部变量也不是字段：X（X）` | `javax/crypto/Cipher.java` |
| 174 | 1 | 0.1% | `[compile] 赋值作为表达式时只支持局部变量：X(X { X: X(X("X"), "X"), X: X("N") }, "X")（X）` | `jdk/internal/icu/text/BidiBase.java` |
| 175 | 1 | 0.1% | `[parse] X 无法解析的表达式，实际 关键字 X` | `sun/security/util/Cache.java` |
| 176 | 1 | 0.1% | `[parse] X 期望类型声明，实际 标识符 X` | `module-info.java` |

共 176 个桶。
