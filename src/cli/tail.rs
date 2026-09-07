//! `tracker tail` — 实时跟随本地当日活动。

use std::time::Duration;

use lexopt::prelude::*;

use crate::local_time::{Calendar, SystemCalendar};
use crate::paths::AppPaths;
use crate::storage::crypto::{load_or_create_master_key, Cipher};
use crate::storage::dict::DictReader;
use crate::storage::query::{visit_local_date_range, LocalRecordSlice};
use crate::storage::writer::now_unix;

pub struct TailArgs {
    pub interval: u64,
    pub once: bool,
    pub history: usize,
}

const TAIL_HELP: &str = "\
tracker tail — 实时跟随本地当日活动

OPTIONS:
    --interval N    轮询间隔秒（默认 2）
    --once          仅显示一次后退出
    --history N     启动时打印最近 N 条历史（默认 10）
    -h, --help      打印帮助
";

pub fn parse(p: &mut lexopt::Parser) -> Result<TailArgs, lexopt::Error> {
    let mut args = TailArgs {
        interval: 2,
        once: false,
        history: 10,
    };
    while let Some(arg) = p.next()? {
        match arg {
            Short('h') | Long("help") => {
                print!("{TAIL_HELP}");
                std::process::exit(0);
            }
            Long("interval") => args.interval = p.value()?.parse()?,
            Long("once") => args.once = true,
            Long("history") => args.history = p.value()?.parse()?,
            _ => return Err(arg.unexpected()),
        }
    }
    Ok(args)
}

pub fn run(args: TailArgs, paths: &AppPaths, machine_scope: bool) -> std::io::Result<()> {
    let key = load_or_create_master_key(&paths.key_file, machine_scope)?;
    let cipher = Cipher::new(&key);
    let calendar = SystemCalendar::new();

    let mut first = true;
    let mut current_day = None;
    let mut previous = Vec::new();
    let mut previous_warning = None;
    let mut previous_error = None;

    loop {
        let today = calendar.today_at(now_unix())?;
        let day_identity = (today, calendar.day_start(today)?);
        if current_day != Some(day_identity) {
            current_day = Some(day_identity);
            first = true;
        }
        let refresh = match read_refresh(
            paths,
            &cipher,
            &calendar,
            today,
            &previous,
            first,
            args.history,
        ) {
            Ok(refresh) => refresh,
            Err(error) => {
                if args.once {
                    return Err(error);
                }
                if let Some(notice) = refresh_error_notice(&mut previous_error, &error) {
                    eprintln!("{notice}");
                }
                std::thread::sleep(Duration::from_secs(args.interval.max(1)));
                continue;
            }
        };
        if previous_error.take().is_some() {
            eprintln!("[Read recovered; refreshing records.]");
        }
        if refresh.warning != previous_warning {
            if let Some(warning) = &refresh.warning {
                eprintln!("{warning}");
            }
            previous_warning = refresh.warning;
        }
        if refresh.corrected {
            println!("[Capture correction: earlier output changed; showing recent records again.]");
        }
        for line in refresh.lines {
            println!("{line}");
        }
        first = false;
        previous = refresh.records;

        if args.once {
            break;
        }
        std::thread::sleep(Duration::from_secs(args.interval.max(1)));
    }
    Ok(())
}

struct Refresh {
    records: Vec<LocalRecordSlice>,
    lines: Vec<String>,
    warning: Option<String>,
    corrected: bool,
}

fn read_refresh(
    paths: &AppPaths,
    cipher: &Cipher,
    calendar: &impl Calendar,
    today: chrono::NaiveDate,
    previous: &[LocalRecordSlice],
    first: bool,
    history: usize,
) -> std::io::Result<Refresh> {
    let mut records = Vec::new();
    let quality = visit_local_date_range(paths, cipher, calendar, today, today, |record| {
        records.push(record);
        Ok(())
    })?;
    let mut apps = DictReader::open(&paths.apps_dict)?;
    let mut titles = DictReader::open(&paths.titles_dict)?;
    let corrected =
        !first && (previous.len() > records.len() || records[..previous.len()] != *previous);
    let start = if first || corrected {
        records.len().saturating_sub(history)
    } else {
        previous.len()
    };
    let mut lines = Vec::new();
    // Validate and format the whole refresh before printing or advancing the
    // cursor. A failed dictionary lookup must not leave half a refresh visible.
    for r in records.iter().skip(start) {
        let local_time = calendar.format_time(r.start_unix)?;
        let exe = if r.is_gap() {
            "[capture gap — excluded from usage]"
        } else {
            apps.resolve(r.app_id)?
        };
        let title = if r.is_gap() || r.title_id == 0 {
            ""
        } else {
            titles.resolve(r.title_id)?
        };
        let exe_short = std::path::Path::new(exe)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| exe.to_string());
        let title_part = if title.is_empty() {
            String::new()
        } else {
            format!(" │ {title}")
        };
        lines.push(format!(
            "[{local_time} for {:>4}s] {}{}",
            r.duration_secs, exe_short, title_part
        ));
    }
    Ok(Refresh {
        records,
        lines,
        warning: quality.warning(),
        corrected,
    })
}

