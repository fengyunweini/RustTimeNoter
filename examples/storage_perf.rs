//! Fixed isolated storage replay. Usage: storage_perf [records=4000] [rounds=3].
//! Run the identical example against each source snapshot in release mode.
use std::alloc::{GlobalAlloc, Layout, System};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::time::{Duration, Instant};
use tracker::paths::{AppPaths, InstallScope};
use tracker::storage::crypto::{load_or_create_master_key, Cipher};
use tracker::storage::log::{LogDate, LogReader};
use tracker::storage::writer::{self, utc_midnight_unix, WriterConfig, WriterMsg};
use tracker::storage::Segment;

struct CountAlloc;
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE_BYTES: AtomicU64 = AtomicU64::new(0);
#[global_allocator]
static ALLOCATOR: CountAlloc = CountAlloc;
unsafe impl GlobalAlloc for CountAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            LIVE_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
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
            LIVE_BYTES.fetch_add(size as u64, Ordering::Relaxed);
            LIVE_BYTES.fetch_sub(layout.size() as u64, Ordering::Relaxed);
        }
        new_pointer
    }
}

fn acknowledge(sender: &SyncSender<WriterMsg>) -> io::Result<()> {
    let (tx, rx) = mpsc::channel();
    sender
        .send(WriterMsg::FlushAndAck(tx))
        .map_err(io::Error::other)?;
    rx.recv_timeout(Duration::from_secs(30))
        .map_err(io::Error::other)?
        .map_err(io::Error::other)
}

#[cfg(windows)]
fn process_cycles() -> io::Result<u64> {
    use windows_sys::Win32::Foundation::{BOOL, HANDLE};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    #[link(name = "kernel32")]
    extern "system" {
        fn QueryProcessCycleTime(process: HANDLE, cycles: *mut u64) -> BOOL;
    }
    let mut cycles = 0;
    if unsafe { QueryProcessCycleTime(GetCurrentProcess(), &mut cycles) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cycles)
}
#[cfg(not(windows))]
fn process_cycles() -> io::Result<u64> {
    Ok(0)
}

fn file_bytes(path: &std::path::Path) -> io::Result<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        total += if entry.file_type()?.is_dir() {
            file_bytes(&entry.path())?
        } else {
            entry.metadata()?.len()
        };
    }
    Ok(total)
}

fn replay(scenario: &str, count: usize, round: usize) -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let paths = AppPaths::from_root(directory.path());
    let date = LogDate {
        year: 2026,
        month: 9,
        day: 5,
    };
    let start = utc_midnight_unix(date) + 3600;
    let messages: Vec<_> = if scenario == "idle" {
        Vec::new()
    } else {
        (0..count)
            .map(|index| {
                let title_index = if scenario == "unique_titles" {
                    index
                } else {
                    index % 128
                };
                WriterMsg::Segment(Segment {
                    app_path: format!("C:/Bench/app-{:02}.exe", index % 16),
                    app_basename: format!("app-{:02}.exe", index % 16),
                    title: Some(format!(
                        "Title {title_index:08} - {}",
                        "fixed-window-context-".repeat(6)
                    )),
                    start_unix: start + index as u64,
                    end_unix: start + index as u64 + 1,
                })
            })
            .collect()
    };
    let (sender, receiver) = mpsc::sync_channel(256);
    let cfg = WriterConfig {
        paths: paths.clone(),
        scope: InstallScope::User,
        flush_block_records: 256,
        flush_interval_secs: 1,
    };
    let worker = std::thread::spawn(move || writer::run(cfg, receiver));
    acknowledge(&sender)?;
    let allocations_before = ALLOCATIONS.load(Ordering::Relaxed);
    let bytes_before = ALLOCATED_BYTES.load(Ordering::Relaxed);
    let cycles_before = process_cycles()?;
    let clock = Instant::now();
    if scenario == "idle" {
        std::thread::sleep(Duration::from_millis(2200));
    } else {
        for (index, message) in messages.into_iter().enumerate() {
            sender.send(message).map_err(io::Error::other)?;
            if (index + 1) % 128 == 0 {
                acknowledge(&sender)?;
            }
        }
    }
    acknowledge(&sender)?;
    let elapsed_ms = clock.elapsed().as_secs_f64() * 1000.0;
    let cycles = process_cycles()?.saturating_sub(cycles_before);
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - allocations_before;
    let allocated_bytes = ALLOCATED_BYTES.load(Ordering::Relaxed) - bytes_before;
    let live_heap_bytes = LIVE_BYTES.load(Ordering::Relaxed);
    sender.send(WriterMsg::Shutdown).map_err(io::Error::other)?;
    worker
        .join()
        .map_err(|_| io::Error::other("writer panicked"))??;
    let key = load_or_create_master_key(&paths.key_file, false)?;
    let records = LogReader::new(Cipher::new(&key), date)
        .read_day(&paths.log_file_for_day(date.year, date.month, date.day))?
        .records;
    let expected_count = if scenario == "idle" { 0 } else { count };
    assert_eq!(records.len(), expected_count);
    assert_eq!(
        records
            .iter()
            .map(|record| u64::from(record.duration_secs))
            .sum::<u64>(),
        expected_count as u64
    );
    println!(
        "{}",
        serde_json::json!({
            "scenario": scenario, "round": round, "records": records.len(),
            "elapsed_ms": elapsed_ms, "process_cycles": cycles,
            "allocations": allocations, "allocated_bytes": allocated_bytes,
            "live_heap_bytes_at_ack": live_heap_bytes, "file_bytes": file_bytes(directory.path())?
        })
    );
    Ok(())
}

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let count = args
        .next()
        .map(|value| value.parse::<usize>().expect("records must be numeric"))
        .unwrap_or(4000);
    let rounds = args
        .next()
        .map(|value| value.parse::<usize>().expect("rounds must be numeric"))
        .unwrap_or(3);
    assert!(
        count <= 80_000,
        "benchmark data must stay within one UTC day"
    );
    for round in 1..=rounds {
        for scenario in ["idle", "repeated_titles", "unique_titles"] {
            replay(scenario, count, round)?;
        }
    }
    Ok(())
}
