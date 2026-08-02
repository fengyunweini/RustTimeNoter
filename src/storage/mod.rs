//! 持久化与加密日志层。
//!
//! - `dict`：进程路径 / 窗口标题 → u32 ID 的字符串池。
//! - `crypto`：AES-256-GCM block 加密 + DPAPI 主密钥包裹（仅 Windows）。
//! - `log`：每日 `.log` 文件读写。
//! - `writer`：聚合段 (`Segment`) → 字典 → block buffer → 落盘。
//! - `query`：按本地日历范围读取 UTC 分片。
//! - `model`：跨模块共享数据结构。

pub mod crypto;
pub mod dict;
pub mod log;
pub mod model;
pub mod query;
pub mod writer;

pub use model::{Record, Segment};
