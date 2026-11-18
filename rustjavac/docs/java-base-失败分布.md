# java.base 编译失败分布

> 由 `examples/cover_dir.rs` 生成，改完代码重跑即可刷新。

- 源码根：`D:/vmless/jbsrc/java.base`
- classpath：`D:/vmless/patch/img/java.base`
- 生成时间：2026-11-18 09:11:42 +08:00

## 总览

| 文件总数 | 通过 | 编译失败 | 解析失败 | 通过率 |
|---|---|---|---|---|
| 3381 | 2096 | 1081 | 204 | **62.0%** |

## 失败桶

| # | 次数 | 占失败比 | 错误骨架 | 样例文件 |
|---|---|---|---|---|
| 1 | 242 | 18.8% | `[compile] 未找到实例方法 X（X）` | `com/sun/crypto/provider/DESedeKeyGenerator.java`<br>`com/sun/crypto/provider/ML_KEM.java`<br>`com/sun/crypto/provider/OAEPParameters.java` |
| 2 | 203 | 15.8% | `[compile] 未定义的变量 X（X）` | `com/sun/crypto/provider/AESKeyWrap.java`<br>`com/sun/crypto/provider/AESKeyWrapPadded.java`<br>`com/sun/crypto/provider/ARCFOURCipher.java` |
| 3 | 100 | 7.8% | `[parse] X 期望 X，实际 X` | `java/io/ObjectInputStream.java`<br>`java/io/StreamTokenizer.java`<br>`java/lang/ClassValue.java` |
| 4 | 78 | 6.1% | `[compile] 暂不支持 X 模式绑定（X）（X）` | `java/io/OutputStreamWriter.java`<br>`java/io/PrintStream.java`<br>`java/io/PrintWriter.java` |
| 5 | 71 | 5.5% | `[compile] 未找到方法 X（X）` | `com/sun/crypto/provider/TlsKeyMaterialGenerator.java`<br>`com/sun/security/ntlm/Client.java`<br>`java/lang/ProcessEnvironment.java` |
| 6 | 43 | 3.3% | `[compile] 找不到类 X（X）` | `java/lang/LiveStackFrameInfo.java`<br>`java/lang/ThreadGroup.java`<br>`java/security/DomainLoadStoreParameter.java` |
| 7 | 42 | 3.3% | `[compile] 字段 X 在父类与子类中重复声明（X）` | `com/sun/crypto/provider/SunJCE.java`<br>`java/io/LineNumberReader.java`<br>`java/io/ProxyingConsole.java` |
| 8 | 36 | 2.8% | `[compile] 自增/自减的目标不是局部变量：X（X）` | `com/sun/crypto/provider/Poly1305.java`<br>`java/io/BufferedInputStream.java`<br>`java/io/BufferedOutputStream.java` |
| 9 | 32 | 2.5% | `[compile] 暂不支持的类型转换（X）（X）` | `java/lang/Runtime.java`<br>`java/lang/invoke/VarHandleGuards.java`<br>`java/net/HttpConnectSocketImpl.java` |
| 10 | 32 | 2.5% | `[parse] X 期望 X，实际 关键字 X` | `com/sun/crypto/provider/DHKEM.java`<br>`com/sun/crypto/provider/PKCS12PBECipherCore.java`<br>`java/lang/VirtualThread.java` |
| 11 | 29 | 2.3% | `[compile] 赋值作为表达式时目标不是局部变量：X（X）` | `java/io/DataInputStream.java`<br>`java/lang/Enum.java`<br>`java/lang/ScopedValue.java` |
| 12 | 26 | 2.0% | `[parse] X 期望 X，实际 标识符 X` | `java/lang/Module.java`<br>`java/lang/classfile/attribute/ConstantValueAttribute.java`<br>`java/lang/classfile/snippet-files/PackageSnippets.java` |
| 13 | 22 | 1.7% | `[compile] 暂不支持的表达式: X { X: X("X"), X: "X" }（X）` | `java/lang/Class.java`<br>`java/lang/classfile/CompoundElement.java`<br>`java/lang/classfile/MethodSignature.java` |
| 14 | 21 | 1.6% | `[compile] 比较结果作为表达式时，前面不能有未消费的操作数（X）（X）` | `com/sun/crypto/provider/AESCrypt.java`<br>`com/sun/crypto/provider/ChaCha20Cipher.java`<br>`java/lang/reflect/Constructor.java` |
| 15 | 17 | 1.3% | `[parse] X 无法解析的表达式，实际 关键字 X` | `java/lang/reflect/Proxy.java`<br>`java/lang/runtime/Carriers.java`<br>`java/lang/runtime/SwitchBootstraps.java` |
| 16 | 16 | 1.2% | `[compile] 未找到实例方法 X` | `java/io/File.java`<br>`java/lang/invoke/VarHandleByteArrayBase.java`<br>`java/net/Inet6Address.java` |
| 17 | 15 | 1.2% | `[compile] 未定义的变量 X` | `com/sun/crypto/provider/DHParameterGenerator.java`<br>`java/nio/charset/StandardCharsets.java`<br>`java/nio/file/TempFileHelper.java` |
| 18 | 11 | 0.9% | `[compile] 暂不支持的表达式: X { X: X("X/X/X"), X: "%X-X" }（X）` | `sun/security/ec/ECPrivateKeyImpl.java`<br>`sun/security/ec/XDHKeyAgreement.java`<br>`sun/security/ec/XDHKeyPairGenerator.java` |
| 19 | 10 | 0.8% | `[compile] X/X 前面不能有未消费的操作数（X）（X）` | `com/sun/crypto/provider/CipherCore.java`<br>`com/sun/crypto/provider/GaloisCounterMode.java`<br>`java/io/FileInputStream.java` |
| 20 | 10 | 0.8% | `[parse] X 期望标识符，实际 X` | `java/io/FileDescriptor.java`<br>`java/security/PEMRecord.java`<br>`jdk/internal/classfile/impl/AnnotationImpl.java` |
| 21 | 8 | 0.6% | `[parse] X 意外的字符 X` | `com/sun/crypto/provider/PBES2Parameters.java`<br>`java/lang/reflect/ProxyGenerator.java`<br>`sun/security/pkcs/SignerInfo.java` |
| 22 | 7 | 0.5% | `[compile] 暂不支持的表达式: X { X: X("X"), X: "%X-X" }（X）` | `java/lang/Package.java`<br>`java/net/DatagramPacket.java`<br>`java/util/stream/GathererOp.java` |
| 23 | 7 | 0.5% | `[parse] X 无法解析的表达式，实际 X` | `java/io/ObjectStreamClass.java`<br>`java/util/concurrent/Phaser.java`<br>`java/util/concurrent/locks/StampedLock.java` |
| 24 | 6 | 0.5% | `[compile] X 目前只支持一个参数（X）` | `java/lang/IO.java`<br>`jdk/internal/vm/vector/Utils.java`<br>`sun/security/ssl/SSLLogger.java` |
| 25 | 6 | 0.5% | `[compile] 暂不支持的表达式: X { X: X("X/X/X"), X: "X" }（X）` | `java/time/format/DateTimeFormatter.java`<br>`jdk/internal/foreign/abi/CallingSequence.java`<br>`sun/net/ftp/impl/FtpClient.java` |
| 26 | 6 | 0.5% | `[compile] 暂不支持的表达式: X(X("X { X: X(\"X\"), X: [X { X: [X(X(X(\"X\"), \"X\"))], X: [X(X(\"X\"))], X: X }, X { X:…` | `java/math/RoundingMode.java`<br>`jdk/internal/foreign/abi/aarch64/AArch64Architecture.java`<br>`jdk/internal/foreign/abi/ppc64/PPC64Architecture.java` |
| 27 | 6 | 0.5% | `[compile] 赋值作为表达式时只支持局部变量：X(X("X"), "X")（X）` | `java/util/LinkedHashMap.java`<br>`java/util/concurrent/DelayScheduler.java`<br>`java/util/concurrent/Exchanger.java` |
| 28 | 5 | 0.4% | `[compile] 暂不支持的表达式: X { X: X("X"), X: "X" }` | `java/security/spec/EncodedKeySpec.java`<br>`javax/crypto/SealedObject.java`<br>`javax/crypto/spec/SecretKeySpec.java` |
| 29 | 5 | 0.4% | `[compile] 暂不支持的表达式: X { X: X("X/X/X/X/X"), X: "X" }（X）` | `java/lang/classfile/attribute/EnclosingMethodAttribute.java`<br>`java/lang/classfile/attribute/InnerClassInfo.java`<br>`java/lang/classfile/attribute/MethodParameterInfo.java` |
| 30 | 5 | 0.4% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X("X([X { X: X { X: [\"X\"], X: [] }, X: \"X\", X: X(X { X: X(X(\"X\"))…` | `java/lang/ProcessHandleImpl.java`<br>`java/lang/module/Resolver.java`<br>`java/lang/ref/FinalizerHistogram.java` |
| 31 | 5 | 0.4% | `[compile] 表达式嵌套过深：脏槽池只有 N 个（X）` | `java/lang/classfile/AnnotationValue.java`<br>`java/text/DecimalFormatSymbols.java`<br>`jdk/internal/util/regex/IndicConjunctBreak.java` |
| 32 | 5 | 0.4% | `[compile] 赋值作为表达式时只支持局部变量：X(X, "X")（X）` | `com/sun/crypto/provider/PBKDF2KeyImpl.java`<br>`java/util/ArrayPrefixHelpers.java`<br>`java/util/concurrent/atomic/DoubleAccumulator.java` |
| 33 | 4 | 0.3% | `[compile] X-X 的被迭代表达式不是数组，也找不到 X 方法（X）（X）` | `java/net/CookieManager.java`<br>`jdk/internal/misc/TerminatingThreadLocal.java`<br>`sun/security/provider/DomainKeyStore.java` |
| 34 | 4 | 0.3% | `[compile] 引用类型只能比较 == / !=（X）` | `java/text/ChoiceFormat.java`<br>`java/util/stream/StreamOpFlag.java`<br>`jdk/internal/util/regex/Grapheme.java` |
| 35 | 4 | 0.3% | `[compile] 整数字面量 X 解析失败（X）` | `com/sun/crypto/provider/GHASH.java`<br>`java/lang/FdLibm.java`<br>`java/util/SplittableRandom.java` |
| 36 | 4 | 0.3% | `[compile] 暂不支持的表达式: X { X: X, X: "X" }（X）` | `java/lang/Process.java`<br>`sun/nio/ch/DatagramChannelImpl.java`<br>`sun/nio/ch/NioSocketImpl.java` |
| 37 | 4 | 0.3% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: "X", X: [X { X: X(X("X")), X: "X", X: [X("X")], X: [] }], X:…` | `java/util/function/DoubleUnaryOperator.java`<br>`java/util/function/Function.java`<br>`java/util/function/IntUnaryOperator.java` |
| 38 | 4 | 0.3% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X("X([X(X { X: X, X: \"X\", X: [X(\"X\")], X: [] }), X(X { X: X(X(\"X\"…` | `java/util/function/Consumer.java`<br>`java/util/function/DoubleConsumer.java`<br>`java/util/function/IntConsumer.java` |
| 39 | 4 | 0.3% | `[compile] 暂不支持的表达式: X(X("X { X: X(\"X\"), X: [X { X: [X(X(\"N\"))], X: [X(X(\"X\"))], X: X }, X { X: [X(X(\"N\…` | `java/time/chrono/IsoEra.java`<br>`java/time/chrono/MinguoEra.java`<br>`java/time/chrono/ThaiBuddhistEra.java` |
| 40 | 4 | 0.3% | `[compile] 类 X 的构造器里 X(N)/X(N) 必须是第一条语句` | `sun/security/ssl/CertificateRequest.java`<br>`sun/security/ssl/CipherSuite.java`<br>`sun/security/ssl/SSLHandshake.java` |
| 41 | 3 | 0.2% | `[compile] 暂不支持的表达式: X { X: X("X/X/X/X"), X: "X" }（X）` | `jdk/internal/module/ModuleBootstrap.java`<br>`jdk/internal/module/ModulePathValidator.java`<br>`jdk/internal/vm/ThreadContainers.java` |
| 42 | 3 | 0.2% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X(X("X([X { X: X { X: X, X: X { X: X(X(\"X\")), X: \"X\", X: [X(\"X\…` | `java/lang/classfile/ClassTransform.java`<br>`java/lang/classfile/FieldTransform.java`<br>`java/lang/classfile/MethodTransform.java` |
| 43 | 3 | 0.2% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X("X")), X: "X", X: [X("X"), X("X")], X: [] } }（X）` | `java/time/zone/ZoneRulesProvider.java`<br>`jdk/internal/classfile/components/ClassRemapper.java`<br>`jdk/internal/loader/LoaderPool.java` |
| 44 | 3 | 0.2% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X("X")), X: "X", X: [X("X")], X: [] } }（X）` | `java/util/concurrent/CopyOnWriteArrayList.java`<br>`jdk/internal/foreign/SystemLookup.java`<br>`jdk/internal/module/SystemModuleFinders.java` |
| 45 | 3 | 0.2% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: X { X: X, X: "X", X: [X("X")], X: [] }, X: X { X: X(X("X")),…` | `java/util/function/DoublePredicate.java`<br>`java/util/function/IntPredicate.java`<br>`java/util/function/LongPredicate.java` |
| 46 | 3 | 0.2% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X("X") }（X）` | `java/lang/ModuleLayer.java`<br>`java/util/function/UnaryOperator.java`<br>`jdk/internal/vm/ThreadContainer.java` |
| 47 | 3 | 0.2% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X("X([X { X: X { X: [\"X\", \"X\"], X: [] }, X: \"X\", X: X(X { X: X { …` | `java/util/stream/DoubleStream.java`<br>`java/util/stream/IntStream.java`<br>`java/util/stream/LongStream.java` |
| 48 | 3 | 0.2% | `[compile] 暂不支持的表达式: X { X: [], X: X { X: X(X("X/X/X")), X: "X", X: [X("X"), X { X: X("X"), X: X("X") }], X: []…` | `com/sun/crypto/provider/DESKey.java`<br>`com/sun/crypto/provider/DESedeKey.java`<br>`com/sun/crypto/provider/PBEKey.java` |
| 49 | 3 | 0.2% | `[compile] 自增/自减作为表达式时只支持局部变量：X { X: X("X"), X: X("X") }（X）` | `com/sun/crypto/provider/CounterMode.java`<br>`sun/security/rsa/MGF1.java`<br>`sun/security/ssl/Authenticator.java` |
| 50 | 2 | 0.2% | `[compile] 暂不支持的表达式: X { X: X(X), X: X("X"), X: X("X") }（X）` | `java/math/BigInteger.java`<br>`jdk/internal/util/random/RandomSupport.java` |
| 51 | 2 | 0.2% | `[compile] 暂不支持的表达式: X { X: X(X), X: X("X"), X: X(X("X/X/X/X/X"), "X") }（X）` | `jdk/internal/random/Xoroshiro128PlusPlus.java`<br>`jdk/internal/random/Xoshiro256PlusPlus.java` |
| 52 | 2 | 0.2% | `[compile] 未找到方法 X` | `jdk/internal/perf/PerfCounter.java`<br>`sun/nio/cs/MS950_HKSCS.java` |
| 53 | 2 | 0.2% | `[parse] X 期望标识符，实际 关键字 X` | `java/util/stream/FindOps.java`<br>`jdk/internal/foreign/AbstractMemorySegmentImpl.java` |
| 54 | 1 | 0.1% | `[compile] X 字面量 X 解析失败（X）` | `java/lang/runtime/ExactConversionsSupport.java` |
| 55 | 1 | 0.1% | `[compile] 只能把引用值转成引用类型（X）` | `sun/nio/ch/AsynchronousSocketChannelImpl.java` |
| 56 | 1 | 0.1% | `[compile] 整数字面量 X 解析失败` | `sun/net/www/ParseUtil.java` |
| 57 | 1 | 0.1% | `[compile] 暂不支持的类型转换（X）` | `sun/net/InetAddressCachePolicy.java` |
| 58 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: X(X), X: X { X: X("X"), X: X("X") }, X: X { X: X("X"), X: X { X: X, X: X { X: X("X"…` | `sun/security/provider/MD2.java` |
| 59 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: X(X), X: X("X"), X: X("N") }（X）` | `java/util/DualPivotQuicksort.java` |
| 60 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X { X: X { X: X, X: X { X: X(X("X")), X: "X", X: [X("X"), X("X")], X…` | `java/util/function/BinaryOperator.java` |
| 61 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X { X: X { X: X, X: X("X"), X: X }, X: X, X: X { X: X(X), X: "X", X:…` | `java/util/concurrent/CompletionStage.java` |
| 62 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X { X: X(X { X: X(X("X")), X: "X", X: [], X: [] }), X: "X", X: [X { …` | `java/util/Map.java` |
| 63 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X { X: X(X("X")), X: "X", X: [X { X: X, X: "X", X: [X("X"), X("X")],…` | `java/util/function/BiFunction.java` |
| 64 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X { X: X(X("X")), X: "X", X: [X("X"), X { X: ["X"], X: X { X: X(X("X…` | `jdk/internal/classfile/components/CodeRelabeler.java` |
| 65 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X { X: X, X: X { X: X, X: "X", X: [X("X"), X("X")], X: [] }, X: X { …` | `java/util/function/BiPredicate.java` |
| 66 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X(X("X([X { X: X { X: X(\"X\"), X: X { X: [\"X\"], X: [X { X: [\"?\"…` | `jdk/internal/classfile/impl/AbstractUnboundModel.java` |
| 67 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X(X("X([X { X: X { X: X, X: X { X: X, X: \"X\", X: [X(\"X\"), X(\"X\…` | `java/util/concurrent/ConcurrentMap.java` |
| 68 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X(X("X([X { X: X { X: X, X: X(\"X\"), X: X }, X: X([X { X: X { X: [\…` | `java/security/UnresolvedPermissionCollection.java` |
| 69 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X(X("X([X { X: X(\"X\"), X: \"X\", X: X(X { X: X(X(\"X\")), X: \"X\"…` | `java/time/chrono/ChronoLocalDateTime.java` |
| 70 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X(X("X([X { X: X(\"X\"), X: \"X\", X: X(X { X: X, X: \"X\", X: [X(\"…` | `java/util/Comparator.java` |
| 71 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X(X("X([X(X { X: X, X: \"X\", X: [X(\"X\"), X(\"X\")], X: [] }), X(X…` | `java/util/function/BiConsumer.java` |
| 72 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X(X("X([X(X(X { X: X(X(\"X\")), X: \"X\", X: [X { X: X(X(\"X\")), X:…` | `java/time/chrono/ChronoLocalDate.java` |
| 73 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X", "X"], X: X(X) }` | `sun/security/provider/certpath/ssl/SSLServerCertStore.java` |
| 74 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X { X: ["X"], X: [] }, X: [] } }（X）` | `jdk/internal/foreign/abi/SoftReferenceCache.java` |
| 75 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X { X: ["X/X/X", "X"], X: [] }, X: [X("X"), X { X: X(X("X")), X: "…` | `sun/security/ec/ECDSAOperations.java` |
| 76 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X { X: ["X/X/X", "X"], X: [] }, X: [X("X"), X("X")] } }（X）` | `java/nio/charset/CoderResult.java` |
| 77 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X { X: ["X/X/X", "X"], X: [] }, X: [] } }（X）` | `sun/security/rsa/RSACore.java` |
| 78 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X { X: ["X/X/X/X/X", "X"], X: [] }, X: [] } }（X）` | `sun/net/www/protocol/http/AuthCacheImpl.java` |
| 79 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X("X"), X: X { X: ["X/X/X", "X"], X: [] }, X: X } }（X）` | `jdk/internal/foreign/FunctionDescriptorImpl.java` |
| 80 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X { X: X(X { X: X(X { X: X(X { X: X(X("X")), X: "X", X: [X { X: …` | `javax/crypto/JceSecurityManager.java` |
| 81 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X { X: X(X { X: X(X { X: X(X { X: X(X("X")), X: "X", X: [X("[")]…` | `jdk/internal/vm/JcmdVThreadCommands.java` |
| 82 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X { X: X(X { X: X(X("X")), X: "X", X: [X { X: X("X"), X: "X" }],…` | `java/lang/SecurityManager.java` |
| 83 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X { X: X(X("X")), X: "X", X: [X { X: X(X("X")), X: "X", X: [], X…` | `jdk/internal/jrtfs/JrtFileSystem.java` |
| 84 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X { X: X(X("X")), X: "X", X: [X("N")], X: [] }), X: "X", X: [X {…` | `java/lang/ThreadLocal.java` |
| 85 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X { X: X(X("X")), X: "X", X: [], X: [] }), X: "X", X: [], X: [] …` | `jdk/internal/classfile/impl/InterfacesImpl.java` |
| 86 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X { X: X, X: "X", X: [X("X")], X: [] }), X: "X", X: [X { X: [], …` | `java/lang/foreign/SymbolLookup.java` |
| 87 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X("X")), X: "X", X: [X { X: X(X("X")), X: "X", X: [], X: [] }, X…` | `java/lang/WeakPairMap.java` |
| 88 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X("X")), X: "X", X: [X(" ")], X: [] } }（X）` | `java/lang/invoke/GenerateJLIClassesHelper.java` |
| 89 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X(X("X"), "X")), X: "X", X: [X("X"), X("X")], X: [] } }（X）` | `sun/security/provider/certpath/URICertStore.java` |
| 90 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X(X("X"), "X")), X: "X", X: [X("X")], X: [] } }（X）` | `java/lang/module/Configuration.java` |
| 91 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X(X(X("X/X/X/X/X"), "X")), X: "X", X: [X("X")], X: [] } }（X）` | `java/lang/classfile/attribute/ModuleRequireInfo.java` |
| 92 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: "X", X: [X { X: X(X("X")), X: "X", X: [X("/"), X("N")], X: […` | `jdk/internal/loader/BootLoader.java` |
| 93 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: "X", X: [X("X"), X("X")], X: [] } }（X）` | `jdk/internal/foreign/abi/fallback/LibFallback.java` |
| 94 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: X { X: X(X("X")), X: "X", X: [X("X"), X("-X")], X: [] }, X: …` | `sun/security/tools/keytool/Main.java` |
| 95 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: X { X: X(X("X")), X: "X", X: [X("X")], X: [] } } }（X）` | `jdk/internal/module/DefaultRoots.java` |
| 96 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: X { X: X(X("X")), X: "X", X: [X("X")], X: [] }, X: X } }（X）` | `jdk/internal/vm/SharedThreadContainer.java` |
| 97 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: X { X: X(X("X")), X: "X", X: [], X: [] }, X: X(X("X/X/X/X/X"…` | `jdk/internal/module/ModulePath.java` |
| 98 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: X { X: X(X("X")), X: "X", X: [], X: [] }, X: X(X(X("X"), "X"…` | `sun/security/util/HostnameChecker.java` |
| 99 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: X { X: X, X: X { X: X, X: X { X: X, X: X { X: X, X: X { X: X…` | `jdk/internal/classfile/impl/ClassImpl.java` |
| 100 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: X("X"), X: X } }（X）` | `java/nio/channels/spi/AbstractSelectableChannel.java` |
| 101 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X { X: X, X: X("X"), X: X("X") } }（X）` | `sun/security/util/DerInputStream.java` |
| 102 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X("X { X: X(\"X\"), X: [X { X: [X(X(\"X-X-X\"))], X: [X(X(X(\"X\"), \"X…` | `sun/util/locale/provider/LocaleProviderAdapter.java` |
| 103 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X("X([X { X: X { X: [\"X\"], X: [X { X: [\"X\"], X: [] }] }, X: \"X\", …` | `java/util/stream/Stream.java` |
| 104 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X("X([X { X: X { X: [\"X\"], X: [] }, X: \"X\", X: X(X { X: X(X { X: X(…` | `sun/security/ssl/StatusResponseManager.java` |
| 105 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X("X([X(X { X: X(X(\"X\")), X: \"X\", X: [X { X: X(X(\"X\")), X: \"X\",…` | `jdk/internal/module/ModuleHashes.java` |
| 106 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X("X([X(X { X: X(X(\"X\")), X: \"X\", X: [X(\"X\")], X: [] }), X { X: X…` | `java/nio/channels/Selector.java` |
| 107 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X("X([X(X { X: X(X(\"X\")), X: \"X\", X: [X(\"X\")], X: [] }), X(X(X))]…` | `java/lang/StackWalker.java` |
| 108 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X("X([X(X { X: X, X: X { X: X(\"X\"), X: X(\"X\") }, X: X { X: X(X(\"X\…` | `java/util/Arrays.java` |
| 109 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: ["X"], X: X(X("X([])")) }（X）` | `java/lang/classfile/ClassFile.java` |
| 110 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X { X: X { X: ["X"], X: [] }, X: [X("X"), X("X")] } }（X）` | `jdk/internal/module/ModuleReferences.java` |
| 111 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X { X: X { X: ["X/X", "X"], X: [] }, X: [X { X: X, X: X("X X X: "), X: X("X"…` | `sun/security/ec/ECDSASignature.java` |
| 112 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X { X: X { X: ["X/X/X/X", "X"], X: [] }, X: [X("X"), X("X"), X { X: X, X: "X…` | `jdk/internal/foreign/layout/SequenceLayoutImpl.java` |
| 113 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X { X: X(X("X")), X: "X", X: [X("[%X %X] %X"), X("X"), X("X"), X { X: X(X("X…` | `jdk/internal/logger/AbstractLoggerWrapper.java` |
| 114 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X { X: X(X("X/X/X")), X: "X", X: [X { X: X { X: ["X"], X: [] }, X: [] }, X {…` | `java/lang/CharSequence.java` |
| 115 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X { X: X(X("X/X/X")), X: "X", X: [X("X"), X(" ")], X: [] } }（X）` | `javax/security/auth/callback/PasswordCallback.java` |
| 116 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X { X: X(X("X/X/X/X/X")), X: "X", X: [X("X"), X("X")], X: [] } }（X）` | `java/lang/foreign/MemoryLayout.java` |
| 117 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X { X: X, X: "X", X: [X("X"), X("X"), X("X")], X: [] } }（X）` | `sun/reflect/annotation/AnnotationParser.java` |
| 118 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X("N") }（X）` | `sun/nio/ch/SocketInputStream.java` |
| 119 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X(X("X([X { X: X { X: X, X: X { X: X(X(\"X\")), X: \"X\", X: [], X: [] }, X:…` | `jdk/internal/io/JdkConsoleImpl.java` |
| 120 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X(X("X([X { X: X([X(X(X { X: X(X(\"X\")), X: \"X\", X: [X { X: X(X { X: X, X…` | `java/lang/ClassLoader.java` |
| 121 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X(X("X([X([X { X: X { X: [\"X\"], X: [] }, X: \"X\", X: X(X { X: X { X: [\"X…` | `jdk/internal/misc/CDS.java` |
| 122 | 1 | 0.1% | `[compile] 暂不支持的表达式: X { X: [], X: X(X) }（X）` | `sun/nio/ch/Poller.java` |
| 123 | 1 | 0.1% | `[compile] 暂不支持的表达式: X("\X \"X\": '{'\X \"X\" : \"{N}\",\X \"X\": \"{N}\"\X '}'")（X）` | `sun/security/ssl/Alert.java` |
| 124 | 1 | 0.1% | `[compile] 暂不支持的表达式: X("\X \"{N} ({N})\": '{'\X {N}\X '}'")（X）` | `sun/security/ssl/SSLExtension.java` |
| 125 | 1 | 0.1% | `[compile] 暂不支持的表达式: X(X("X { X: X { X: X(X { X: X(X(\"X\")), X: \"X\", X: [], X: [] }), X: \"X\", X: [X(X(\"X\…` | `sun/security/util/SignatureFileVerifier.java` |
| 126 | 1 | 0.1% | `[compile] 暂不支持的表达式: X(X("X { X: X { X: X(X(\"X\")), X: \"X\", X: [X(\"N\")], X: [] }, X: [X { X: [X(X(\"X\"))]…` | `java/io/ObjectStreamField.java` |
| 127 | 1 | 0.1% | `[compile] 暂不支持的表达式: X(X("X { X: X { X: X, X: X(\"X\"), X: X(\"N\") }, X: [X { X: [X(X(\"N\"))], X: [X(X(X(\"X\…` | `java/lang/CharacterData.java` |
| 128 | 1 | 0.1% | `[compile] 暂不支持的表达式: X(X("X { X: X { X: X, X: \"X\", X: [], X: [] }, X: [X { X: [X(X(\"N\"))], X: [X(X { X: X, …` | `java/time/format/SignStyle.java` |
| 129 | 1 | 0.1% | `[compile] 暂不支持的表达式: X(X("X { X: X(\"X\"), X: [X { X: [X(X(X(\"X\"), \"X\"))], X: [X(X { X: X(X(\"X\")), X: \"X…` | `com/sun/crypto/provider/ConstructKeys.java` |
| 130 | 1 | 0.1% | `[compile] 暂不支持的表达式: X(X("X { X: X(\"X\"), X: [X { X: [X(X(\"N\")), X(X(\"N\")), X(X(\"N\")), X(X(\"N\")), X(X(…` | `java/util/Properties.java` |
| 131 | 1 | 0.1% | `[compile] 暂不支持的表达式: X(X("X { X: X(\"X\"), X: [X { X: [X(X(\"N\"))], X: [X(X { X: X(X(\"X\")), X: \"X\", X: [X(…` | `jdk/internal/reflect/DirectMethodHandleAccessor.java` |
| 132 | 1 | 0.1% | `[compile] 暂不支持的表达式: X(X("X { X: X(\"X\"), X: [X { X: [X(X(\"N\"))], X: [X(X { X: X(X(\"X\")), X: \"X\", X: [],…` | `jdk/internal/reflect/DirectConstructorHandleAccessor.java` |
| 133 | 1 | 0.1% | `[compile] 暂不支持的表达式: X(X("X { X: X(\"X\"), X: [X { X: [X(X(\"N\"))], X: [X(X { X: X, X: \"X\", X: [X { X: X(\"X…` | `java/lang/invoke/StringConcatFactory.java` |
| 134 | 1 | 0.1% | `[compile] 暂不支持的表达式: X(X("X { X: X(\"X\"), X: [X { X: [X(X(\"N\"))], X: [X(X(X(\"X\"), \"X\"))], X: X }, X { X:…` | `jdk/internal/vm/Continuation.java` |
| 135 | 1 | 0.1% | `[compile] 暂不支持的表达式: X(X("X { X: X(\"X\"), X: [X { X: [X(X(\"X\")), X(X(\"X\"))], X: [X(X(\"X\"))], X: X }, X {…` | `jdk/internal/util/Architecture.java` |
| 136 | 1 | 0.1% | `[compile] 暂不支持的表达式: X(X("X { X: X(\"X\"), X: [X { X: [X(X(\"X\"))], X: [X(X(X))], X: X }, X { X: [X(X(\"X\"))]…` | `java/io/FilePermission.java` |
| 137 | 1 | 0.1% | `[compile] 父类里未找到方法 X（X）` | `jdk/internal/logger/DefaultLoggerFinder.java` |
| 138 | 1 | 0.1% | `[compile] 继承环: X/X/X → X/X/X` | `java/lang/Object.java` |
| 139 | 1 | 0.1% | `[compile] 自增/自减作为表达式时只支持局部变量：X(X("X"), "X")（X）` | `jdk/internal/icu/impl/NormalizerImpl.java` |
| 140 | 1 | 0.1% | `[compile] 赋值作为表达式时只支持局部变量：X { X: X("X"), X: X { X: X(X(X(X("X"), "X"), "X")), X: "X", X: [], X: [] } }（X）` | `java/lang/invoke/VarForm.java` |
| 141 | 1 | 0.1% | `[compile] 赋值作为表达式时只支持局部变量：X { X: X("X"), X: X { X: X, X: X("X"), X: X("X") } }（X）` | `com/sun/crypto/provider/CipherFeedback.java` |
| 142 | 1 | 0.1% | `[compile] 赋值作为表达式时只支持局部变量：X(X { X: X(X("X"), "X"), X: X("N") }, "X")（X）` | `jdk/internal/icu/text/BidiBase.java` |
| 143 | 1 | 0.1% | `[parse] X 意外的 X` | `sun/security/provider/ML_DSA.java` |
| 144 | 1 | 0.1% | `[parse] X 期望类型声明，实际 标识符 X` | `module-info.java` |

共 144 个桶。
