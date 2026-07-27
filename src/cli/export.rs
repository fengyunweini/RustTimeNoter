//! 导出 CSV / JSON 原始记录。

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use lexopt::prelude::*;
use rand::RngCore;
use serde::Serialize;

use crate::classifier::Classifier;
use crate::cli::report::{fmt_date, parse_date};
use crate::local_time::{Calendar, SystemCalendar};
use crate::paths::AppPaths;
use crate::storage::crypto::{load_or_create_master_key, Cipher};
use crate::storage::dict::Dict;
use crate::storage::query::{validate_local_date_range, visit_local_date_range};
use crate::storage::writer::now_unix;

pub struct ExportArgs {
    pub format: Format,
    pub out: PathBuf,
    pub from: Option<String>,
    pub to: Option<String>,
}

#[derive(Clone, Copy)]
pub enum Format {
    Csv,
    Json,
}

#[derive(Serialize)]
struct Row<'a> {
    date: String,
    start_time: String,
    duration_secs: u32,
    app_path: &'a str,
    title: Option<&'a str>,
    category: Option<&'a str>,
    start_timestamp: String,
}

const CSV_HEADER: &str = "date,start_time,duration_secs,app_path,title,category,start_timestamp";
const TEMP_FILE_PREFIX: &str = ".rusttimenoter-export-";
const TEMP_FILE_ATTEMPTS: usize = 128;

const EXPORT_HELP: &str = "\
tracker export — 导出原始记录

OPTIONS:
    --format csv|json     输出格式（默认 csv）
    --out PATH            输出文件路径（必填）
    --from YYYY-MM-DD     起始本地日期（默认今天）
    --to   YYYY-MM-DD     截止本地日期（默认今天）
    -h, --help            打印帮助
";

pub fn parse(p: &mut lexopt::Parser) -> Result<ExportArgs, lexopt::Error> {
    let mut format = Format::Csv;
    let mut out: Option<PathBuf> = None;
    let mut from: Option<String> = None;
    let mut to: Option<String> = None;
    while let Some(arg) = p.next()? {
        match arg {
            Short('h') | Long("help") => {
                print!("{EXPORT_HELP}");
                std::process::exit(0);
            }
            Long("format") => {
                format = match p.value()?.string()?.as_str() {
                    "csv" => Format::Csv,
                    "json" => Format::Json,
                    other => {
                        return Err(lexopt::Error::UnexpectedValue {
                            option: "--format".into(),
                            value: other.into(),
                        })
                    }
                };
            }
            Long("out") => out = Some(p.value()?.into()),
            Long("from") => from = Some(p.value()?.string()?),
            Long("to") => to = Some(p.value()?.string()?),
            _ => return Err(arg.unexpected()),
        }
    }
    let out = out.ok_or(lexopt::Error::MissingValue {
        option: Some("--out".into()),
    })?;
    Ok(ExportArgs {
        format,
        out,
        from,
        to,
    })
}

pub fn run(args: ExportArgs, paths: &AppPaths, machine_scope: bool) -> std::io::Result<()> {
    let calendar = SystemCalendar::new();
    let today = calendar.today_at(now_unix())?;
    let from = args
        .from
        .as_deref()
        .map(parse_date)
        .transpose()?
        .unwrap_or(today);
    let to = args
        .to
        .as_deref()
        .map(parse_date)
        .transpose()?
        .unwrap_or(today);
    validate_local_date_range(from, to)?;

    let key = load_or_create_master_key(&paths.key_file, machine_scope)?;
    let cipher = Cipher::new(&key);
    let apps = Dict::open(&paths.apps_dict)?;
    let titles = Dict::open(&paths.titles_dict)?;
    let classifier = Classifier::load(&paths.rules_file)?;

    let format = args.format;
    write_atomically(&args.out, |out| {
        let mut json_first = true;
        if matches!(format, Format::Csv) {
            writeln!(out, "{CSV_HEADER}")?;
        } else {
            write!(out, "[")?;
        }

        visit_local_date_range(paths, &cipher, &calendar, from, to, |r| {
            let exe = apps.get(r.app_id).unwrap_or("?");
            let title = if r.title_id == 0 {
                None
            } else {
                titles.get(r.title_id)
            };
            let category = classifier.classify(exe, title);
            let (start_time, start_timestamp) = calendar.format_time_and_rfc3339(r.start_unix)?;
            match format {
                Format::Csv => {
                    writeln!(
                        out,
                        "{},{},{},{},{},{},{}",
                        fmt_date(r.local_date),
                        start_time,
                        r.duration_secs,
                        csv_escape(exe),
                        csv_escape(title.unwrap_or("")),
                        csv_escape(category.unwrap_or("")),
                        start_timestamp,
                    )?;
                }
                Format::Json => {
                    if !json_first {
                        write!(out, ",")?;
                    }
                    json_first = false;
                    let row = Row {
                        date: fmt_date(r.local_date),
                        start_time,
                        duration_secs: r.duration_secs,
                        app_path: exe,
                        title,
                        category,
                        start_timestamp,
                    };
                    serde_json::to_writer(&mut *out, &row)?;
                }
            }
            Ok(())
        })?;

        if matches!(format, Format::Json) {
            write!(out, "]")?;
        }
        Ok(())
    })?;

    println!("wrote {}", args.out.display());
    Ok(())
}

