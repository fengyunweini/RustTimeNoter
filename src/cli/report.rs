//! 报表：按 app / category / title 聚合时长。

use std::collections::HashMap;
use std::path::Path;

use clap::{Args, ValueEnum};

use crate::classifier::Classifier;
use crate::paths::AppPaths;
use crate::storage::crypto::{load_or_create_master_key, Cipher};
use crate::storage::dict::Dict;
use crate::storage::log::{LogDate, LogReader};
use crate::storage::writer::{now_unix, unix_to_utc_date, utc_midnight_unix};

#[derive(Debug, Args)]
pub struct ReportArgs {
    /// 显式日期范围 (YYYY-MM-DD)。
    #[arg(long)]
    pub from: Option<String>,
    #[arg(long)]
    pub to: Option<String>,
    /// 快捷范围。
    #[arg(long, conflicts_with_all = ["from", "to"])]
    pub today: bool,
    #[arg(long, conflicts_with_all = ["from", "to", "today"])]
    pub yesterday: bool,
    #[arg(long, conflicts_with_all = ["from", "to", "today", "yesterday"])]
    pub week: bool,
    #[arg(long, conflicts_with_all = ["from", "to", "today", "yesterday", "week"])]
    pub month: bool,
    /// 聚合维度。
    #[arg(long, value_enum, default_value_t = By::App)]
    pub by: By,
    /// 显示前 N 项。
    #[arg(long, default_value_t = 20)]
    pub top: usize,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum By {
    App,
    Category,
    Title,
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

fn fmt_dur(secs: u64) -> String {
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
    if args.today {
        return Ok((today, today));
    }
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
