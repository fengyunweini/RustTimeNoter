//! `tracker status` — 查询 daemon 是否在跑、当日记录数、数据目录大小。

use std::path::Path;

use crate::paths::AppPaths;
use crate::storage::crypto::{load_or_create_master_key, Cipher};
use crate::storage::log::LogReader;
use crate::storage::writer::{now_unix, unix_to_utc_date};

pub fn run(paths: &AppPaths, machine_scope: bool) -> std::io::Result<()> {
    let running = is_daemon_running();
    println!("Daemon:        {}", if running { "RUNNING" } else { "stopped" });
    println!("Scope:         {}", if machine_scope { "machine" } else { "user" });
    println!("Data root:     {}", paths.root.display());
    println!("Config:        {}", paths.config_file.display());

    // 当日统计
    let today = unix_to_utc_date(now_unix());
    let log_path = paths.log_file_for_day(today.year, today.month, today.day);
    let log_size = file_size(&log_path);
    let total_size = dir_size(&paths.data_dir);

    print!("Today log:     {} ({} bytes)", log_path.display(), log_size);
    if log_size > 0 {
        let key = load_or_create_master_key(&paths.key_file, machine_scope)?;
        let cipher = Cipher::new(&key);
        match LogReader::new(cipher, today).read_all(&log_path) {
            Ok(recs) => {
                let total_secs: u64 = recs.iter().map(|r| r.duration_secs as u64).sum();
                println!();
                println!("Today records: {}    total: {}", recs.len(), fmt_dur(total_secs));
            }
            Err(e) => println!("    [read error: {e}]"),
        }
    } else {
        println!();
        println!("Today records: 0");
    }
    println!("Data dir size: {} bytes ({:.2} MB)", total_size, total_size as f64 / 1024.0 / 1024.0);
    Ok(())
}

#[cfg(windows)]
fn is_daemon_running() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let name: Vec<u16> = crate::MUTEX_NAME.encode_utf16().chain(std::iter::once(0)).collect();
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

#[cfg(not(windows))]
fn is_daemon_running() -> bool {
    false
}

fn file_size(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

fn dir_size(p: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(rd) = std::fs::read_dir(p) else { return 0 };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            total += dir_size(&entry.path());
        } else if let Ok(m) = entry.metadata() {
            total += m.len();
        }
    }
    total
}

fn fmt_dur(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 { format!("{h}h {m:02}m {s:02}s") }
    else if m > 0 { format!("{m}m {s:02}s") }
    else { format!("{s}s") }
}
