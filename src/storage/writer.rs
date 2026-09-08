//! Daemon storage: dictionary IDs are synchronized before their log records.
//! Empty writers sleep until a message; buffered records retain a flush deadline.
use super::crypto::{load_or_create_master_key, Cipher};
use super::dict::Dict;
use super::log::{LogDate, LogWriter, MAX_WRITE_BLOCK_RECORDS};
use super::model::{Record, Segment, RECORD_FLAG_GAP};
use crate::paths::{AppPaths, InstallScope};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub enum WriterMsg {
    Segment(Segment),
    Gap {
        start_unix: u64,
        end_unix: u64,
    },
    Flush,
    /// Acknowledge only after dictionaries and preceding records are durable.
    FlushAndAck(Sender<Result<(), String>>),
    Shutdown,
}
pub struct WriterConfig {
    pub paths: AppPaths,
    pub scope: InstallScope,
    pub flush_block_records: u32,
    pub flush_interval_secs: u32,
}

struct WriterState {
    apps: Dict,
    titles: Dict,
    current: Option<(LogDate, LogWriter, PathBuf)>,
    buffer: Vec<Record>,
    buffered_since: Option<Instant>,
}
impl WriterState {
    fn flush(&mut self) -> std::io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        // Both barriers must succeed before any referenced ID reaches a log.
        // A failure poisons the dictionary and stops the writer without ACK.
        self.apps.sync_pending()?;
        self.titles.sync_pending()?;
        let (_, writer, _) = self
            .current
            .as_mut()
            .ok_or_else(|| std::io::Error::other("buffered records have no open log"))?;
        writer.write_block(&self.buffer)?;
        self.buffer.clear();
        self.buffered_since = None;
        Ok(())
    }
}

pub fn run(cfg: WriterConfig, rx: Receiver<WriterMsg>) -> std::io::Result<()> {
    cfg.paths.ensure_dirs()?;
    if (!cfg.paths.apps_dict.try_exists()? || !cfg.paths.titles_dict.try_exists()?)
        && super::crypto::existing_logs(&cfg.paths.data_dir)?
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "recorded logs exist but a dictionary is missing; refusing to reuse its IDs",
        ));
    }
    let key = load_or_create_master_key(&cfg.paths.key_file, cfg.scope == InstallScope::Machine)?;
    // An existing DPAPI blob may have survived an interrupted first creation
    // only in the OS cache. Establish its durability before acknowledging any
    // records, without imposing writes on the separate read-only CLI path.
    let key_file = std::fs::OpenOptions::new()
        .write(true)
        .open(&cfg.paths.key_file)?;
    #[cfg(all(test, windows))]
    let key_file = super::dict::startup_sync_handle(&cfg.paths.key_file, key_file)?;
    key_file.sync_all()?;
    drop(key_file);
    let cipher = Cipher::new(&key);
    let (required_app, required_title) =
        super::log::referenced_dictionary_ids(&cfg.paths.data_dir, &cipher)?;
    let mut state = WriterState {
        apps: Dict::open_writer_checked(&cfg.paths.apps_dict, required_app)?,
        titles: Dict::open_writer_checked(&cfg.paths.titles_dict, required_title)?,
        current: None,
        buffer: Vec::with_capacity(block_record_limit(&cfg).min(256)),
        buffered_since: None,
    };
    let flush_interval = Duration::from_secs(cfg.flush_interval_secs.max(1) as u64);
    loop {
        let message = if let Some(since) = state.buffered_since {
            rx.recv_timeout(flush_interval.saturating_sub(since.elapsed()))
        } else {
            rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        match message {
            Ok(WriterMsg::Segment(segment)) => process_segment(segment, &mut state, &cfg, &cipher)?,
            Ok(WriterMsg::Gap {
                start_unix,
                end_unix,
            }) => process_interval(
                start_unix,
                end_unix,
                Record {
                    start_offset_secs: 0,
                    duration_secs: 0,
                    app_id: 0,
                    title_id: 0,
                    flags: RECORD_FLAG_GAP,
                },
                &mut state,
                &cfg,
                &cipher,
            )?,
            Ok(WriterMsg::Flush) | Err(RecvTimeoutError::Timeout) => state.flush()?,
            Ok(WriterMsg::FlushAndAck(ack)) => {
                let result = state.flush();
                let _ = ack.send(result.as_ref().map_err(ToString::to_string).copied());
                result?;
            }
            Ok(WriterMsg::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                state.flush()?;
                return Ok(());
            }
        }
        // recv_timeout can still receive from a busy queue with a zero timeout.
        // Check the deadline after processing too, so a stream cannot starve it.
        if state
            .buffered_since
            .is_some_and(|since| since.elapsed() >= flush_interval)
        {
            state.flush()?;
        }
    }
}

