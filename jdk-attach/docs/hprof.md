# HPROF 导出：格式、校验、以及"怎么证明它能被打开"

> 状态：2026-11-17 · 对应 `vmless-attach 0.1.0`

## 写出来的东西

`jmap -dump:format=b,file=x.hprof <pid>` → `JAVA PROFILE 1.0.2`，结构：

```
header  "JAVA PROFILE 1.0.2\0"  u4 id_size(=8)  u8 timestamp_ms
record  u1 tag  u4 usec_delta  u4 length  body
        0x01 STRING             id + utf8
        0x02 LOAD_CLASS         u4 serial, id class, u4 stack, id name_string
        0x1C HEAP_DUMP_SEGMENT  子记录（≤512KB，子记录不跨段）
        0x2C HEAP_DUMP_END
子记录
        0x05 ROOT_STICKY_CLASS   每个类一条
        0x20 CLASS_DUMP          id, u4 stack, id super, id loader, id signers,
                                 id prot, id res1, id res2, u4 instance_size,
                                 u2 cp_count [u2 index,u1 type,value],
                                 u2 static_count [u2 index,u1 type,value],
                                 u2 inst_field_count [u2 index,u1 type]
        0x21 INSTANCE_DUMP       id, u4 stack, id class, u4 num_bytes, bytes
```

三条硬约束（违反任何一条读端就认为文件坏了）：
1. **全部多字节字段是大端**（HPROF 是 BE 格式）。
2. `LOAD_CLASS` 必须在 `CLASS_DUMP` 之前。
3. 必须有 `HEAP_DUMP_END`，子记录不能跨段。

## 校验：`hprofcheck`

`jhat` 在 **JDK 9 就被删掉了**，`jhsdb` 只能看活进程不能读文件。所以本机没有任何
"真工具"能打开一个 hprof。能做到的最强验证是三件事：

### ① 严格解析器（按规范写，不猜、不跳过）

```bash
hprofcheck ours.hprof
```
```
bytes       = 20340
id_size     = 8
timestamp   = 1794885526740
strings     = 20
load_class  = 10
class_dump  = 10
instances   = 346
roots       = 10
segments    = 1
ended       = true
OK: 结构合法（header + 记录链 + 堆段子记录 + HEAP_DUMP_END 全部通过）
```

解析器和写端共用同一份格式知识 —— 这只能证明"自洽"。所以还要有 ②。

### ② 用真 `jmap -dump` 的文件反过来校验解析器

对 `java -Xmx32m Sleeper.java` 起的真 HotSpot 跑 `jmap -dump`，得到 18.9 MB 的
18 万条记录 / 15 个堆段，然后：

- **header 一致**：`id_size=8`（u4 BE）、timestamp BE、记录链 15 个段全部走通；
- **STRING / LOAD_CLASS / INSTANCE_DUMP / OBJ_ARRAY_DUMP / PRIM_ARRAY_DUMP /
  ROOT_* 全部一致**（这也顺带确认了 ② 里的类型码是 JVM 编号 2,4..11，不是 ASCII）；
- **不一致的地方**：JDK 25 的 `CLASS_DUMP` 在"常量池 / 静态字段"这一段（真文件里
  第一个类就是 `static_count=2`）用了我没能逆向出来的编码，解析器在那里对不上，
  报的第一个坏字节偏移是段内 71。

**结论：本库自己写出来的 dump 不经过那一段**（writer 恒写 `cp_count=0`、
`static_count=0`），所以上面 ① 的通过是有意义的；但"真 MAT 能打开"这件事在本机
**无法证明**，只能证伪不了。要最终确认，需要在一台装了 Eclipse MAT / VisualVM 的
机器上打开一次 —— 命令与期望输出见下节。

### ③ 可复现的确认步骤（在装了 MAT 的机器上）

```
1. 起被测 VM：  vmsim.exe --quiet
2. 导出：      jmap.exe -dump:format=b,file=D:\dump.hprof <pid>
3. 打开：      MAT → File → Open Heap Dump → 选 dump.hprof
               期望：能进 "Overview"，类数/实例数与 `jmap -histo` 的输出一致
               （histo 里的 instances 总数 = Overview 的 instance count）
```

## 已知未做

| 项 | 原因 |
| --- | --- |
| 字段名（`u2 index` 指向类的常量池）只写 0 常量池 | writer 写 `cp_count=0` 而实例字段引用索引 1..N —— 读端如果要按索引查名会查不到名字，但**大小与布局是对的**。要补需要先确认 JDK 25 的真实编码（见上）。 |
| 对象之间的引用关系（instance → instance） | 现在 `data` 里放的是字段字节，不解析引用图。MAT 会显示为"无引用"。 |
| 数组对象（`OBJ_ARRAY_DUMP` / `PRIM_ARRAY_DUMP`） | 未产出；`[B` 之类在 histo 里能看到，但 dump 里没有数组记录。 |
| 线程栈 / monitor 根 | 未产出（`ROOT_THREAD_OBJECT`、`ROOT_MONITOR_USED`）。 |

这些都不影响"文件结构合法"，但会影响"打开后能看到多少东西"。**如实标在这里，
而不是让报告看起来完整。**
