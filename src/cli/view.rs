//! `tracker view` — 把最近 N 天的统计渲染成自包含 HTML，用默认浏览器打开。
//! 这是"能看但不需要完美"的界面：零额外依赖，零二进制膨胀。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lexopt::prelude::*;

use crate::paths::AppPaths;
use crate::storage::crypto::{load_or_create_master_key, Cipher};
use crate::storage::dict::Dict;
use crate::storage::log::{LogDate, LogReader};
use crate::storage::writer::{now_unix, unix_to_utc_date, utc_midnight_unix};

pub struct ViewArgs {
    pub days: u32,
    pub no_open: bool,
    pub out: Option<PathBuf>,
}

const VIEW_HELP: &str = "\
tracker view — 生成最近若干天的 HTML 报表，并用默认浏览器打开

OPTIONS:
    --days N       统计最近 N 天（含今日，默认 7）
    --out PATH     指定输出 HTML 路径（默认写到临时目录）
    --no-open      只生成文件，不调用浏览器
    -h, --help     打印帮助
";

pub fn parse(p: &mut lexopt::Parser) -> Result<ViewArgs, lexopt::Error> {
    let mut args = ViewArgs { days: 7, no_open: false, out: None };
    while let Some(arg) = p.next()? {
        match arg {
            Short('h') | Long("help") => { print!("{VIEW_HELP}"); std::process::exit(0); }
            Long("days") => args.days = p.value()?.parse()?,
            Long("no-open") => args.no_open = true,
            Long("out") => args.out = Some(PathBuf::from(p.value()?.string()?)),
            _ => return Err(arg.unexpected()),
        }
    }
    if args.days == 0 { args.days = 1; }
    Ok(args)
}

pub fn run(args: ViewArgs, paths: &AppPaths, machine_scope: bool) -> std::io::Result<()> {
    let key = load_or_create_master_key(&paths.key_file, machine_scope)?;
    let cipher = Cipher::new(&key);
    let apps = Dict::open(&paths.apps_dict)?;

    let today = unix_to_utc_date(now_unix());
    let from_unix = utc_midnight_unix(today).saturating_sub((args.days as u64 - 1) * 86400);
    let from = unix_to_utc_date(from_unix);

    let mut by_day: Vec<(LogDate, Vec<(String, u64)>, u64)> = Vec::new();
    let mut by_app_total: HashMap<String, u64> = HashMap::new();

    let mut date = from;
    while date <= today {
        let path = paths.log_file_for_day(date.year, date.month, date.day);
        let recs = LogReader::new(cipher.clone(), date).read_all(&path).unwrap_or_default();
        let mut day_totals: HashMap<String, u64> = HashMap::new();
        for r in &recs {
            let exe = apps.get(r.app_id).unwrap_or("?");
            let name = display_app(exe);
            *day_totals.entry(name.clone()).or_insert(0) += r.duration_secs as u64;
            *by_app_total.entry(name).or_insert(0) += r.duration_secs as u64;
        }
        let day_total: u64 = day_totals.values().sum();
        let mut rows: Vec<_> = day_totals.into_iter().collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1));
        by_day.push((date, rows, day_total));
        date = next_day(date);
    }

    let mut overall: Vec<_> = by_app_total.into_iter().collect();
    overall.sort_by(|a, b| b.1.cmp(&a.1));
    let overall_total: u64 = overall.iter().map(|(_, v)| *v).sum();

    let html = render_html(&overall, overall_total, &by_day, args.days, &paths.root);

    let out_path = match args.out.clone() {
        Some(p) => p,
        None => std::env::temp_dir().join(format!(
            "rusttimenoter-{}-{:02}-{:02}.html",
            today.year, today.month, today.day
        )),
    };
    std::fs::write(&out_path, html.as_bytes())?;
    println!("Report: {}", out_path.display());

    if !args.no_open {
        open_in_browser(&out_path)?;
    }
    Ok(())
}

fn display_app(exe_path: &str) -> String {
    Path::new(exe_path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| exe_path.to_string())
}

fn next_day(d: LogDate) -> LogDate {
    let t = utc_midnight_unix(d) + 86400;
    unix_to_utc_date(t)
}

fn fmt_dur(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 { format!("{h}h {m:02}m {s:02}s") }
    else if m > 0 { format!("{m}m {s:02}s") }
    else { format!("{s}s") }
}

