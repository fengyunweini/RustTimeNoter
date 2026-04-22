//! 报表：按 app / category / title 聚合时长。

use std::collections::HashMap;
use std::path::Path;

use lexopt::prelude::*;

use crate::classifier::Classifier;
use crate::paths::AppPaths;
use crate::storage::crypto::{load_or_create_master_key, Cipher};
use crate::storage::dict::Dict;
use crate::storage::log::{LogDate, LogReader};
use crate::storage::writer::{now_unix, unix_to_utc_date, utc_midnight_unix};

pub struct ReportArgs {
    pub from: Option<String>,
    pub to: Option<String>,
    pub today: bool,
    pub yesterday: bool,
    pub week: bool,
    pub month: bool,
    pub by: By,
    pub top: usize,
}

#[derive(Clone, Copy, Debug)]
pub enum By {
    App,
    Category,
    Title,
}

const REPORT_HELP: &str = "\
tracker report — 按 app/category/title 聚合时长

OPTIONS:
    --from YYYY-MM-DD     起始日期（含）
    --to   YYYY-MM-DD     截止日期（含）
    --today               仅今天
    --yesterday           仅昨天
    --week                最近 7 天
    --month               最近 30 天
    --by app|category|title   聚合维度（默认 app）
    --top N               显示前 N 项（默认 20）
    -h, --help            打印帮助
";

pub fn parse(p: &mut lexopt::Parser) -> Result<ReportArgs, lexopt::Error> {
    let mut args = ReportArgs {
        from: None, to: None, today: false, yesterday: false, week: false, month: false,
        by: By::App, top: 20,
    };
    while let Some(arg) = p.next()? {
        match arg {
            Short('h') | Long("help") => { print!("{REPORT_HELP}"); std::process::exit(0); }
            Long("from") => args.from = Some(p.value()?.string()?),
            Long("to") => args.to = Some(p.value()?.string()?),
            Long("today") => args.today = true,
            Long("yesterday") => args.yesterday = true,
            Long("week") => args.week = true,
            Long("month") => args.month = true,
            Long("by") => {
                args.by = match p.value()?.string()?.as_str() {
                    "app" => By::App,
                    "category" => By::Category,
                    "title" => By::Title,
                    other => return Err(lexopt::Error::UnexpectedValue {
                        option: "--by".into(),
                        value: other.into(),
                    }),
                };
            }
            Long("top") => args.top = p.value()?.parse()?,
            _ => return Err(arg.unexpected()),
        }
    }
    Ok(args)
}

pub fn run(args: ReportArgs, paths: &AppPaths, machine_scope: bool) -> std::io::Result<()> {
    let (from, to) = resolve_range(&args)?;
    let key = load_or_create_master_key(&paths.key_file, machine_scope)?;
    let cipher = Cipher::new(&key);
    let apps = Dict::open(&paths.apps_dict)?;
    let titles = Dict::open(&paths.titles_dict)?;
    let classifier = Classifier::load(&paths.rules_file)?;

    let mut totals: HashMap<String, u64> = HashMap::new();
    let mut date = from;
    while date <= to {
        let path = paths.log_file_for_day(date.year, date.month, date.day);
        let recs = LogReader::new(cipher.clone(), date).read_all(&path)?;
        for r in recs {
            let exe = apps.get(r.app_id).unwrap_or("?").to_string();
            let title = if r.title_id == 0 { None } else { titles.get(r.title_id) };
            let key = match args.by {
                By::App => display_app(&exe),
                By::Title => title.unwrap_or("(no title)").to_string(),
                By::Category => classifier.classify(&exe, title).unwrap_or("(uncategorized)").to_string(),
            };
            *totals.entry(key).or_insert(0) += r.duration_secs as u64;
        }
        date = next_day(date);
    }

    let mut rows: Vec<_> = totals.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1));
    rows.truncate(args.top);

    let total: u64 = rows.iter().map(|(_, s)| *s).sum();
    println!("Range: {}  →  {}", fmt_date(from), fmt_date(to));
    println!("By: {:?}    Top: {}    Total in scope: {}", args.by, args.top, fmt_dur(total));
    println!();
    println!("{:>12}  {:>6}  {}", "DURATION", "PCT", "ITEM");
    println!("{}", "-".repeat(60));
    let total_for_pct = if total == 0 { 1 } else { total };
    for (k, v) in &rows {
        let pct = (*v as f64) * 100.0 / (total_for_pct as f64);
        println!("{:>12}  {:>5.1}%  {}", fmt_dur(*v), pct, k);
    }
    Ok(())
}

fn display_app(exe_path: &str) -> String {
    Path::new(exe_path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| exe_path.to_string())
}

pub fn fmt_dur(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 { format!("{h}h {m:02}m {s:02}s") }
    else if m > 0 { format!("{m}m {s:02}s") }
    else { format!("{s}s") }
}

pub fn fmt_date(d: LogDate) -> String {
    format!("{:04}-{:02}-{:02}", d.year, d.month, d.day)
}

pub fn parse_date(s: &str) -> std::io::Result<LogDate> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "expected YYYY-MM-DD"));
    }
    let y: i32 = parts[0].parse().map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad year"))?;
    let m: u32 = parts[1].parse().map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad month"))?;
    let d: u32 = parts[2].parse().map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad day"))?;
    Ok(LogDate { year: y, month: m, day: d })
}

pub fn next_day(d: LogDate) -> LogDate {
    let t = utc_midnight_unix(d) + 86400;
    unix_to_utc_date(t)
}

fn resolve_range(args: &ReportArgs) -> std::io::Result<(LogDate, LogDate)> {
    let today = unix_to_utc_date(now_unix());
    if args.today { return Ok((today, today)); }
    if args.yesterday {
        let y = unix_to_utc_date(utc_midnight_unix(today).saturating_sub(86400));
        return Ok((y, y));
    }
    if args.week {
        let from = unix_to_utc_date(utc_midnight_unix(today).saturating_sub(6 * 86400));
        return Ok((from, today));
    }
    if args.month {
        let from = unix_to_utc_date(utc_midnight_unix(today).saturating_sub(29 * 86400));
        return Ok((from, today));
    }
    if let (Some(f), Some(t)) = (&args.from, &args.to) {
        return Ok((parse_date(f)?, parse_date(t)?));
    }
    if let Some(f) = &args.from {
        return Ok((parse_date(f)?, today));
    }
    Ok((today, today))
}