fn write_atomically<F>(destination: &Path, write_contents: F) -> io::Result<()>
where
    F: FnOnce(&mut dyn Write) -> io::Result<()>,
{
    let mut pending = PendingOutput::create(destination)?;
    let write_result = {
        let mut out = BufWriter::new(pending.file_mut());
        write_contents(&mut out).and_then(|_| out.flush())
    };
    if let Err(error) = write_result {
        return pending.abort(error);
    }
    if let Err(error) = pending.sync_all() {
        return pending.abort(error);
    }
    pending.commit(destination)
}

struct PendingOutput {
    path: PathBuf,
    file: Option<File>,
    committed: bool,
}

impl PendingOutput {
    fn create(destination: &Path) -> io::Result<Self> {
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;

        let mut random = rand::thread_rng();
        for _ in 0..TEMP_FILE_ATTEMPTS {
            let mut name = OsString::from(TEMP_FILE_PREFIX);
            name.push(format!("{:016x}.tmp", random.next_u64()));
            let path = parent.join(name);
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                        committed: false,
                    })
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }

        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create a unique temporary export file",
        ))
    }

    fn file_mut(&mut self) -> &mut File {
        self.file
            .as_mut()
            .expect("pending output file is available before commit")
    }

    fn sync_all(&self) -> io::Result<()> {
        self.file
            .as_ref()
            .expect("pending output file is available before commit")
            .sync_all()
    }

    fn commit(mut self, destination: &Path) -> io::Result<()> {
        self.file.take();
        match std::fs::rename(&self.path, destination) {
            Ok(()) => {
                self.committed = true;
                Ok(())
            }
            Err(error) => self.abort(error),
        }
    }

    fn abort<T>(mut self, source: io::Error) -> io::Result<T> {
        self.file.take();
        match std::fs::remove_file(&self.path) {
            Ok(()) => {
                self.committed = true;
                Err(source)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.committed = true;
                Err(source)
            }
            Err(cleanup) => Err(io::Error::new(
                source.kind(),
                format!(
                    "{source}; additionally failed to remove temporary export {}: {cleanup}",
                    self.path.display()
                ),
            )),
        }
    }
}

impl Drop for PendingOutput {
    fn drop(&mut self) {
        self.file.take();
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Days, NaiveDate};

    use crate::local_time::{Calendar, SystemCalendar, TestCalendar};
    use crate::storage::log::LogWriter;
    use crate::storage::model::Record;
    use crate::storage::query::MAX_QUERY_DAYS;
    use crate::storage::writer::{unix_to_utc_date, utc_midnight_unix};

    use super::*;

    fn args(format: Format, out: PathBuf, from: NaiveDate, to: NaiveDate) -> ExportArgs {
        ExportArgs {
            format,
            out,
            from: Some(from.to_string()),
            to: Some(to.to_string()),
        }
    }

    fn assert_no_pending_exports(parent: &Path) {
        let entries = std::fs::read_dir(parent).unwrap();
        for entry in entries {
            let name = entry.unwrap().file_name();
            assert!(
                !name.to_string_lossy().starts_with(TEMP_FILE_PREFIX),
                "temporary export was not cleaned up: {}",
                name.to_string_lossy()
            );
        }
    }

    #[test]
    fn csv_header_preserves_legacy_columns_and_appends_timestamp() {
        let columns: Vec<_> = CSV_HEADER.split(',').collect();

        assert_eq!(
            &columns[..6],
            [
                "date",
                "start_time",
                "duration_secs",
                "app_path",
                "title",
                "category",
            ]
        );
        assert_eq!(columns.len(), 7);
        assert_eq!(columns.last(), Some(&"start_timestamp"));
    }

