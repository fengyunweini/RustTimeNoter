//! Daemon: 事件钩子 + 状态聚合 + 派发到 writer。
//!
//! 入口：[`run`]，由 `main.rs` 在无参分发或 service 启动时调用。

pub mod aggregator;
pub mod event_queue;
pub mod hook;
pub mod resolver;
pub mod runtime;
pub mod tray;

pub use runtime::run;