fn fmt_date(d: LogDate) -> String {
    format!("{:04}-{:02}-{:02}", d.year, d.month, d.day)
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn render_html(
    overall: &[(String, u64)],
    overall_total: u64,
    by_day: &[(LogDate, Vec<(String, u64)>, u64)],
    days: u32,
    data_root: &Path,
) -> String {
    let mut s = String::new();
    s.push_str("<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">");
    s.push_str("<title>RustTimeNoter Report</title>");
    s.push_str("<style>\
:root{--bg:#0e1116;--card:#161b22;--text:#e6edf3;--muted:#8b949e;--accent:#58a6ff;--bar:#1f6feb;--bar-bg:#21262d;}\
*{box-sizing:border-box}\
body{margin:0;padding:24px;background:var(--bg);color:var(--text);font:14px/1.5 -apple-system,Segoe UI,sans-serif}\
h1{margin:0 0 4px 0;font-size:20px}\
.meta{color:var(--muted);font-size:12px;margin-bottom:24px}\
.card{background:var(--card);border-radius:8px;padding:16px 20px;margin-bottom:20px}\
.card h2{margin:0 0 12px 0;font-size:15px;font-weight:600}\
table{width:100%;border-collapse:collapse}\
td{padding:6px 8px;vertical-align:middle;border-bottom:1px solid #21262d}\
tr:last-child td{border-bottom:none}\
.app{font-weight:500}\
.dur{color:var(--muted);text-align:right;width:120px;font-variant-numeric:tabular-nums}\
.pct{width:40%}\
.bar{height:8px;background:var(--bar-bg);border-radius:4px;overflow:hidden}\
.bar>span{display:block;height:100%;background:var(--bar);border-radius:4px}\
.day-total{color:var(--accent);font-weight:600;margin-bottom:8px}\
.empty{color:var(--muted);font-style:italic;padding:8px 0}\
footer{color:var(--muted);font-size:11px;margin-top:24px;text-align:center}\
</style></head><body>");

    s.push_str("<h1>RustTimeNoter</h1>");
    s.push_str(&format!(
        "<div class=\"meta\">Last {} day{} · Total tracked: <b>{}</b> · Data: <code>{}</code></div>",
        days,
        if days == 1 { "" } else { "s" },
        fmt_dur(overall_total),
        html_escape(&data_root.display().to_string()),
    ));

    // Overall card
    s.push_str("<div class=\"card\"><h2>By application (overall)</h2>");
    if overall.is_empty() {
        s.push_str("<div class=\"empty\">No records yet. Use the computer for a few minutes, then refresh.</div>");
    } else {
        s.push_str("<table>");
        let max = overall.iter().map(|(_, v)| *v).max().unwrap_or(1);
        for (name, secs) in overall.iter().take(30) {
            let pct = (*secs as f64) * 100.0 / (overall_total.max(1) as f64);
            let bar_pct = (*secs as f64) * 100.0 / (max as f64);
            s.push_str(&format!(
                "<tr><td class=\"app\">{}</td><td class=\"pct\"><div class=\"bar\"><span style=\"width:{:.1}%\"></span></div></td><td class=\"dur\">{} · {:.1}%</td></tr>",
                html_escape(name), bar_pct, fmt_dur(*secs), pct,
            ));
        }
        s.push_str("</table>");
    }
    s.push_str("</div>");

    // Per-day cards (most recent first)
    for (date, rows, day_total) in by_day.iter().rev() {
        s.push_str("<div class=\"card\">");
        s.push_str(&format!("<h2>{}</h2>", fmt_date(*date)));
        s.push_str(&format!("<div class=\"day-total\">{}</div>", fmt_dur(*day_total)));
        if rows.is_empty() {
            s.push_str("<div class=\"empty\">No records.</div>");
        } else {
            s.push_str("<table>");
            let max = rows.iter().map(|(_, v)| *v).max().unwrap_or(1);
            for (name, secs) in rows.iter().take(15) {
                let bar_pct = (*secs as f64) * 100.0 / (max as f64);
                s.push_str(&format!(
                    "<tr><td class=\"app\">{}</td><td class=\"pct\"><div class=\"bar\"><span style=\"width:{:.1}%\"></span></div></td><td class=\"dur\">{}</td></tr>",
                    html_escape(name), bar_pct, fmt_dur(*secs),
                ));
            }
            s.push_str("</table>");
        }
        s.push_str("</div>");
    }

    s.push_str("<footer>RustTimeNoter · static HTML report · refresh page after running <code>tracker view</code> again</footer>");
    s.push_str("</body></html>");
    s
}

#[cfg(windows)]
fn open_in_browser(path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let verb: Vec<u16> = "open\0".encode_utf16().collect();
    let file: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL as i32,
        )
    };
    // ShellExecuteW returns HINSTANCE; values <= 32 are errors.
    if (r as isize) <= 32 {
        return Err(std::io::Error::other(format!(
            "ShellExecuteW failed (code {})", r as isize
        )));
    }
    Ok(())
}

#[cfg(not(windows))]
fn open_in_browser(_path: &Path) -> std::io::Result<()> {
    Ok(())
}
