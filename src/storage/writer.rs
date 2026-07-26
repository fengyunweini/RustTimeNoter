//! Daemon-side 写入器：聚合段 → 字典 → 日志。
//!
//! 单线程消费 `mpsc::Receiver<WriterMsg>`，跨日自动切文件。
//! 持有当前日期的 `LogWriter`、apps + titles 两本 `Dict`、缓冲若干 `Record`。
//! 触发 flush 的条件：
//! - 缓冲达到 `flush_block_records`
//! - 距上次 flush 超过 `flush_interval_secs`
//! - 收到 `Flush` 显式消息
//! - 收到 `Shutdown`

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::crypto::{load_or_create_master_key, Cipher};
use super::dict::Dict;
use super::log::{LogDate, LogWriter};
use super::model::{Record, Segment};
use crate::paths::{AppPaths, InstallScope};

pub enum WriterMsg {
    Segment(Segment),
    Flush,
    Shutdown,
}

pub struct WriterConfig {
    pub paths: AppPaths,
    pub scope: InstallScope,
    pub flush_block_records: u32,
    pub flush_interval_secs: u32,
}

pub fn run(cfg: WriterConfig, rx: Receiver<WriterMsg>) -> std::io::Result<()> {
    cfg.paths.ensure_dirs()?;
    let machine_scope = cfg.scope == InstallScope::Machine;
    let key = load_or_create_master_key(&cfg.paths.key_file, machine_scope)?;
    let cipher = Cipher::new(&key);
    let mut apps = Dict::open(&cfg.paths.apps_dict)?;
    let mut titles = Dict::open(&cfg.paths.titles_dict)?;

    let mut current: Option<(LogDate, LogWriter, PathBuf)> = None;
    let mut buffer: Vec<Record> = Vec::with_capacity(cfg.flush_block_records as usize);
    let mut last_flush = Instant::now();
    let flush_interval = Duration::from_secs(cfg.flush_interval_secs.max(1) as u64);

    loop {
        let timeout = flush_interval
            .checked_sub(last_flush.elapsed())
            .unwrap_or(Duration::from_millis(0));
        match rx.recv_timeout(timeout) {
            Ok(WriterMsg::Segment(seg)) => {
                process_segment(
                    seg,
                    &mut apps,
                    &mut titles,
                    &mut buffer,
                    &mut current,
                    &cfg,
                    &cipher,
                )?;
                if buffer.len() >= cfg.flush_block_records as usize {
                    flush(&mut buffer, current.as_mut())?;
                    last_flush = Instant::now();
                }
            }
            Ok(WriterMsg::Flush) => {
                flush(&mut buffer, current.as_mut())?;
                last_flush = Instant::now();
            }
            Ok(WriterMsg::Shutdown) => {
                flush(&mut buffer, current.as_mut())?;
                return Ok(());
            }
            Err(RecvTimeoutError::Timeout) => {
                if !buffer.is_empty() {
                    flush(&mut buffer, current.as_mut())?;
                }
                last_flush = Instant::now();
            }
            Err(RecvTimeoutError::Disconnected) => {
                flush(&mut buffer, current.as_mut())?;
                return Ok(());
            }
        }
    }
}

fn process_segment(
    seg: Segment,
    apps: &mut Dict,
    titles: &mut Dict,
    buffer: &mut Vec<Record>,
    current: &mut Option<(LogDate, LogWriter, PathBuf)>,
    cfg: &WriterConfig,
    cipher: &Cipher,
) -> std::io::Result<()> {
    if seg.duration() == 0 {
        return Ok(());
    }
    let app_id = apps.intern(&seg.app_path)?;
    let title_id = match seg.title.as_deref().filter(|s| !s.is_empty()) {
        Some(t) => titles.intern(t)?,
        None => 0,
    };

    // 跨日拆分
    for piece in split_by_day(seg.start_unix, seg.end_unix) {
        let date = unix_to_utc_date(piece.0);
        // 切换/打开当日 writer
        if current
            .as_ref()
            .map(|(d, _, _)| *d != date)
            .unwrap_or(true)
        {
            // flush 旧缓冲到旧 writer 后再切
            flush(buffer, current.as_mut())?;
            let path = cfg.paths.log_file_for_day(date.year, date.month, date.day);
            let lw = LogWriter::open(&path, cipher.clone(), date)?;
            *current = Some((date, lw, path));
        }
        let day_start = utc_midnight_unix(date);
        let start_offset = piece.0.saturating_sub(day_start) as u32;
        let dur = piece.1.saturating_sub(piece.0) as u32;
        if dur == 0 {
            continue;
        }
        buffer.push(Record {
            start_offset_secs: start_offset,
            duration_secs: dur,
            app_id,
            title_id,
            flags: 0,
        });
    }
    Ok(())
}

