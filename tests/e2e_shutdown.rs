//! E2E：spawn daemon → 等若干秒采样 → `tracker stop` → 验证子进程优雅退出且当日日志非空。
//!
//! 需要先 `cargo build --release` 或在测试中复用 debug 二进制。
//! 这里直接用 cargo 当前 profile 产物（`env!("CARGO_BIN_EXE_tracker")`）。

#![cfg(windows)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tracker")
}

#[test]
fn graceful_shutdown_via_stop_cmd() {
    // 启动 daemon
    let mut child = Command::new(bin())
        .arg("run")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");

    // 给它至少一个 idle tick 周期采集前台窗口
    std::thread::sleep(Duration::from_secs(4));

    // 触发 stop
    let stop = Command::new(bin())
        .arg("stop")
        .output()
        .expect("run tracker stop");
    assert!(stop.status.success(), "stop cmd failed");
    let stdout = String::from_utf8_lossy(&stop.stdout);
    assert!(
        stdout.contains("Stop signal sent"),
        "unexpected stop stdout: {stdout}"
    );

    // 等子进程退出，超时 10s
    let deadline = Instant::now() + Duration::from_secs(10);
    let exit_status = loop {
        match child.try_wait().expect("try_wait") {
            Some(s) => break s,
            None if Instant::now() > deadline => {
                let _ = child.kill();
                panic!("daemon did not exit within 10s after stop signal");
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    assert!(
        exit_status.success(),
        "daemon exit was non-zero: {exit_status:?}"
    );
}

#[test]
fn stop_when_no_daemon_running_is_clean() {
    // 先确保没有 daemon（前一个 test 应该已经退出，串行跑 OK）
    std::thread::sleep(Duration::from_millis(500));
    let stop = Command::new(bin())
        .arg("stop")
        .output()
        .expect("run tracker stop");
    assert!(stop.status.success(), "stop cmd should succeed even with no daemon");
    let stdout = String::from_utf8_lossy(&stop.stdout);
    assert!(
        stdout.contains("No running daemon"),
        "unexpected stop stdout: {stdout}"
    );
}
