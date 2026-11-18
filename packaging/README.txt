vmless —— 不带 JVM 的 Java 工具链
=================================

三个运行时 + 32 个 JDK 工具，没有 jvm.dll，没有 HotSpot。.class 由我们自己生成，由我们自己执行。

运行时（bin\）
  compile_src.exe   .java → .class（自研前端 + 字节码生成）
  rustjava.exe      解释器：直接跑 .class，不依赖任何 JDK
  run_jit.exe       JIT：字节码 → Cranelift → 本机机器码后执行

JDK 工具（bin\，名字与 JDK 里的一致）
  镜像/模块   jimage jmod jlink
  文档/调试   javadoc jdb jshell jrunscript jwebserver jpackage
  检查        jdeprscan jnativescan serialver rmiregistry
  启动/进程   javaw jps jcmd jinfo jmap jstack jstat jstatd
  密钥/签名   keytool jarsigner ktab
  Kerberos    kinit klist
  JFR         jfr jhsdb
  无障碍      jabswitch jaccessinspector jaccesswalker jconsole

  这些工具**都是真实现**（附件协议、PKCS12、MIT keytab、JFR 2.1 二进制、Win32 窗口）。
  它们服务的是本项目的 VM：`jps` 能列出真 HotSpot 进程（读它的 hsperfdata），
  但 `jstack`/`jmap`/`jstat`/`jcmd`/`jinfo` 只对挂了 attach 服务端的 VM 有效 ——
  对真 HotSpot 进程会明确报「有 perfdata，但没有 attach 端点」并退出，
  不会假装成功。真 HotSpot 请用 JDK 自带的同名工具。

快速上手
--------
1. 写一个 Hello.java：

       public class Hello {
           public static void main(String[] a) {
               System.out.println("hello, vmless");
           }
       }

2. 编译（第二个参数是输出目录，同时当 classpath）：

       bin\compile_src.exe Hello.java out

3. 跑（解释器 / JIT 二选一）：

       bin\rustjava.exe out\Hello.class
       bin\run_jit.exe  out\Hello.class

   两者输出必须逐字节一致 —— 这也是验收脚本的做法。

几点须知
--------
* 这三个运行时只认 .class 文件，不认 jar、不读模块系统。
* 目录布局：`out\Hello.class`。带包名的类会写成 `out\pkg\Hello.class`。
* 有不支持的字节码时，解释器报「未实现的字节码 0x..」，JIT 报「JIT 暂不支持字节码 0x..」，
  都不会静默给出错的结果。
* 环境变量 VMLESS_HEAP_MB 可压低堆上限（验收时用 2 或 8 强制触发回收）；
  run_jit 加 VMLESS_GC_STATS=1 会把分代 GC 统计打到 stderr。

支持范围、与真 JVM 的差别、基准数字
----------------------------------
见仓库的 README.md 与 docs/支持矩阵.md。`bin` 里除三个运行时外还带全套 JDK 工具，
用法与真 JDK 的同名命令基本一致（`<工具> -h` 都有中文帮助）。

许可
----
见同目录 LICENSE。
