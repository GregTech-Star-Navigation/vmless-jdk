# CHANGELOG

## 0.1.0 — 2026-11-17

首个版本：四个工具从"只有一个空 `lib.rs`"到可用。

### 新增

- **`jconsole`**：Win32 原生 GUI，四个标签页
  - 概览：堆/非堆/元空间进度条（used / committed，另标 max）、线程数、类加载数、
    运行时长、进程 CPU 时间（来自 OS `GetProcessTimes`，不依赖 VM 计数器）、GC 收集器表
  - 内存：`jstat -gc` 的 19 个计数器（列顺序、KB/秒/次数口径与真 jstat 逐列一致）
    + 最近 60 个采样点的自绘折线（年轻代 / 老年代 / 元空间的 已用÷容量）
  - 线程：`Thread.print` 解析出的线程列表（名字/状态/ID/守护）+ 选中线程的栈
  - 类：`GC.class_histogram` 的直方图，点表头按实例数 / 字节数排序
  - 数据每秒刷新；采样在独立线程，UI 线程不碰网络；断线每 3 秒重连并在状态栏显示次数
  - `-p <pid>` 直连，省略则弹出进程选择框（列出本机所有可 attach 的 VMless VM）
- **`jaccessinspector`**：`oleacc!AccessibleObjectFromWindow` + `IAccessible`
  （手写 vtable，28 槽）读出 role / name / state / rect 的树；
  MSAA 提供者不给子节点属性时逐个**子窗口**探 MSAA；两者都没有才退 Win32 窗口树，
  并在输出首行注明用的是哪一层。`-win32` 可强制对比。
- **`jaccesswalker`**：`EnumWindows` 按进程分组列出类名/标题/可见性/尺寸，
  `-pid` / `-title` 过滤，`-children` 带子窗口，`-o gui` 树形视图。
- **`jabswitch`**：读写真注册表值
  `HKCU\Software\Microsoft\Windows NT\CurrentVersion\Accessibility\Configuration\JavaAccessBridge`，
  `-enable` / `-disable` / `-q`，写完回读校验。

### 为什么这么选

- **不实现 Swing/AWT**：VM 没有 `java.desktop`，为四个工具造 GUI 工具包的代价远大于收益。
  界面直接用 `user32`/`gdi32`/`comctl32`，数据全部来自 `vmless-attach`。
- **依赖只有一个**：Win32 函数手写 `extern "system"` 声明，不引 `windows` crate。
  四个二进制 126–298 KB，远低于 2 MB 上限。
- **采样线程 + 快照**：不放 UI 线程，否则目标 VM 卡死会带着窗口一起卡 2 秒。

### 已知问题 / 刻意不做

- 控件走 comctl32 v5（没有 manifest），不跟随系统深色模式。
- 线程栈是 `Thread.print` 的文本解析（协议层没有结构化线程接口）。
- `jconsole` 的概览页在数据稀疏时下半部分留白（不画假数据）。