    #[test]
    fn json_preserves_field_order_and_numeric_utc_offset() {
        let calendar = TestCalendar(chrono_tz::Asia::Shanghai);
        let (start_time, start_timestamp) = calendar.format_time_and_rfc3339(0).unwrap();
        let row = Row {
            date: "1970-01-01".to_string(),
            start_time,
            duration_secs: 42,
            app_path: r"C:\Apps\editor.exe",
            title: Some("notes"),
            category: Some("work"),
            start_timestamp,
        };

        assert_eq!(
            serde_json::to_string(&row).unwrap(),
            r#"{"date":"1970-01-01","start_time":"08:00:00","duration_secs":42,"app_path":"C:\\Apps\\editor.exe","title":"notes","category":"work","start_timestamp":"1970-01-01T08:00:00+08:00"}"#
        );
    }

    #[test]
    fn oversized_range_has_no_storage_or_output_side_effects() {
        let sandbox = tempfile::tempdir().unwrap();
        let state_root = sandbox.path().join("state");
        let paths = AppPaths::from_root(&state_root);
        let output = sandbox.path().join("existing.json");
        std::fs::write(&output, b"keep this export").unwrap();

        let from = NaiveDate::from_ymd_opt(2020, 1, 1).unwrap();
        let to = from.checked_add_days(Days::new(MAX_QUERY_DAYS)).unwrap();
        let error = run(args(Format::Json, output.clone(), from, to), &paths, false).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(std::fs::read(&output).unwrap(), b"keep this export");
        assert!(!paths.key_file.exists());
        assert!(!paths.apps_dict.exists());
        assert!(!paths.titles_dict.exists());
        assert!(!state_root.exists());

        let missing_parent = sandbox.path().join("new-output-directory");
        let missing_output = missing_parent.join("export.json");
        let error = run(args(Format::Json, missing_output, from, to), &paths, false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!missing_parent.exists());
        assert!(!state_root.exists());
    }

    #[test]
    fn read_error_preserves_existing_output_and_cleans_temporary_file() {
        let sandbox = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_root(&sandbox.path().join("state"));
        let output = sandbox.path().join("existing.csv");
        std::fs::write(&output, b"keep this export").unwrap();

        let key = load_or_create_master_key(&paths.key_file, false).unwrap();
        let cipher = Cipher::new(&key);
        let calendar = SystemCalendar::new();
        let from = calendar.today_at(now_unix()).unwrap();
        let to = from.succ_opt().unwrap();
        let range_start = calendar.day_start(from).unwrap();
        let first_shard_date = unix_to_utc_date(range_start);
        let first_shard_start = utc_midnight_unix(first_shard_date);
        let first_shard_end = first_shard_start + 86_400;
        let duration = (first_shard_end - range_start).min(60) as u32;
        assert!(duration > 0);

        let first_record = Record {
            start_offset_secs: (range_start - first_shard_start) as u32,
            duration_secs: duration,
            app_id: 1,
            title_id: 0,
            flags: 0,
        };
        let first_path = paths.log_file_for_day(
            first_shard_date.year,
            first_shard_date.month,
            first_shard_date.day,
        );
        let mut writer = LogWriter::open(&first_path, cipher, first_shard_date).unwrap();
        writer.write_block(&[first_record]).unwrap();
        drop(writer);

        let second_shard_start = first_shard_start + 86_400;
        let second_shard_date = unix_to_utc_date(second_shard_start);
        let second_path = paths.log_file_for_day(
            second_shard_date.year,
            second_shard_date.month,
            second_shard_date.day,
        );
        std::fs::create_dir_all(second_path.parent().unwrap()).unwrap();
        std::fs::write(second_path, b"bad log header").unwrap();

        let error = run(args(Format::Csv, output.clone(), from, to), &paths, false).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&output).unwrap(), b"keep this export");
        assert_no_pending_exports(sandbox.path());
    }

    #[test]
    fn write_error_preserves_existing_output_and_cleans_temporary_file() {
        let sandbox = tempfile::tempdir().unwrap();
        let output = sandbox.path().join("existing.json");
        std::fs::write(&output, b"keep this export").unwrap();

        let error = write_atomically(&output, |out| {
            out.write_all(b"partial replacement")?;
            Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "injected export write failure",
            ))
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::WriteZero);
        assert_eq!(std::fs::read(&output).unwrap(), b"keep this export");
        assert_no_pending_exports(sandbox.path());
    }

    #[test]
    fn successful_export_atomically_replaces_existing_output() {
        let sandbox = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_root(&sandbox.path().join("state"));
        let output = sandbox.path().join("existing.json");
        std::fs::write(&output, b"old export").unwrap();

        let calendar = SystemCalendar::new();
        let today = calendar.today_at(now_unix()).unwrap();
        run(
            args(Format::Json, output.clone(), today, today),
            &paths,
            false,
        )
        .unwrap();

        assert_eq!(std::fs::read(&output).unwrap(), b"[]");
        assert_no_pending_exports(sandbox.path());
    }
}