fn refresh_error_notice(previous: &mut Option<String>, error: &std::io::Error) -> Option<String> {
    let message = error.to_string();
    if previous.as_ref() == Some(&message) {
        return None;
    }
    let notice = format!("[Refresh failed: {message}. Retrying; previous output is unchanged.]");
    *previous = Some(message);
    Some(notice)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_time::TestCalendar;
    use crate::storage::crypto::MasterKey;
    use crate::storage::dict::Dict;
    use crate::storage::log::{LogDate, LogWriter};
    use crate::storage::model::{Record, RECORD_FLAG_GAP};

    #[test]
    fn incomplete_first_append_can_recover_and_resume_without_reprinting_old_records() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_root(temp.path());
        let cipher = Cipher::new(&MasterKey::new_random());
        let calendar = TestCalendar(chrono_tz::UTC);
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 5).unwrap();
        let date = LogDate {
            year: 2026,
            month: 9,
            day: 5,
        };
        let path = paths.log_file_for_day(2026, 9, 5);
        let app_id = Dict::open_writer(&paths.apps_dict)
            .unwrap()
            .intern("editor.exe")
            .unwrap();
        let record = Record {
            start_offset_secs: 10,
            duration_secs: 10,
            app_id,
            title_id: 0,
            flags: 0,
        };
        LogWriter::open(&path, cipher.clone(), date)
            .unwrap()
            .write_block(&[record])
            .unwrap();
        let complete = std::fs::read(&path).unwrap();
        // A deterministic snapshot of a first append still missing its tag.
        std::fs::write(&path, &complete[..complete.len() - 5]).unwrap();
        let error = read_refresh(&paths, &cipher, &calendar, today, &[], true, 10)
            .err()
            .unwrap();
        let mut previous_error = None;
        assert!(refresh_error_notice(&mut previous_error, &error)
            .unwrap()
            .contains("Refresh failed"));
        assert!(refresh_error_notice(&mut previous_error, &error).is_none());
        std::fs::write(&path, complete).unwrap();
        let recovered = read_refresh(&paths, &cipher, &calendar, today, &[], true, 10).unwrap();
        assert_eq!(recovered.lines.len(), 1);
        assert!(previous_error.take().is_some());
        assert!(refresh_error_notice(&mut previous_error, &error).is_some());
        let same = read_refresh(
            &paths,
            &cipher,
            &calendar,
            today,
            &recovered.records,
            false,
            10,
        )
        .unwrap();
        assert!(same.lines.is_empty());
        assert!(!same.corrected);

        // A late correction after recovery still causes an explicit redraw.
        LogWriter::open(&path, cipher.clone(), date)
            .unwrap()
            .write_block(&[Record {
                start_offset_secs: 12,
                duration_secs: 2,
                app_id: 0,
                title_id: 0,
                flags: RECORD_FLAG_GAP,
            }])
            .unwrap();
        let corrected = read_refresh(
            &paths,
            &cipher,
            &calendar,
            today,
            &recovered.records,
            false,
            10,
        )
        .unwrap();
        assert!(corrected.corrected);
        assert_eq!(corrected.lines.len(), 3);
        assert!(corrected
            .lines
            .iter()
            .any(|line| line.contains("capture gap")));
        let repeated = read_refresh(
            &paths,
            &cipher,
            &calendar,
            today,
            &corrected.records,
            false,
            10,
        )
        .unwrap();
        assert!(!repeated.corrected);
        assert!(repeated.lines.is_empty());
    }

    #[test]
    fn changed_refresh_error_is_reported_once() {
        let mut previous = None;
        for error in ["first block incomplete", "dictionary id missing"] {
            let error = std::io::Error::other(error);
            assert!(refresh_error_notice(&mut previous, &error).is_some());
            assert!(refresh_error_notice(&mut previous, &error).is_none());
        }
    }
}
