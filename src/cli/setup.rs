//! `tracker setup` — 一键安装：
//!   1) 复制自身到 %LOCALAPPDATA%\RustTimeNoter\bin\tracker.exe
//!   2) 写 HKCU autostart
//!   3) 后台拉起 daemon（如果没在跑）
//!   4) 生成并打开 HTML 报表
//!
//! 没有任何参数；幂等。

use std::path::PathBuf;

use crate::cli::install::{install, InstallArgs, Mode};
use crate::cli::view::{self, ViewArgs};
use crate::paths::{AppPaths, InstallScope};

pub fn run() -> std::io::Result<()> {
    println!("=== RustTimeNoter setup ===");

    let paths = AppPaths::for_scope(InstallScope::User)?;
    paths.ensure_dirs()?;

    // 1+2: install autostart (该函数会复制自身到 bin_dir 并写注册表)
    println!("[1/3] installing autostart ...");
    install(InstallArgs {
        mode: Mode::Autostart,
    })?;

    // 3: launch the daemon now (if not already running) — detached, no console.
    // The no-argument path is background mode and persists initialization
    // failures to crash.log.
    let bin = paths.bin_dir.join("tracker.exe");
    if !is_daemon_running() {
        println!("[2/3] starting daemon ...");
        spawn_detached(&bin)?;
        // 给 daemon 200ms 时间初始化，免得 view 还没目录可读
        std::thread::sleep(std::time::Duration::from_millis(200));
    } else {
        println!("[2/3] daemon already running, skip launch");
    }

    // 4: open HTML viewer
    println!("[3/3] opening report ...");
    view::run(
        ViewArgs {
            days: 7,
            no_open: false,
            out: None,
        },
        &paths,
        false,
    )?;

    println!();
    println!("Done. Tracker will auto-start on next logon.");
    println!("  data : {}", paths.root.display());
    println!("  view : tracker view");
    println!("  stop : tracker stop");
    println!("  off  : tracker uninstall autostart");
    Ok(())
}

fn is_daemon_running() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let name: Vec<u16> = crate::daemon_mutex_name()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let h = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if h.is_null() {
            return false;
        }
        let err = GetLastError();
        CloseHandle(h);
        err == ERROR_ALREADY_EXISTS
    }
}

fn spawn_detached(exe: &PathBuf) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    // DETACHED_PROCESS (0x00000008) | CREATE_NEW_PROCESS_GROUP (0x00000200)
    const FLAGS: u32 = 0x00000008 | 0x00000200;
    std::process::Command::new(exe)
        .creation_flags(FLAGS)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}
