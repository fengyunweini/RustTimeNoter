//! 导出 CSV / JSON 原始记录。

use std::io::Write;
use std::path::PathBuf;

use lexopt::prelude::*;
use serde::Serialize;

use crate::classifier::Classifier;
use crate::cli::report::{fmt_date, next_day, parse_date};
use crate::paths::AppPaths;
use crate::storage::crypto::{load_or_create_master_key, Cipher};
use crate::storage::dict::Dict;
use crate::storage::log::LogReader;
use crate::storage::writer::{now_unix, unix_to_utc_date, utc_midnight_unix};

pub struct ExportArgs {
    pub format: Format,
    pub out: PathBuf,
    pub from: Option<String>,
    pub to: Option<String>,
}

#[derive(Clone, Copy)]
pub enum Format { Csv, Json }

#[derive(Serialize)]
struct Row<'a> {
    date: String,
    start_time: String,
    duration_secs: u32,
    app_path: &'a str,
    title: Option<&'a str>,
    category: Option<&'a str>,
}

const EXPORT_HELP: &str = "\
tracker export — 导出原始记录

OPTIONS:
    --format csv|json     输出格式（默认 csv）
    --out PATH            输出文件路径（必填）
    --from YYYY-MM-DD     起始（默认今天）
    --to   YYYY-MM-DD     截止（默认今天）
    -h, --help            打印帮助
";

pub fn parse(p: &mut lexopt::Parser) -> Result<ExportArgs, lexopt::Error> {
    let mut format = Format::Csv;
    let mut out: Option<PathBuf> = None;
    let mut from: Option<String> = None;
    let mut to: Option<String> = None;
    while let Some(arg) = p.next()? {
        match arg {
            Short('h') | Long("help") => { print!("{EXPORT_HELP}"); std::process::exit(0); }
            Long("format") => {
                format = match p.value()?.string()?.as_str() {
                    "csv" => Format::Csv,
                    "json" => Format::Json,
                    other => return Err(lexopt::Error::UnexpectedValue {
                        option: "--format".into(),
                        value: other.into(),
                    }),
                };
            }
            Long("out") => out = Some(p.value()?.into()),
            Long("from") => from = Some(p.value()?.string()?),
            Long("to") => to = Some(p.value()?.string()?),
            _ => return Err(arg.unexpected()),
        }
    }
    let out = out.ok_or(lexopt::Error::MissingValue { option: Some("--out".into()) })?;
    Ok(ExportArgs { format, out, from, to })
}

pub fn run(args: ExportArgs, paths: &AppPaths, machine_scope: bool) -> std::io::Result<()> {
    let today = unix_to_utc_date(now_unix());
    let from = args.from.as_deref().map(parse_date).transpose()?.unwrap_or(today);
    let to = args.to.as_deref().map(parse_date).transpose()?.unwrap_or(today);

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

    let mut date = from;
    let mut json_first = true;
    if matches!(args.format, Format::Csv) {
        writeln!(out, "date,start_time,duration_secs,app_path,title,category")?;
    } else {
        write!(out, "[")?;
    }

    while date <= to {
        let path = paths.log_file_for_day(date.year, date.month, date.day);
        let recs = LogReader::new(cipher.clone(), date).read_all(&path)?;
        let day_start = utc_midnight_unix(date);
        for r in recs {
            let exe = apps.get(r.app_id).unwrap_or("?");
            let title = if r.title_id == 0 { None } else { titles.get(r.title_id) };
            let category = classifier.classify(exe, title);
            let start_time = fmt_time_of_day(r.start_offset_secs);
            let _start_unix = day_start + r.start_offset_secs as u64;
            match args.format {
                Format::Csv => {
                    writeln!(
                        out,
                        "{},{},{},{},{},{}",
                        fmt_date(date),
                        start_time,
                        r.duration_secs,
                        csv_escape(exe),
                        csv_escape(title.unwrap_or("")),
                        csv_escape(category.unwrap_or("")),
                    )?;
                }
                Format::Json => {
                    if !json_first {
                        write!(out, ",")?;
                    }
                    json_first = false;
                    let row = Row {
                        date: fmt_date(date),
                        start_time,
                        duration_secs: r.duration_secs,
                        app_path: exe,
                        title,
                        category,
                    };
                    serde_json::to_writer(&mut out, &row)?;
                }
            }
        }
        date = next_day(date);
    }

    if matches!(args.format, Format::Json) {
        write!(out, "]")?;
    }
    out.flush()?;
    println!("wrote {}", args.out.display());
    Ok(())
}

fn fmt_time_of_day(secs: u32) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}
