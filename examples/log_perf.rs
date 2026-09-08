//! Isolated hot-cache log read/query/reopen/startup benchmark. No desktop hooks.
//! Usage: log_perf [blocks=2048] [rounds=9]; 32 one-second records per block.
//! Fixture generation is outside measurement; all fields feed an output digest.
use chrono::NaiveDate;
use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tracker::local_time::Calendar;
use tracker::paths::{AppPaths, InstallScope};
use tracker::storage::crypto::{load_or_create_master_key, Cipher};
use tracker::storage::log::{LogDate, LogReader, LogWriter};
use tracker::storage::model::{Record, Segment};
use tracker::storage::query::visit_local_date_range;
use tracker::storage::writer::{self, utc_midnight_unix, WriterConfig, WriterMsg};

struct CountAlloc;
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static PEAK_BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE_BYTES: AtomicU64 = AtomicU64::new(0);
#[global_allocator]
static ALLOCATOR: CountAlloc = CountAlloc;
unsafe impl GlobalAlloc for CountAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            let live = LIVE_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed)
                + layout.size() as u64;
            PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let new_pointer = unsafe { System.realloc(pointer, layout, size) };
        if !new_pointer.is_null() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(size as u64, Ordering::Relaxed);
            let live = LIVE_BYTES.fetch_add(size as u64, Ordering::Relaxed) + size as u64;
            let live = live - layout.size() as u64;
            LIVE_BYTES.fetch_sub(layout.size() as u64, Ordering::Relaxed);
            PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
        }
        new_pointer
    }
}

