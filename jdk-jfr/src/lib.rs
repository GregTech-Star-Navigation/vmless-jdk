//! `vmless-jfr` —— JFR（Java Flight Recorder）的 `.jfr` 读 / 写 / 录制 + `jhsdb` 等价能力。
//!
//! # 一句话
//!
//! 真 `jfr` 能读我们写的文件，我们也能读真 `jfr` 写的文件 —— 双向兼容由
//! `src.zip` 里的 `jdk.jfr.internal.*` 逐行对齐保证，不是靠猜格式。
//!
//! # 分层
//!
//! ```text
//! bin/jfr     命令行：summary / print / metadata
//! bin/jhsdb   等价于 jmap/jstack/jinfo 的那几个子命令（走 vmless-attach）
//! bin/jfrfuzz 解析入口的 fuzz 驱动（5 万例随机字节，0 panic）
//!
//! recorder  Recorder::start → 环形缓冲 → dump(path)
//! writer    Metadata + 事件 → 合法 chunk 字节
//! reader    chunk → 元数据 / 常量池 / 事件流
//! model     类型定义与值
//! format    二进制原语：varint、chunk 头、元数据树、常量池、事件块
//! ```
//!
//! # 最小例子
//!
//! ```no_run
//! use vmless_jfr::recorder::{Recorder, RecorderConfig, ThreadInfo};
//!
//! let mut r = Recorder::start(RecorderConfig::default());
//! r.thread_end(&ThreadInfo { java_name: "main".into(), java_id: 1, ..Default::default() })?;
//! r.dump("out.jfr")?;
//! # Ok::<(), vmless_jfr::Error>(())
//! ```
//!
//! 写出来的文件可以直接喂给真 `jfr summary` / `jfr print`。
//!
//! # 无依赖
//!
//! 除了 `vmless-attach`（jhsdb 要用），本 crate **一个外部依赖都没有**：
//! 变长整数、UTF-16 编解码、时区偏移、内存峰值统计都是自己写的几十行。

pub mod format;
pub mod model;
pub mod reader;
pub mod recorder;
pub mod writer;

pub use format::{Error, Result};
pub use model::{Metadata, TypeDef, Value};
pub use reader::{LoadedEvent, Recording, Summary};
pub use recorder::{Recorder, RecorderConfig};
pub use writer::ChunkSpec;

/// 版本号（与 `Cargo.toml` 一致）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
