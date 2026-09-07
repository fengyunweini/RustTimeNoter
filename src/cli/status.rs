//! `tracker status` — 查询 daemon 是否在跑、本地当日记录数、数据目录大小。

use std::path::Path;

use crate::local_time::{Calendar, SystemCalendar};
use crate::paths::AppPaths;
use crate::storage::crypto::{load_or_create_master_key, Cipher};
use crate::storage::query::visit_local_date_range;
use crate::storage::writer::now_unix;

pub fn run(paths: &AppPaths, machine_scope: bool) -> std::io::Result<()> {
    match is_daemon_running() {
        Ok(true) => println!("Daemon:        RUNNING"),
        Ok(false) => println!("Daemon:        stopped"),
        Err(error) => println!("Daemon:        [status unknown: {error}]"),
    }
    println!(
        "Scope:         {}",
        if machine_scope { "machine" } else { "user" }
    );
    println!("Data root:     {}", paths.root.display());
    println!("Config:        {}", paths.config_file.display());

    let calendar = SystemCalendar::new();
    let today = calendar.today_at(now_unix())?;
    let data_stats = dir_stats(&paths.data_dir);
    let mut record_count = 0usize;
    let mut total_secs = 0u64;
    let read_error = (|| -> std::io::Result<()> {
        if paths.key_file.try_exists()? {
            let key = load_or_create_master_key(&paths.key_file, machine_scope)?;
            let cipher = Cipher::new(&key);
            let quality =
                visit_local_date_range(paths, &cipher, &calendar, today, today, |record| {
                    if !record.is_gap() {
                        record_count += 1;
                        total_secs += record.duration_secs as u64;
                    }
                    Ok(())
                })?;
            if let Some(warning) = quality.warning() {
                println!("{warning}");
            }
        } else {
            let (_, has_data) = data_stats
                .as_ref()
                .map_err(|error| std::io::Error::new(error.kind(), error.to_string()))?;
            if *has_data {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("missing encryption key: {}", paths.key_file.display()),
                ));
            }
        }
        Ok(())
    })()
    .err();

    println!("Today:         {today}");
    if let Some(e) = read_error {
        println!("Today records: [read error: {e}]");
    } else {
        println!(
            "Today records: {record_count}    total: {}",
            fmt_dur(total_secs)
        );
    }
    match data_stats {
        Ok((total_size, _)) => println!(
            "Data dir size: {} bytes ({:.2} MB)",
            total_size,
            total_size as f64 / 1024.0 / 1024.0
        ),
        Err(error) => println!("Data dir size: [unavailable: {error}]"),
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn is_daemon_running() -> std::io::Result<bool> {
    let name: Vec<u16> = crate::daemon_mutex_name()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    probe_daemon_mutex(&name)
}

#[cfg(windows)]
fn probe_daemon_mutex(name: &[u16]) -> std::io::Result<bool> {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
    use windows_sys::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
    unsafe {
        // Merely querying status must never create the daemon's singleton
        // object and make a concurrent startup believe another daemon exists.
        let h = OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr());
        if h.is_null() {
            return mutex_probe_error(GetLastError());
        }
        CloseHandle(h);
        Ok(true)
    }
}

#[cfg(windows)]
fn mutex_probe_error(error: u32) -> std::io::Result<bool> {
    if error == windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND {
        Ok(false)
    } else {
        Err(std::io::Error::from_raw_os_error(error as i32))
    }
}

#[cfg(not(windows))]
pub(crate) fn is_daemon_running() -> std::io::Result<bool> {
    Ok(false)
}

fn dir_stats(p: &Path) -> std::io::Result<(u64, bool)> {
    let mut total = 0u64;
    let mut has_data = false;
    let rd = match std::fs::read_dir(p) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((0, false)),
        Err(error) => return Err(error),
    };
    for entry in rd {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            let (nested_total, nested_has_data) = dir_stats(&entry.path())?;
            total += nested_total;
            has_data |= nested_has_data;
        } else {
            let m = entry.metadata()?;
            total += m.len();
            has_data = true;
        }
    }
    Ok((total, has_data))
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
    #[cfg(windows)]
    fn denied_mutex_probe_is_unknown_not_stopped() {
        use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND};
        assert!(!super::mutex_probe_error(ERROR_FILE_NOT_FOUND).unwrap());
        assert_eq!(
            super::mutex_probe_error(ERROR_ACCESS_DENIED)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn dir_stats_distinguishes_empty_data_from_zero_length_files() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(dir_stats(temp.path()).unwrap(), (0, false));

        let nested = temp.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::File::create(nested.join("empty.log")).unwrap();
        assert_eq!(dir_stats(temp.path()).unwrap(), (0, true));

        std::fs::write(nested.join("records.log"), b"abc").unwrap();
        assert_eq!(dir_stats(temp.path()).unwrap(), (3, true));
    }

    #[test]
    fn invalid_data_directory_is_not_reported_as_empty() {
        let temp = tempfile::tempdir().unwrap();
        let absent = temp.path().join("absent");
        assert_eq!(dir_stats(&absent).unwrap(), (0, false));
        let file = temp.path().join("data");
        std::fs::write(&file, b"not a directory").unwrap();
        assert!(dir_stats(&file).is_err());
    }
}
