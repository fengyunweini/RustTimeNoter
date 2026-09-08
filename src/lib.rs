//! Library facade so unit tests can reach internal modules.
//! Binary entry-point lives in `main.rs`.

#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod classifier;
pub mod config;
pub mod local_time;
pub mod paths;
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
pub const STOP_EVENT_NAME: &str = "Global\\RustTimeNoter.Stop";

pub fn daemon_mutex_name() -> String {
    isolated_name(MUTEX_NAME)
}
pub fn stop_event_name() -> String {
    isolated_name(STOP_EVENT_NAME)
}

fn isolated_name(base: &str) -> String {
    if std::env::var_os("RUSTTIMENOTER_TEST_ROOT").is_some() {
        if let Ok(instance) = std::env::var("RUSTTIMENOTER_TEST_INSTANCE") {
            if valid_test_instance(&instance) {
                return format!("{base}.{instance}");
            }
        }
    }
    base.to_owned()
}

pub(crate) fn valid_test_instance(instance: &str) -> bool {
    !instance.is_empty()
        && instance.len() <= 64
        && instance
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}
