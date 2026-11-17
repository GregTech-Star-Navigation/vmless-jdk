# AGENTS.md —— 给 agent 的项目约定

状态：`0.1.0`（2026-11-17）。这份文件里的命令都实测过；未验证的会标「（未验证）」。

## 这是什么

`jconsole` / `jaccessinspector` / `jaccesswalker` / `jabswitch` 四个工具的 Win32 原生实现。
**不是** Java 程序，不依赖 `java.desktop`，不实现 Swing。

## 确切命令

```bash
export PATH="$PATH:/c/Users/Administrator/.cargo/bin"
export CARGO_TARGET_DIR='D:\vmless\rt_gui1'      # 别用默认 target/，多个 agent 会互相踩
cd /d/vmless/jdk-gui

cargo build --release                             # 四个二进制
cargo build --release --bin jconsole              # 只建一个
```

跑起来需要一个目标 VM：

```bash
D:\vmless\batt\vmsim.exe --quiet --threads 6 --gc-interval 1500     # 记下 pid
jconsole -p <pid> &                                  # 后台起，别在前台等
jaccesswalker -pid <jconsole pid> -children           # 读它的控件树（不需要截图工具）
jaccessinspector -pid <jconsole pid> -d 2
```

验证 150% 布局（没有高 DPI 显示器也能验）：`GUI_DPI=144 jconsole -p <pid>`。

## 目录约定

| 文件 | 职责 | 不许放什么 |
| --- | --- | --- |
| `src/win.rs` | Win32 裸 FFI（结构体、常量、`extern "system"`） | 业务逻辑、界面布局 |
| `src/ui.rs` | 窗口类 / 消息循环 / 双缓冲自绘 / 控件封装 | 具体页面的画法 |
| `src/vm.rs` | 采样线程、计数器提取、`Thread.print` 解析 | 任何 Win32 绘图 |
| `src/winenum.rs` | 窗口枚举 | 无障碍相关 |
| `src/msaa.rs` | `IAccessible` 的手写 vtable | 界面 |
| `src/reg.rs` | 注册表读写 | 别处直接调 `Reg*` |
| `src/pick.rs` | 进程选择对话框 | 别的对话框 |
| `src/bin/*.rs` | 四个工具各自的 UI 与命令行 | 共享逻辑（放 lib） |

## 代码约定

- 一律用 **W（UTF-16）** API。字符串进出只走 `win::wide` / `windows` / `wide_z` / `from_wide`。
- **交给 Win32 的字符串必须是 `wide_z`**（带结尾 NUL）。`wide("")` 是空 `Vec`，
  `as_mut_ptr()` 给出**非空但悬挂**的指针 —— comctl32 按 NUL 读就会访问违例（踩过一次，
  症状是"插列表项时崩在 LVM_SETITEMTEXTW"）。
- 颜色只用 `ui::rgb(r, g, b)`。`COLORREF` 是 `0x00BBGGRR`，手写十六进制一定会颠倒红蓝。
- 尺寸只用 `scale(dpi, n)`（96-DPI 设计值 → 实际像素）。不要写死像素。
- GDI 对象走 RAII（`Font` / `Canvas`），或在 `WM_DESTROY` 里先 `DestroyWindow` 子控件再析构。
- `SetTimer` 必须有配对的 `KillTimer`；跨线程只 `PostMessage` + `Arc<Mutex<_>>`，不要传裸 `Box`。

## 禁区

- **不要改** `../jdk-attach`（只读消费）、`../rustjavac`、`../repo`、`../dllwork`、其它 `jdk-*`。
- **不要** 在 `Cargo.toml` 里加依赖（只有 `vmless-attach`）。需要新的 Win32 函数就手写声明。
- **不要** 给 `thread_local` 状态起「临时」名字或在 `with_app` 持锁期间调会回调自己的
  `SendMessage`（`try_borrow_mut` 失败即跳过，这是有意的）。
- 不要按进程名杀 node/python；清理测试进程用 `Get-Process <名字> | Stop-Process`。

## 验收怎么做

```bash
# 1) 数据在变：读两次状态栏 statictext，采样 # 应该不同
jaccessinspector -pid <jconsole pid> -d 1 | grep statictext
sleep 2
jaccessinspector -pid <jconsole pid> -d 1 | grep statictext

# 2) 错误路径：必须非 0 退出码 + 明确报错
jconsole -p 999999; echo $?          # 期望 2
jaccessinspector -pid 999999; echo $? # 期望 2

# 3) 断线：杀掉目标 VM，状态栏应变成「连接断开：… · 每 3 秒重试（第 N 次）」
```
