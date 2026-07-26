//! `tracker status` — 查询 daemon 是否在跑、本地当日记录数、数据目录大小。

use std::path::Path;

use crate::local_time::{Calendar, SystemCalendar};
use crate::paths::AppPaths;
use crate::storage::crypto::{load_or_create_master_key, Cipher};
use crate::storage::query::visit_local_date_range;
use crate::storage::writer::now_unix;

pub fn run(paths: &AppPaths, machine_scope: bool) -> std::io::Result<()> {
    let running = is_daemon_running();
    println!(
        "Daemon:        {}",
        if running { "RUNNING" } else { "stopped" }
    );
    println!(
        "Scope:         {}",
        if machine_scope { "machine" } else { "user" }
    );
    println!("Data root:     {}", paths.root.display());
    println!("Config:        {}", paths.config_file.display());

    let calendar = SystemCalendar::new();
    let today = calendar.today_at(now_unix())?;
    let (total_size, has_data) = dir_stats(&paths.data_dir);
    let mut record_count = 0usize;
    let mut total_secs = 0u64;
    let read_error = if paths.key_file.exists() {
        let key = load_or_create_master_key(&paths.key_file, machine_scope)?;
        let cipher = Cipher::new(&key);
        visit_local_date_range(paths, &cipher, &calendar, today, today, |record| {
            record_count += 1;
            total_secs += record.duration_secs as u64;
            Ok(())
        })
        .err()
    } else if has_data {
        Some(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("missing encryption key: {}", paths.key_file.display()),
        ))
    } else {
        None
    };

    println!("Today:         {today}");
    if let Some(e) = read_error {
        println!("Today records: [read error: {e}]");
    } else {
        println!(
            "Today records: {record_count}    total: {}",
            fmt_dur(total_secs)
        );
    }
    println!(
        "Data dir size: {} bytes ({:.2} MB)",
        total_size,
        total_size as f64 / 1024.0 / 1024.0
    );
    Ok(())
}

#[cfg(windows)]
fn is_daemon_running() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let name: Vec<u16> = crate::MUTEX_NAME
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

#[cfg(not(windows))]
fn is_daemon_running() -> bool {
    false
}

fn dir_stats(p: &Path) -> (u64, bool) {
    let mut total = 0u64;
    let mut has_data = false;
    let Ok(rd) = std::fs::read_dir(p) else {
        return (0, false);
    };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            let (nested_total, nested_has_data) = dir_stats(&entry.path());
            total += nested_total;
            has_data |= nested_has_data;
        } else if let Ok(m) = entry.metadata() {
            total += m.len();
            has_data = true;
        }
    }
    (total, has_data)
}

fn fmt_dur(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h {m:02}m {s:02}s")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::dir_stats;

    #[test]
    fn dir_stats_distinguishes_empty_data_from_zero_length_files() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(dir_stats(temp.path()), (0, false));

        let nested = temp.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::File::create(nested.join("empty.log")).unwrap();
        assert_eq!(dir_stats(temp.path()), (0, true));

        std::fs::write(nested.join("records.log"), b"abc").unwrap();
        assert_eq!(dir_stats(temp.path()), (3, true));
    }
}