fn process_segment(
    seg: Segment,
    state: &mut WriterState,
    cfg: &WriterConfig,
    cipher: &Cipher,
) -> std::io::Result<()> {
    if seg.duration() == 0 {
        return Ok(());
    }
    let app_id = state.apps.intern_buffered(&seg.app_path)?;
    let title_id = match seg.title.as_deref().filter(|title| !title.is_empty()) {
        Some(title) => state.titles.intern_buffered(title)?,
        None => 0,
    };
    process_interval(
        seg.start_unix,
        seg.end_unix,
        Record {
            start_offset_secs: 0,
            duration_secs: 0,
            app_id,
            title_id,
            flags: 0,
        },
        state,
        cfg,
        cipher,
    )
}

fn process_interval(
    start: u64,
    end: u64,
    template: Record,
    state: &mut WriterState,
    cfg: &WriterConfig,
    cipher: &Cipher,
) -> std::io::Result<()> {
    for piece in split_by_day(start, end) {
        let date = unix_to_utc_date(piece.0);
        if state
            .current
            .as_ref()
            .map(|(current_date, _, _)| *current_date != date)
            .unwrap_or(true)
        {
            state.flush()?;
            let path = cfg.paths.log_file_for_day(date.year, date.month, date.day);
            let writer = LogWriter::open_daily(&path, cipher.clone(), date)?;
            state.current = Some((date, writer, path));
        }
        let duration_secs = piece.1.saturating_sub(piece.0) as u32;
        if duration_secs == 0 {
            continue;
        }
        if state.buffer.is_empty() {
            state.buffered_since = Some(Instant::now());
        }
        state.buffer.push(Record {
            start_offset_secs: piece.0.saturating_sub(utc_midnight_unix(date)) as u32,
            duration_secs,
            ..template
        });
        if state.buffer.len() >= block_record_limit(cfg) {
            state.flush()?;
        }
    }
    Ok(())
}
fn block_record_limit(cfg: &WriterConfig) -> usize {
    (cfg.flush_block_records as usize).clamp(1, MAX_WRITE_BLOCK_RECORDS)
}
/// 把 [start, end) 按 UTC 自然日切片，返回每段的 (start_unix, end_unix)。
fn split_by_day(start: u64, end: u64) -> impl Iterator<Item = (u64, u64)> {
    let mut next_start = start;
    std::iter::from_fn(move || {
        if next_start >= end {
            return None;
        }
        let next_midnight = utc_midnight_unix(unix_to_utc_date(next_start)).saturating_add(86_400);
        let piece_end = end.min(next_midnight);
        if piece_end <= next_start {
            return None;
        }
        let piece = (next_start, piece_end);
        next_start = piece_end;
        Some(piece)
    })
}

// ── 极简 UTC 日期工具，不依赖 chrono ──────────────────────────────────────
// 仅处理 1970-01-01 之后的正常日期。

pub fn unix_to_utc_date(t: u64) -> LogDate {
    let days = (t / 86400) as i64;
    let (y, m, d) = days_to_ymd(days);
    LogDate {
        year: y,
        month: m as u32,
        day: d as u32,
    }
}

