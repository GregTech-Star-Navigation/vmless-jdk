vmless —— 不带 JVM 的 Java 工具链
=================================

三个可执行文件，没有 jvm.dll，没有 HotSpot。.class 由我们自己生成，由我们自己执行。

  bin\compile_src.exe   .java → .class（自研前端 + 字节码生成）
  bin\rustjava.exe      解释器：直接跑 .class，不依赖任何 JDK
  bin\run_jit.exe       JIT：字节码 → Cranelift → 本机机器码后执行

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
* 这三个程序只认 .class 文件，不认 jar、不读模块系统。
* 目录布局：`out\Hello.class`。带包名的类会写成 `out\pkg\Hello.class`。
* 有不支持的字节码时，解释器报「未实现的字节码 0x..」，JIT 报「JIT 暂不支持字节码 0x..」，
  都不会静默给出错的结果。
* 环境变量 VMLESS_HEAP_MB 可压低堆上限（验收时用 2 或 8 强制触发回收）；
  run_jit 加 VMLESS_GC_STATS=1 会把分代 GC 统计打到 stderr。

支持范围、与真 JVM 的差别、基准数字
----------------------------------
见仓库的 README.md 与 docs/支持矩阵.md。这个包里只放运行所需的三个可执行文件。

许可
----
见同目录 LICENSE。