struct UtcCalendar;
impl Calendar for UtcCalendar {
    fn date_at(&self, unix: u64) -> io::Result<NaiveDate> {
        Ok(chrono::DateTime::from_timestamp(unix as i64, 0)
            .unwrap()
            .date_naive())
    }
    fn day_start(&self, date: NaiveDate) -> io::Result<u64> {
        Ok(date.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp() as u64)
    }
    fn format_time(&self, _: u64) -> io::Result<String> {
        unreachable!()
    }
    fn format_rfc3339(&self, _: u64) -> io::Result<String> {
        unreachable!()
    }
}
fn hash(hash: &mut u64, fields: &[u64]) {
    for field in fields {
        *hash = (*hash ^ field).wrapping_mul(1_099_511_628_211);
    }
}
fn measure<F>(scenario: &str, round: usize, mut action: F) -> io::Result<()>
where
    F: FnMut() -> io::Result<(usize, u64)>,
{
    let live = LIVE_BYTES.load(Ordering::Relaxed);
    PEAK_BYTES.store(live, Ordering::Relaxed);
    let allocs = ALLOCATIONS.load(Ordering::Relaxed);
    let bytes = ALLOCATED_BYTES.load(Ordering::Relaxed);
    let start = Instant::now();
    let (count, digest) = action()?;
    let elapsed_us = start.elapsed().as_secs_f64() * 1_000_000.;
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - allocs;
    let allocated_bytes = ALLOCATED_BYTES.load(Ordering::Relaxed) - bytes;
    let peak_heap_growth = PEAK_BYTES.load(Ordering::Relaxed).saturating_sub(live);
    println!(
        "{}",
        serde_json::json!({"scenario":scenario,"round":round,
        "elapsed_us":elapsed_us,"allocations":allocations,"allocated_bytes":allocated_bytes,
        "peak_heap_growth":peak_heap_growth,"records":count,"digest":digest})
    );
    Ok(())
}
fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let blocks: u32 = args.next().map(|v| v.parse().unwrap()).unwrap_or(2048);
    let rounds: usize = args.next().map(|v| v.parse().unwrap()).unwrap_or(9);
    assert!(blocks > 0 && blocks <= 2700);
    let dir = tempfile::tempdir()?;
    let paths = AppPaths::from_root(dir.path());
    let date = LogDate {
        year: 2026,
        month: 9,
        day: 5,
    };
    let day_start = utc_midnight_unix(date);
    let cfg = || WriterConfig {
        paths: paths.clone(),
        scope: InstallScope::User,
        flush_block_records: 256,
        flush_interval_secs: 30,
    };
    // Establish ordinary v1 key and dictionaries through the real writer.
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(WriterMsg::Segment(Segment {
        app_path: "C:/bench.exe".into(),
        app_basename: "bench.exe".into(),
        title: Some("benchmark".into()),
        start_unix: day_start,
        end_unix: day_start + 1,
    }))
    .unwrap();
    tx.send(WriterMsg::Shutdown).unwrap();
    writer::run(cfg(), rx)?;
    let key = load_or_create_master_key(&paths.key_file, false)?;
    let cipher = Cipher::new(&key);
    let path = paths.log_file_for_day(date.year, date.month, date.day);
    // The fixture uses documented v1 framing, with one final sync; writing is
    // not timed. This avoids thousands of fixture-only disk barriers.
    let mut file = std::io::BufWriter::new(std::fs::File::create(&path)?);
    file.write_all(b"RTNL")?;
    file.write_all(&1u32.to_le_bytes())?;
    file.write_all(&date.pack().to_le_bytes())?;
    file.write_all(&0u32.to_le_bytes())?;
    for index in 0..blocks {
        let mut plaintext = Vec::new();
        for offset in 0..32 {
            Record {
                start_offset_secs: index * 32 + offset,
                duration_secs: 1,
                app_id: 1,
                title_id: 1,
                flags: 0,
            }
            .write_to(&mut plaintext);
        }
        let mut aad = b"RTNL".to_vec();
        aad.extend_from_slice(&date.pack().to_le_bytes());
        aad.extend_from_slice(&index.to_le_bytes());
        file.write_all(&cipher.seal_block(&plaintext, &aad))?;
    }
    file.flush()?;
    file.get_ref().sync_all()?;
    drop(file);
    let expected = (blocks * 32) as usize;
    let reader = LogReader::new(cipher.clone(), date);
    assert_eq!(reader.read_all(&path)?.len(), expected); // hot-cache warmup
    let local_date = NaiveDate::from_ymd_opt(2026, 9, 5).unwrap();
    for round in 1..=rounds {
        measure("read", round, || {
            let records = reader.read_all(&path)?;
            assert_eq!(records.len(), expected);
            let mut digest = 0;
            for r in &records {
                hash(
                    &mut digest,
                    &[
                        r.start_offset_secs as u64,
                        r.duration_secs as u64,
                        r.app_id as u64,
                        r.title_id as u64,
                        r.flags as u64,
                    ],
                );
            }
            Ok((records.len(), digest))
        })?;
        measure("query", round, || {
            let mut count = 0;
            let mut digest = 0;
            let quality = visit_local_date_range(
                &paths,
                &cipher,
                &UtcCalendar,
                local_date,
                local_date,
                |r| {
                    count += 1;
                    hash(
                        &mut digest,
                        &[
                            r.start_unix - day_start,
                            r.duration_secs as u64,
                            r.app_id as u64,
                            r.title_id as u64,
                            r.flags as u64,
                        ],
                    );
                    Ok(())
                },
            )?;
            assert_eq!(count, expected);
            assert_eq!(quality.gap_seconds, 0);
            assert_eq!(quality.damaged_files, 0);
            Ok((count, digest))
        })?;
        measure("reopen", round, || {
            let reopened = LogWriter::open_daily(&path, cipher.clone(), date)?;
            drop(reopened);
            Ok((expected, 0))
        })?;
        measure("writer_startup", round, || {
            let (tx, rx) = std::sync::mpsc::channel();
            let (ack, received) = std::sync::mpsc::channel();
            tx.send(WriterMsg::FlushAndAck(ack)).unwrap();
            tx.send(WriterMsg::Shutdown).unwrap();
            writer::run(cfg(), rx)?;
            received.recv().unwrap().map_err(io::Error::other)?;
            Ok((expected, 0))
        })?;
    }
    Ok(())
}