pub fn utc_midnight_unix(date: LogDate) -> u64 {
    let days = ymd_to_days(date.year, date.month as i64, date.day as i64);
    (days as u64) * 86400
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
        for &(y, m, d) in &[
            (1970i32, 1u32, 1u32),
            (2000, 2, 29),
            (2026, 4, 22),
            (2099, 12, 31),
        ] {
            let date = LogDate {
                year: y,
                month: m,
                day: d,
            };
            let t = utc_midnight_unix(date);
            assert_eq!(unix_to_utc_date(t), date);
            assert_eq!(unix_to_utc_date(t + 3600), date);
            assert_eq!(unix_to_utc_date(t + 86399), date);
        }
    }

    #[test]
    fn cross_day_split() {
        let date = LogDate {
            year: 2026,
            month: 4,
            day: 22,
        };
        let mid = utc_midnight_unix(date);
        let pieces: Vec<_> = split_by_day(mid + 86000, mid + 86400 + 200).collect();
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
        let date = LogDate {
            year: 2026,
            month: 4,
            day: 22,
        };
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

    #[test]
    fn gap_crosses_midnight_and_ack_confirms_readable_records() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_root(dir.path());
        let cfg = WriterConfig {
            paths: paths.clone(),
            scope: InstallScope::User,
            flush_block_records: u32::MAX,
            flush_interval_secs: 60,
        };
        assert_eq!(block_record_limit(&cfg), MAX_WRITE_BLOCK_RECORDS);
        let date = LogDate {
            year: 2026,
            month: 9,
            day: 5,
        };
        let midnight = utc_midnight_unix(date) + 86_400;
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || run(cfg, rx));
        tx.send(WriterMsg::Gap {
            start_unix: midnight - 10,
            end_unix: midnight + 20,
        })
        .unwrap();
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        tx.send(WriterMsg::FlushAndAck(ack_tx)).unwrap();
        assert_eq!(
            ack_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            Ok(())
        );
        let key = load_or_create_master_key(&paths.key_file, false).unwrap();
        let mut records = Vec::new();
        for at in [midnight - 10, midnight + 20] {
            let day = unix_to_utc_date(at);
            records.extend(
                super::super::log::LogReader::new(Cipher::new(&key), day)
                    .read_day(&paths.log_file_for_day(day.year, day.month, day.day))
                    .unwrap()
                    .records,
            );
        }
        assert_eq!(records.len(), 2);
        assert_eq!(
            records
                .iter()
                .map(|record| record.duration_secs)
                .sum::<u32>(),
            30
        );
        assert!(records.iter().all(|record| record.flags == RECORD_FLAG_GAP
            && record.app_id == 0
            && record.title_id == 0));
        assert!(Dict::open(&paths.apps_dict).unwrap().is_empty());
        tx.send(WriterMsg::Shutdown).unwrap();
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn writer_failure_cannot_produce_success_ack() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_root(dir.path());
        paths.ensure_dirs().unwrap();
        let date = LogDate {
            year: 2026,
            month: 9,
            day: 5,
        };
        std::fs::create_dir_all(paths.log_file_for_day(date.year, date.month, date.day)).unwrap();
        let cfg = WriterConfig {
            paths,
            scope: InstallScope::User,
            flush_block_records: 2,
            flush_interval_secs: 60,
        };
        let start = utc_midnight_unix(date);
        let (tx, rx) = std::sync::mpsc::channel();
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        tx.send(WriterMsg::Gap {
            start_unix: start,
            end_unix: start + 10,
        })
        .unwrap();
        tx.send(WriterMsg::FlushAndAck(ack_tx)).unwrap();
        assert!(run(cfg, rx).is_err());
        assert!(ack_rx.recv().is_err());
    }

    #[test]
    fn truncated_dictionary_cannot_reuse_ids_referenced_by_history() {
        for uppercase_history in [false, true] {
            for truncate_titles in [false, true] {
                for partial_entry in [false, true] {
                    let dir = tempfile::tempdir().unwrap();
                    let paths = AppPaths::from_root(dir.path());
                    let date = LogDate {
                        year: 2026,
                        month: 9,
                        day: 4,
                    };
                    let start = utc_midnight_unix(date);
                    let config = || WriterConfig {
                        paths: paths.clone(),
                        scope: InstallScope::User,
                        flush_block_records: 1,
                        flush_interval_secs: 60,
                    };
                    let segment = |index: u64| {
                        WriterMsg::Segment(Segment {
                            app_path: format!("C:/app-{index}.exe"),
                            app_basename: format!("app-{index}.exe"),
                            title: Some(format!("title-{index}")),
                            start_unix: start + index * 10,
                            end_unix: start + index * 10 + 5,
                        })
                    };
                    let (tx, rx) = std::sync::mpsc::channel();
                    tx.send(segment(1)).unwrap();
                    tx.send(segment(2)).unwrap();
                    tx.send(WriterMsg::Shutdown).unwrap();
                    run(config(), rx).unwrap();
                    let dict_path = if truncate_titles {
                        &paths.titles_dict
                    } else {
                        &paths.apps_dict
                    };
                    let mut bytes = std::fs::read(dict_path).unwrap();
                    let first_length =
                        u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
                    // Both a complete missing entry and a plausible torn text tail
                    // previously permitted reusing ID 2 after restart.
                    bytes.truncate(16 + first_length + if partial_entry { 9 } else { 0 });
                    std::fs::write(dict_path, &bytes).unwrap();
                    let mut log_path = paths.log_file_for_day(date.year, date.month, date.day);
                    if uppercase_history {
                        let intermediate = log_path.with_extension("case-change");
                        std::fs::rename(&log_path, &intermediate).unwrap();
                        log_path.set_extension("LOG");
                        std::fs::rename(intermediate, &log_path).unwrap();
                    }
                    let log_before = std::fs::read(&log_path).unwrap();
                    let (tx, rx) = std::sync::mpsc::channel();
                    let (ack_tx, ack_rx) = std::sync::mpsc::channel();
                    tx.send(segment(3)).unwrap();
                    tx.send(WriterMsg::FlushAndAck(ack_tx)).unwrap();
                    tx.send(WriterMsg::Shutdown).unwrap();
                    let error = run(config(), rx).unwrap_err();
                    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
                    assert!(ack_rx.recv().is_err());
                    assert_eq!(std::fs::read(dict_path).unwrap(), bytes);
                    assert_eq!(std::fs::read(log_path).unwrap(), log_before);
                }
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn restart_cannot_ack_cached_dictionary_entries_without_a_write_barrier() {
        for block_titles in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let paths = AppPaths::from_root(dir.path());
            paths.ensure_dirs().unwrap();
            let cfg = WriterConfig {
                paths: paths.clone(),
                scope: InstallScope::User,
                flush_block_records: 256,
                flush_interval_secs: 60,
            };
            let cipher = Cipher::new(&load_or_create_master_key(&paths.key_file, false).unwrap());
            let date = LogDate {
                year: 2026,
                month: 9,
                day: 5,
            };
            let start = utc_midnight_unix(date);
            let segment = Segment {
                app_path: "C:/cached.exe".into(),
                app_basename: "cached.exe".into(),
                title: Some("cached title".into()),
                start_unix: start,
                end_unix: start + 10,
            };
            let mut interrupted = WriterState {
                apps: Dict::open_writer(&paths.apps_dict).unwrap(),
                titles: Dict::open_writer(&paths.titles_dict).unwrap(),
                current: None,
                buffer: Vec::new(),
                buffered_since: None,
            };
            process_segment(segment.clone(), &mut interrupted, &cfg, &cipher).unwrap();
            assert_eq!(interrupted.buffer.len(), 1);
            // Model an abrupt stop before WriterState::flush. Complete entries
            // are readable from cache, but no dictionary barrier has run.
            drop(interrupted);
            assert_eq!(
                Dict::open(&paths.apps_dict).unwrap().get(1),
                Some("C:/cached.exe")
            );
            assert_eq!(
                Dict::open(&paths.titles_dict).unwrap().get(1),
                Some("cached title")
            );
            let log_path = paths.log_file_for_day(date.year, date.month, date.day);
            let log_before = std::fs::read(&log_path).unwrap();
            let blocked = if block_titles {
                &paths.titles_dict
            } else {
                &paths.apps_dict
            };
            let (tx, rx) = std::sync::mpsc::channel();
            let (ack, result) = std::sync::mpsc::channel();
            tx.send(WriterMsg::Segment(segment)).unwrap();
            tx.send(WriterMsg::FlushAndAck(ack)).unwrap();
            tx.send(WriterMsg::Shutdown).unwrap();
            let restarted =
                super::super::dict::with_read_only_startup_sync(blocked, || run(cfg, rx));
            assert!(
                restarted.is_err(),
                "startup must establish both dictionary barriers"
            );
            assert_eq!(restarted.unwrap_err().raw_os_error(), Some(5));
            assert!(
                result.recv().is_err(),
                "startup failure cannot produce a positive ACK"
            );
            assert_eq!(std::fs::read(log_path).unwrap(), log_before);
        }
    }

    #[cfg(windows)]
    #[test]
    fn restart_cannot_ack_before_the_cached_master_key_is_synchronized() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_root(dir.path());
        paths.ensure_dirs().unwrap();
        load_or_create_master_key(&paths.key_file, false).unwrap();
        let wrapped_key = std::fs::read(&paths.key_file).unwrap();
        // Re-create the complete DPAPI blob without syncing, modelling a stop
        // between the first write_all and sync_all in key creation.
        std::fs::remove_file(&paths.key_file).unwrap();
        std::fs::write(&paths.key_file, &wrapped_key).unwrap();
        let cfg = WriterConfig {
            paths: paths.clone(),
            scope: InstallScope::User,
            flush_block_records: 256,
            flush_interval_secs: 60,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let (ack, result) = std::sync::mpsc::channel();
        tx.send(WriterMsg::Gap {
            start_unix: 1_000,
            end_unix: 1_010,
        })
        .unwrap();
        tx.send(WriterMsg::FlushAndAck(ack)).unwrap();
        tx.send(WriterMsg::Shutdown).unwrap();
        let error =
            super::super::dict::with_read_only_startup_sync(&paths.key_file, || run(cfg, rx))
                .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(5));
        assert!(result.recv().is_err());
        assert_eq!(std::fs::read(&paths.key_file).unwrap(), wrapped_key);
        assert!(!paths.apps_dict.exists());
        assert!(!paths.titles_dict.exists());
        assert!(!super::super::crypto::existing_logs(&paths.data_dir).unwrap());
    }

    #[test]
    fn very_long_interval_is_split_lazily() {
        let start = utc_midnight_unix(LogDate {
            year: 2026,
            month: 9,
            day: 5,
        });
        let pieces: Vec<_> = split_by_day(start, start + 86_400 * 1_000_000)
            .take(2)
            .collect();
        assert_eq!(
            pieces,
            vec![(start, start + 86_400), (start + 86_400, start + 172_800)]
        );
    }

    #[test]
    #[cfg(windows)]
    fn dictionary_sync_failure_never_writes_referencing_log_records() {
        for fail_titles in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let paths = AppPaths::from_root(dir.path());
            paths.ensure_dirs().unwrap();
            let key = load_or_create_master_key(&paths.key_file, false).unwrap();
            let cipher = Cipher::new(&key);
            let cfg = WriterConfig {
                paths: paths.clone(),
                scope: InstallScope::User,
                flush_block_records: 256,
                flush_interval_secs: 60,
            };
            let mut state = WriterState {
                apps: Dict::open_writer(&paths.apps_dict).unwrap(),
                titles: Dict::open_writer(&paths.titles_dict).unwrap(),
                current: None,
                buffer: Vec::new(),
                buffered_since: None,
            };
            let date = LogDate {
                year: 2026,
                month: 9,
                day: 5,
            };
            let start = utc_midnight_unix(date);
            process_segment(
                Segment {
                    app_path: "C:/pending.exe".into(),
                    app_basename: "pending.exe".into(),
                    title: Some("pending-title".into()),
                    start_unix: start,
                    end_unix: start + 5,
                },
                &mut state,
                &cfg,
                &cipher,
            )
            .unwrap();
            let path = paths.log_file_for_day(date.year, date.month, date.day);
            let before = std::fs::read(&path).unwrap();
            if fail_titles {
                state.titles.make_pending_sync_handle_read_only();
            } else {
                state.apps.make_pending_sync_handle_read_only();
            }
            // FlushFileBuffers on the read-only handle fails with access denied.
            // This is the same flush called before a positive writer ACK.
            assert!(state.flush().is_err());
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert!(super::super::log::LogReader::new(cipher, date)
                .read_all(&path)
                .unwrap()
                .is_empty());
            assert!(
                state.flush().is_err(),
                "failed dictionary must remain poisoned"
            );
        }
    }

    #[test]
    fn buffered_records_flush_when_producer_goes_quiet() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_root(dir.path());
        paths.ensure_dirs().unwrap();
        let key = load_or_create_master_key(&paths.key_file, false).unwrap();
        let date = LogDate {
            year: 2026,
            month: 9,
            day: 5,
        };
        let start = utc_midnight_unix(date);
        let path = paths.log_file_for_day(date.year, date.month, date.day);
        let cfg = WriterConfig {
            paths,
            scope: InstallScope::User,
            flush_block_records: 256,
            flush_interval_secs: 1,
        };
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || run(cfg, receiver));
        sender
            .send(WriterMsg::Gap {
                start_unix: start,
                end_unix: start + 5,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let reader = super::super::log::LogReader::new(Cipher::new(&key), date);
        let mut observed = false;
        while Instant::now() < deadline {
            if reader
                .read_all(&path)
                .is_ok_and(|records| records.len() == 1)
            {
                observed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        sender.send(WriterMsg::Shutdown).unwrap();
        worker.join().unwrap().unwrap();
        assert!(
            observed,
            "the buffered deadline must still fire without another message"
        );
    }
}
