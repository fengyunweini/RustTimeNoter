//! End-to-end shutdown tests.
//!
//! Every test gets its own data root and Windows named objects. This keeps the
//! default parallel test runner deterministic and, more importantly, prevents
//! a test from stopping or writing into a real RustTimeNoter daemon.

#![cfg(windows)]

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

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
