//! Daemon: 事件钩子 + 状态聚合 + 派发到 writer。
//!
//! 入口：[`run`]，由 `main.rs` 在无参分发或 service 启动时调用。

#![cfg(windows)]

pub mod aggregator;
pub mod resolver;
pub mod hook;
pub mod runtime;

pub use runtime::run;
