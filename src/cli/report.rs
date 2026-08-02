//! 报表：按 app / category / title 聚合时长。

use std::collections::HashMap;
use std::path::Path;

use chrono::{Days, NaiveDate};
use lexopt::prelude::*;

use crate::classifier::Classifier;
use crate::local_time::{Calendar, SystemCalendar};
use crate::paths::AppPaths;
use crate::storage::crypto::{load_or_create_master_key, Cipher};
use crate::storage::dict::Dict;
use crate::storage::query::visit_local_date_range;
use crate::storage::writer::now_unix;

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
    --from YYYY-MM-DD     起始本地日期（含）
    --to   YYYY-MM-DD     截止本地日期（含）
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
        from: None,
        to: None,
        today: false,
        yesterday: false,
        week: false,
        month: false,
        by: By::App,
        top: 20,
    };
    while let Some(arg) = p.next()? {
        match arg {
            Short('h') | Long("help") => {
                print!("{REPORT_HELP}");
                std::process::exit(0);
            }
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
                    other => {
                        return Err(lexopt::Error::UnexpectedValue {
                            option: "--by".into(),
                            value: other.into(),
                        })
                    }
                };
            }
            Long("top") => args.top = p.value()?.parse()?,
            _ => return Err(arg.unexpected()),
        }
    }
    Ok(args)
}

pub fn run(args: ReportArgs, paths: &AppPaths, machine_scope: bool) -> std::io::Result<()> {
    let calendar = SystemCalendar::new();
    let today = calendar.today_at(now_unix())?;
    let (from, to) = resolve_range(&args, today)?;
    let key = load_or_create_master_key(&paths.key_file, machine_scope)?;
    let cipher = Cipher::new(&key);
    let apps = Dict::open(&paths.apps_dict)?;
    let titles = Dict::open(&paths.titles_dict)?;
    let classifier = Classifier::load(&paths.rules_file)?;

    let mut totals: HashMap<String, u64> = HashMap::new();
    visit_local_date_range(paths, &cipher, &calendar, from, to, |r| {
        let exe = apps.get(r.app_id).unwrap_or("?");
        let title = if r.title_id == 0 {
            None
        } else {
            titles.get(r.title_id)
        };
        let key = match args.by {
            By::App => display_app(exe),
            By::Title => title.unwrap_or("(no title)").to_string(),
            By::Category => classifier
                .classify(exe, title)
                .unwrap_or("(uncategorized)")
                .to_string(),
        };
        *totals.entry(key).or_insert(0) += r.duration_secs as u64;
        Ok(())
    })?;

    let mut rows: Vec<_> = totals.into_iter().collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.1));
    rows.truncate(args.top);

    let total: u64 = rows.iter().map(|(_, s)| *s).sum();
    println!("Range: {}  →  {}", fmt_date(from), fmt_date(to));
    println!(
        "By: {:?}    Top: {}    Total in scope: {}",
        args.by,
        args.top,
        fmt_dur(total)
    );
    println!();
    println!("{:>12}  {:>6}  ITEM", "DURATION", "PCT");
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
    if h > 0 {
        format!("{h}h {m:02}m {s:02}s")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

pub(crate) fn fmt_date(d: NaiveDate) -> String {
    d.to_string()
}

pub(crate) fn parse_date(s: &str) -> std::io::Result<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid date {s:?}; expected YYYY-MM-DD"),
        )
    })
}

pub(crate) fn validate_range(from: NaiveDate, to: NaiveDate) -> std::io::Result<()> {
    if from > to {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("start date {from} is after end date {to}"),
        ));
    }
    Ok(())
}

fn resolve_range(args: &ReportArgs, today: NaiveDate) -> std::io::Result<(NaiveDate, NaiveDate)> {
    let explicit_from = args.from.as_deref().map(parse_date).transpose()?;
    let explicit_to = args.to.as_deref().map(parse_date).transpose()?;
    if let (Some(from), Some(to)) = (explicit_from, explicit_to) {
        validate_range(from, to)?;
    }

    if args.today {
        return Ok((today, today));
    }
    if args.yesterday {
        let y = subtract_days(today, 1)?;
        return Ok((y, y));
    }
    if args.week {
        let from = subtract_days(today, 6)?;
        return Ok((from, today));
    }
    if args.month {
        let from = subtract_days(today, 29)?;
        return Ok((from, today));
    }

    let from = explicit_from.unwrap_or(today);
    let to = explicit_to.unwrap_or(today);
    validate_range(from, to)?;
    Ok((from, to))
}

fn subtract_days(date: NaiveDate, days: u64) -> std::io::Result<NaiveDate> {
    date.checked_sub_days(Days::new(days)).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "date range is out of bounds",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> ReportArgs {
        ReportArgs {
            from: None,
            to: None,
            today: false,
            yesterday: false,
            week: false,
            month: false,
            by: By::App,
            top: 20,
        }
    }

    #[test]
    fn parse_date_rejects_invalid_calendar_date() {
        assert!(parse_date("2026-02-29").is_err());
    }

    #[test]
    fn week_uses_seven_calendar_dates() {
        let mut args = args();
        args.week = true;
        let today = NaiveDate::from_ymd_opt(2026, 3, 2).unwrap();
        let (from, to) = resolve_range(&args, today).unwrap();
        assert_eq!(from, NaiveDate::from_ymd_opt(2026, 2, 24).unwrap());
        assert_eq!(to, today);
    }

    #[test]
    fn reversed_explicit_range_is_rejected() {
        let mut args = args();
        args.from = Some("2026-07-27".into());
        args.to = Some("2026-07-26".into());
        let today = NaiveDate::from_ymd_opt(2026, 7, 26).unwrap();
        assert!(resolve_range(&args, today).is_err());
    }
}
