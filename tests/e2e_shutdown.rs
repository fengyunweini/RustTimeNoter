//! End-to-end shutdown tests.
//!
//! Every test gets its own data root and Windows named objects. This keeps the
//! default parallel test runner deterministic and, more importantly, prevents
//! a test from stopping or writing into a real RustTimeNoter daemon.

#![cfg(windows)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tracker::paths::{AppPaths, InstallScope};
use tracker::storage::crypto::{load_or_create_master_key, Cipher};
use tracker::storage::log::{LogDate, LogReader};
use tracker::storage::writer::{self, utc_midnight_unix, WriterConfig, WriterMsg};
use tracker::storage::Segment;

const TEST_ROOT_ENV: &str = "RUSTTIMENOTER_TEST_ROOT";
const TEST_INSTANCE_ENV: &str = "RUSTTIMENOTER_TEST_INSTANCE";
static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(0);

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tracker")
}

fn instance(label: &str) -> String {
    format!(
        "e2e-{label}-{}-{}",
        std::process::id(),
        NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn tracker_command(root: &std::path::Path, instance: &str) -> Command {
    let mut command = Command::new(bin());
    command
        .env(TEST_ROOT_ENV, root)
        .env(TEST_INSTANCE_ENV, instance);
    command
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[test]
fn graceful_shutdown_via_stop_cmd() {
    let root = tempfile::tempdir().expect("create isolated data root");
    let instance = instance("graceful");
    let child = tracker_command(root.path(), &instance)
        .arg("run")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn isolated daemon");
    let mut child = ChildGuard(child);

    // Allow initialization and at least one idle-timer cycle. The unique name
    // and root make this safe even when the sibling test runs concurrently.
    std::thread::sleep(Duration::from_secs(4));
    assert!(
        child.0.try_wait().expect("query daemon").is_none(),
        "daemon exited before the stop request"
    );

    let stop = tracker_command(root.path(), &instance)
        .arg("stop")
        .output()
        .expect("run tracker stop");
    assert!(stop.status.success(), "stop command failed");
    let stdout = String::from_utf8_lossy(&stop.stdout);
    assert!(
        stdout.contains("Stop signal sent"),
        "unexpected stop stdout: {stdout}"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    let exit_status = loop {
        match child.0.try_wait().expect("wait for daemon") {
            Some(status) => break status,
            None if Instant::now() > deadline => {
                panic!("daemon did not exit within 10 seconds after stop signal")
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    assert!(
        exit_status.success(),
        "daemon exit was non-zero: {exit_status:?}"
    );
    assert!(
        root.path().join("key.bin").is_file(),
        "daemon did not initialize its isolated data root"
    );
    assert!(root.path().join("apps.dict").is_file());
    assert!(root.path().join("titles.dict").is_file());
}

#[test]
fn stop_when_no_daemon_running_is_clean() {
    let root = tempfile::tempdir().expect("create isolated data root");
    let instance = instance("absent");
    let stop = tracker_command(root.path(), &instance)
        .arg("stop")
        .output()
        .expect("run tracker stop");
    assert!(stop.status.success(), "stop should succeed with no daemon");
    let stdout = String::from_utf8_lossy(&stop.stdout);
    assert!(
        stdout.contains("No running daemon"),
        "unexpected stop stdout: {stdout}"
    );
}

#[test]
fn detached_startup_failure_leaves_a_readable_error() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("config.toml"), "not valid TOML = [").unwrap();
    let output = tracker_command(root.path(), &instance("invalid-config"))
        .output()
        .expect("run background entrypoint");
    assert!(!output.status.success());
    let error = std::fs::read_to_string(root.path().join("crash.log")).unwrap();
    assert!(error.contains("daemon stopped with error"));
}

fn valid_history(root: &Path) -> AppPaths {
    let paths = AppPaths::from_root(root);
    let date = LogDate {
        year: 2026,
        month: 9,
        day: 1,
    };
    let start = utc_midnight_unix(date) + 10;
    let (sender, receiver) = mpsc::channel();
    sender
        .send(WriterMsg::Segment(Segment {
            app_path: "C:/startup-fixture.exe".into(),
            app_basename: "startup-fixture.exe".into(),
            title: Some("valid recorded history".into()),
            start_unix: start,
            end_unix: start + 10,
        }))
        .unwrap();
    let (ack_sender, ack_receiver) = mpsc::channel();
    sender.send(WriterMsg::FlushAndAck(ack_sender)).unwrap();
    sender.send(WriterMsg::Shutdown).unwrap();
    writer::run(
        WriterConfig {
            paths: paths.clone(),
            scope: InstallScope::User,
            flush_block_records: 1,
            flush_interval_secs: 60,
        },
        receiver,
    )
    .unwrap();
    assert_eq!(ack_receiver.recv().unwrap(), Ok(()));
    let key = load_or_create_master_key(&paths.key_file, false).unwrap();
    let records = LogReader::new(Cipher::new(&key), date)
        .read_all(&paths.log_file_for_day(date.year, date.month, date.day))
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].duration_secs, 10);
    assert_ne!(records[0].app_id, 0);
    assert_ne!(records[0].title_id, 0);
    paths
}

fn data_snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                pending.push(path);
            } else {
                let relative = path.strip_prefix(root).unwrap().to_path_buf();
                // The background entrypoint intentionally appends its error.
                if relative != Path::new("crash.log") {
                    files.insert(relative, std::fs::read(path).unwrap());
                }
            }
        }
    }
    files
}

