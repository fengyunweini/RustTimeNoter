//! 持久化与加密日志层。
//!
//! - `dict`：进程路径 / 窗口标题 → u32 ID 的字符串池。
//! - `crypto`：AES-256-GCM block 加密 + DPAPI 主密钥包裹（仅 Windows）。
//! - `log`：每日 `.log` 文件读写。
//! - `writer`：聚合段 (`Segment`) → 字典 → block buffer → 落盘。
//! - `model`：跨模块共享数据结构。

pub mod model;
pub mod dict;
pub mod crypto;
pub mod log;
pub mod writer;

pub use model::{Segment, Record};
