//! Library facade so unit tests can reach internal modules.
//! Binary entry-point lives in `main.rs`.

#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod paths;
pub mod config;
pub mod classifier;
pub mod storage;

#[cfg(windows)]
pub mod platform;

#[cfg(windows)]
pub mod daemon;

pub mod cli;

/// Service / scheduled-task name used by both installer paths.
pub const APP_NAME: &str = "RustTimeNoter";
pub const SERVICE_NAME: &str = "RustTimeNoter";
pub const RUN_REG_VALUE: &str = "RustTimeNoter";
pub const MUTEX_NAME: &str = "Global\\RustTimeNoter.Daemon";