fn startup_output(root: &Path, label: &str, background: bool) -> Output {
    use std::os::windows::process::CommandExt;

    // Files avoid a full stdout/stderr pipe blocking the child under review.
    let capture = tempfile::tempdir().unwrap();
    let stdout = capture.path().join("stdout");
    let stderr = capture.path().join("stderr");
    let mut command = tracker_command(root, &instance(label));
    if !background {
        command.arg("run");
    }
    let child = command
        .env("RUSTTIMENOTER_SCOPE", "user")
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&stdout).unwrap())
        .stderr(std::fs::File::create(&stderr).unwrap())
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .spawn()
        .expect("start isolated daemon with invalid storage");
    let mut child = ChildGuard(child);
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("query startup failure") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "invalid storage did not fail startup within 20 seconds"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    Output {
        status,
        stdout: std::fs::read(stdout).unwrap(),
        stderr: std::fs::read(stderr).unwrap(),
    }
}

fn assert_storage_startup_failure(label: &str, damage: impl Fn(&AppPaths), expected_error: &str) {
    for background in [false, true] {
        let root = tempfile::tempdir().unwrap();
        // No successful daemon or foreground observations are needed: the
        // public writer produces and verifies the original history directly.
        let paths = valid_history(root.path());
        damage(&paths);
        let before = data_snapshot(root.path());
        let output = startup_output(root.path(), label, background);
        assert_eq!(
            output.status.code(),
            Some(1),
            "background={background}, stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let error = if background {
            let error = std::fs::read_to_string(&paths.crash_log).unwrap();
            assert!(error.contains("daemon stopped with error:"));
            error
        } else {
            assert!(!paths.crash_log.exists());
            String::from_utf8_lossy(&output.stderr).into_owned()
        };
        assert!(
            error.contains(expected_error),
            "storage error was hidden, background={background}: {error}"
        );
        assert_eq!(
            data_snapshot(root.path()),
            before,
            "failed startup changed storage, background={background}"
        );
    }
}

#[test]
fn missing_dictionary_startup_preserves_history_and_reports_original_error() {
    assert_storage_startup_failure(
        "missing-apps",
        |paths| std::fs::remove_file(&paths.apps_dict).unwrap(),
        "recorded logs exist but a dictionary is missing; refusing to reuse its IDs",
    );
}

#[test]
fn damaged_key_startup_preserves_history_and_reports_original_error() {
    assert_storage_startup_failure(
        "damaged-key",
        |paths| std::fs::write(&paths.key_file, b"damaged DPAPI key").unwrap(),
        // The numeric Win32 status can vary; the failing operation must remain
        // visible instead of being replaced with a generic channel error.
        "CryptUnprotectData failed:",
    );
}

#[test]
fn malformed_test_instance_fails_before_command_dispatch() {
    let root = tempfile::tempdir().unwrap();
    // Use a read-only command: even a regression must not signal a real daemon.
    let output = tracker_command(root.path(), "invalid/instance")
        .args(["config", "show"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("RUSTTIMENOTER_TEST_INSTANCE"));
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}
