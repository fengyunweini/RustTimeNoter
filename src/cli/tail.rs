//! `tracker tail` — 实时跟随本地当日活动。

use std::time::Duration;

use lexopt::prelude::*;

use crate::local_time::{Calendar, SystemCalendar};
use crate::paths::AppPaths;
use crate::storage::crypto::{load_or_create_master_key, Cipher};
use crate::storage::dict::Dict;
use crate::storage::query::read_local_date_range;
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
    let apps = Dict::open(&paths.apps_dict)?;
    let titles = Dict::open(&paths.titles_dict)?;
    let calendar = SystemCalendar::new();

    let mut printed: usize = 0;
    let mut first = true;
    let mut current_day = None;

    loop {
        let today = calendar.today_at(now_unix())?;
        let day_identity = (today, calendar.day_start(today)?);
        if current_day != Some(day_identity) {
            current_day = Some(day_identity);
            printed = 0;
            first = true;
        }
        let recs = read_local_date_range(paths, &cipher, &calendar, today, today)?;

        let start = if first {
            first = false;
            recs.len().saturating_sub(args.history)
        } else {
            printed.min(recs.len())
        };

        for r in recs.iter().skip(start) {
            let local_time = calendar.format_time(r.start_unix)?;
            let exe = apps.get(r.app_id).unwrap_or("?");
            let title = if r.title_id == 0 {
                ""
            } else {
                titles.get(r.title_id).unwrap_or("")
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
            println!(
                "[{local_time} for {:>4}s] {}{}",
                r.duration_secs, exe_short, title_part
            );
        }
        printed = recs.len();

        if args.once {
            break;
        }
        std::thread::sleep(Duration::from_secs(args.interval.max(1)));
    }
    Ok(())
}
