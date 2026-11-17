//! vmless 的图形化监控工具（jconsole / jaccess* / jabswitch），Win32 原生，不依赖 java.desktop
//!
//! # 为什么不用 Swing/AWT
//!
//! 真 `jconsole` 是个 Swing 程序，`jaccessinspector` / `jaccesswalker` 也是 Java 写的。
//! 我们的 VM 没有 `java.desktop`，也没有整套 AWT 对等体（peer）实现 —— 为了四个工具去
//! 造一套 GUI 工具包，代价远大于收益。
//!
//! 所以这四个工具**直接用 Win32**：
//!
//! - 界面：`user32` + `gdi32` + `comctl32`（原生控件 + 自绘）
//! - 数据：`vmless-attach`（perfdata 计数器 / attach 命令 / 堆直方图）
//! - 无障碍：`oleacc` 的 `IAccessible`（MSAA），拿不到就退到 Win32 窗口树并在输出里写明
//!
//! 功能上对齐的是"能看见同样的信息"，不是"像素级复刻 Swing 的观感"。
//!
//! # 模块
//!
//! | 模块 | 职责 |
//! | --- | --- |
//! | [`win`] | Win32 裸 FFI（手写声明，零依赖） |
//! | [`ui`] | 窗口类 / 消息循环 / 双缓冲自绘 / 控件封装 |
//! | [`vm`] | attach 采样（计数器、线程栈、类直方图）+ 断线重连 |
//! | [`winenum`] | 窗口树枚举（jaccesswalker / jaccessinspector 共用） |
//! | [`msaa`] | `IAccessible` 的 Rust 侧最小封装 |
//! | [`reg`] | 注册表读写（jabswitch） |

pub mod msaa;
pub mod reg;
pub mod ui;
pub mod vm;
pub mod win;
pub mod winenum;

/// 四个工具的版本串（`-v` / `--version` 与窗口标题共用）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod pick;
