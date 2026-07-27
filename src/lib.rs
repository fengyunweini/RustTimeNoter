//! Library facade so unit tests can reach internal modules.
//! Binary entry-point lives in `main.rs`.

#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod paths;
pub mod config;
pub mod classifier;
pub mod local_time;
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

/// Return the daemon's named mutex. Integration tests provide both private
/// test environment variables to receive an isolated instance suffix.
pub fn daemon_mutex_name() -> String {
    isolated_instance_name(MUTEX_NAME)
}

/// Return the daemon's named stop event. See [`daemon_mutex_name`].
pub fn stop_event_name() -> String {
    isolated_instance_name(STOP_EVENT_NAME)
}

fn isolated_instance_name(base: &str) -> String {
    if std::env::var_os("RUSTTIMENOTER_TEST_ROOT").is_some() {
        if let Some(instance) = std::env::var_os("RUSTTIMENOTER_TEST_INSTANCE") {
            let instance = instance.to_string_lossy();
            let safe: String = instance
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                        c
                    } else {
                        '_'
                    }
                })
                .take(64)
                .collect();
            if !safe.is_empty() {
                return format!("{base}.{safe}");
            }
        }
    }
    base.to_owned()
}
