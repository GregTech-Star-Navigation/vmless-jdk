# vmless-gui

VMless 的四个图形/系统工具，**Win32 原生实现，不依赖 `java.desktop`**：

| 工具 | 干什么 | 数据来源 |
| --- | --- | --- |
| `jconsole` | 实时监控：概览 / 内存（`jstat -gc` 的 19 个计数器 + 折线）/ 线程 / 类直方图 | `vmless-attach`：perfdata 计数器 + attach 命令 |
| `jaccessinspector` | 无障碍（MSAA）树检查器：`role / name / state / rect` | `oleacc!AccessibleObjectFromWindow` + `IAccessible`，拿不到退 Win32 窗口树 |
| `jaccesswalker` | 桌面窗口遍历：按进程分组列出类名 / 标题 / 可见性 / 尺寸 | `EnumWindows` / `EnumChildWindows` |
| `jabswitch` | Java Access Bridge 开关（真注册表值） | `HKCU\…\Accessibility\Configuration\JavaAccessBridge` |

## 为什么不用 Swing/AWT

真 `jconsole` / `jaccess*` 是 Java 程序，靠 `java.desktop` 的 Swing 与 Java Access Bridge。
我们的 VM 没有 `java.desktop`，也没有 AWT 对等体（peer）实现 —— 为了四个工具去造一套 GUI
工具包，代价远大于收益。所以：**界面用 Win32（`user32` / `gdi32` / `comctl32`）自己画，
数据全部来自 `vmless-attach`**。目标是对齐「能看到同样的信息」，不是像素级复刻 Swing 观感。

## 构建

```bash
export PATH="$PATH:/c/Users/Administrator/.cargo/bin"
cd /d/vmless/jdk-gui
export CARGO_TARGET_DIR='D:\vmless\rt_gui1'
cargo build --release          # 四个二进制都在 $CARGO_TARGET_DIR/release/
```

依赖只有 `vmless-attach`（`../jdk-attach`，只读引用）。所有 Win32 函数用
`extern "system"` 手写声明 —— 没有 `windows` / `winapi` crate，所以编译快、体积可控。

## 跑起来

先起一个目标 VM（仓库里现成的模拟 VM）：

```bash
D:\vmless\batt\vmsim.exe --quiet --threads 6 --gc-interval 1500
```

然后：

```bash
# 1) jconsole：带 pid 直连；不带 -p 会弹出进程选择框
jconsole -p <pid>
jconsole -p <pid> -tab 1        # 直接打开"内存"页（0 概览 / 1 内存 / 2 线程 / 3 类）
jconsole -p <pid> -i 500        # 采样间隔 500ms

# 2) jaccesswalker：文本树（默认）/ GUI 树
jaccesswalker                       # 全部顶层窗口，按进程分组
jaccesswalker -pid <pid> -children  # 只看某个进程，连子窗口一起列
jaccesswalker -o gui

# 3) jaccessinspector：MSAA 树
jaccessinspector -title jconsole -d 3          # 文本树，最多 3 层
jaccessinspector -hwnd 0x000A1B2C
jaccessinspector -title jconsole -o gui        # 树形窗口
jaccessinspector -title jconsole -win32        # 强制 Win32 层（和 MSAA 对比用）

# 4) jabswitch
jabswitch -q        # 查询
jabswitch -enable   # 打开
jabswitch -disable  # 关闭
```

`jconsole` 的退出码与其它 `j*` 工具一致（见 `../jdk-attach/docs/cli.md`）：
`0` 正常、`1` 用法错误、`2` 目标不存在/不是 VMless VM、`3` 协议错误、`4` 超时、
`5` 连接失败、`6` 目标拒绝、`7` 窗口创建失败。`jaccess*` 用 `2` 表示找不到目标窗口、
`3` 表示窗口创建失败；`jabswitch` 用 `3` 表示注册表访问失败。

## 架构（三个关键点）

1. **UI 线程绝不碰网络。** 采样在独立线程里跑（`src/vm.rs` 的 `spawn_worker`），
   结果写进 `Arc<Mutex<Snapshot>>` 再 `PostMessage` 通知 UI。所以目标 VM 卡死时窗口
   照样能拖能关 —— 而不是白屏 2 秒。
2. **断线自动重连。** 每次采样前 `ping`；失败就把状态标成"连接断开"、丢掉连接，
   每 3 秒重试一次，状态栏持续显示重试次数。
3. **标签页必须提到 Z 序最前。** `SysTabControl32` 在 `TCM_INSERTITEM` 之后会把自己
   提到同级最前，页面窗口会被它盖住（现象：`WM_PAINT` 跑了、`BitBlt` 返回 1，屏幕上
   却是标签控件的灰底）。见 `ui::bring_to_top`。

## 已知限制

- `jconsole` 的堆/非堆进度条分母用 **committed**（与 HotSpot 的 "Committed" 同口径），
  右上角另标 `max`；`Metaspace` 与 `CompressedClassSpace` 分开显示，不做求和。
- 线程页的栈来自 `Thread.print` 的**文本**解析（协议里没有结构化线程接口）。
- 无障碍：MSAA 优先；有些窗口的提供者是 OLEACC 通用代理，只给子节点数量、不给每个
  子节点的属性 —— 这时逐个**子窗口**探 MSAA（输出里会写明用了哪条路）。两者都拿不到
  才退 Win32 层。
- 没有 manifest：控件用 comctl32 v5（不是 v6 的视觉主题）。深色模式不跟随系统。

## 自检

```bash
# 用 150% DPI 跑一遍布局（没有高 DPI 显示器时也能验）
GUI_DPI=144 jconsole -p <pid>

# 抓窗口里的控件树（不需要截图工具）
jaccesswalker -pid <jconsole pid> -children
jaccessinspector -pid <jconsole pid> -d 2
```
