//! 导出 CSV / JSON 原始记录。

use std::io::Write;
use std::path::PathBuf;

use lexopt::prelude::*;
use serde::Serialize;

use crate::classifier::Classifier;
use crate::cli::report::{fmt_date, parse_date, validate_range};
use crate::local_time::{Calendar, SystemCalendar};
use crate::paths::AppPaths;
use crate::storage::crypto::{load_or_create_master_key, Cipher};
use crate::storage::dict::Dict;
use crate::storage::query::visit_local_date_range;
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
    validate_range(from, to)?;

    let key = load_or_create_master_key(&paths.key_file, machine_scope)?;
    let cipher = Cipher::new(&key);
    let apps = Dict::open(&paths.apps_dict)?;
    let titles = Dict::open(&paths.titles_dict)?;
    let classifier = Classifier::load(&paths.rules_file)?;

    if let Some(parent) = args.out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut out = std::io::BufWriter::new(std::fs::File::create(&args.out)?);

    let mut json_first = true;
    if matches!(args.format, Format::Csv) {
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
        match args.format {
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
                serde_json::to_writer(&mut out, &row)?;
            }
        }
        Ok(())
    })?;

    if matches!(args.format, Format::Json) {
        write!(out, "]")?;
    }
    out.flush()?;
    println!("wrote {}", args.out.display());
    Ok(())
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
    use crate::local_time::TestCalendar;

    use super::*;

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
}