fn flush(
    buffer: &mut Vec<Record>,
    current: Option<&mut (LogDate, LogWriter, PathBuf)>,
) -> std::io::Result<()> {
    if buffer.is_empty() {
        return Ok(());
    }
    if let Some((_, w, _)) = current {
        w.write_block(buffer)?;
    }
    buffer.clear();
    Ok(())
}

/// 把 [start, end) 按 UTC 自然日切片，返回每段的 (start_unix, end_unix)。
fn split_by_day(start: u64, end: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut s = start;
    while s < end {
        let date = unix_to_utc_date(s);
        let next_midnight = utc_midnight_unix(date) + 86400;
        let e = end.min(next_midnight);
        out.push((s, e));
        s = e;
    }
    if out.is_empty() && start == end {
        // 0 长度段不输出
    }
    out
}

// ── 极简 UTC 日期工具，不依赖 chrono ──────────────────────────────────────
// 仅处理 1970-01-01 之后的正常日期。

pub fn unix_to_utc_date(t: u64) -> LogDate {
    let days = (t / 86400) as i64;
    let (y, m, d) = days_to_ymd(days);
    LogDate { year: y, month: m as u32, day: d as u32 }
}

pub fn utc_midnight_unix(date: LogDate) -> u64 {
    let days = ymd_to_days(date.year, date.month as i64, date.day as i64);
    (days as u64) * 86400
}

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// Howard Hinnant's chrono algorithm（公有领域），days = days since 1970-01-01。
fn days_to_ymd(days: i64) -> (i32, i64, i64) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as i64, d as i64)
}

fn ymd_to_days(y: i32, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y as i64 - 1 } else { y as i64 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let doy = ((153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1) as u64; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe as i64 - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_round_trip() {
        for &(y, m, d) in &[(1970i32, 1u32, 1u32), (2000, 2, 29), (2026, 4, 22), (2099, 12, 31)] {
            let date = LogDate { year: y, month: m, day: d };
            let t = utc_midnight_unix(date);
            assert_eq!(unix_to_utc_date(t), date);
            assert_eq!(unix_to_utc_date(t + 3600), date);
            assert_eq!(unix_to_utc_date(t + 86399), date);
        }
    }

    #[test]
    fn cross_day_split() {
        let date = LogDate { year: 2026, month: 4, day: 22 };
        let mid = utc_midnight_unix(date);
        let pieces = split_by_day(mid + 86000, mid + 86400 + 200);
        assert_eq!(pieces.len(), 2);
        assert_eq!(pieces[0], (mid + 86000, mid + 86400));
        assert_eq!(pieces[1], (mid + 86400, mid + 86400 + 200));
    }

    #[test]
    fn writer_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_root(dir.path());
        paths.ensure_dirs().unwrap();
        let cfg = WriterConfig {
            paths: paths.clone(),
            scope: InstallScope::User,
            flush_block_records: 2,
            flush_interval_secs: 60,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let date = LogDate { year: 2026, month: 4, day: 22 };
        let day0 = utc_midnight_unix(date);
        tx.send(WriterMsg::Segment(Segment {
            app_path: "C:/a.exe".into(),
            app_basename: "a.exe".into(),
            title: Some("hello".into()),
            start_unix: day0 + 10,
            end_unix: day0 + 20,
        }))
        .unwrap();
        tx.send(WriterMsg::Segment(Segment {
            app_path: "C:/b.exe".into(),
            app_basename: "b.exe".into(),
            title: None,
            start_unix: day0 + 20,
            end_unix: day0 + 30,
        }))
        .unwrap();
        tx.send(WriterMsg::Flush).unwrap();
        tx.send(WriterMsg::Shutdown).unwrap();
        run(cfg, rx).unwrap();

        // 读回
        let key = load_or_create_master_key(&paths.key_file, false).unwrap();
        let r = super::super::log::LogReader::new(Cipher::new(&key), date)
            .read_all(&paths.log_file_for_day(date.year, date.month, date.day))
            .unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].duration_secs, 10);
        assert_eq!(r[0].app_id, 1);
        assert_eq!(r[0].title_id, 1);
        assert_eq!(r[1].app_id, 2);
        assert_eq!(r[1].title_id, 0);
    }
}
