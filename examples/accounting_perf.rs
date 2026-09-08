//! Reproducible pure accounting benchmark; never launches the tracker daemon.
//!
//! Run `cargo run --release --example accounting_perf -- 100000` on Windows.
//! Five deterministic traces cover 30-second samples, ordinary two-second
//! foreground switches, four title changes per second, and 400 events/second
//! with ordered or 160ms-reordered delivery. The last two are deliberate stress
//! cases, not estimates of ordinary desktop activity. Their two-second pending
//! tails stay below 865 observations, below the unchanged 1024-event capacity.
//! Trace construction is outside timing; cloning owned producer payloads,
//! accounting, consuming outputs and releasing their memory are inside timing.
//! Allocation counts and complete output digests use a separate untimed pass.
//! Window identities stay stable for each application; the title trace changes
//! only the title of one editor window. Its payloads and result fields remain
//! comparable to the 8dc6c54 trace, which already used that same application.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use tracker::daemon::accounting::{
    Accounting, Observation, ObservationKind, Output, WindowIdentity,
};
use tracker::daemon::aggregator::{AppKey, MonoTime, TimePoint};

struct MeasuredAllocator;
static MEASURE: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE_BYTES: AtomicU64 = AtomicU64::new(0);
static PEAK_BYTES: AtomicU64 = AtomicU64::new(0);

fn allocated(bytes: usize) {
    if MEASURE.load(Ordering::Relaxed) {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
        let live = LIVE_BYTES.fetch_add(bytes as u64, Ordering::Relaxed) + bytes as u64;
        PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
    }
}

fn deallocated(bytes: usize) {
    if MEASURE.load(Ordering::Relaxed) {
        LIVE_BYTES.fetch_sub(bytes as u64, Ordering::Relaxed);
    }
}

// All allocations are delegated to the system allocator with their original
// layout. The counters are enabled only while one self-contained run is alive.
unsafe impl GlobalAlloc for MeasuredAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
        }
        pointer
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        deallocated(layout.size());
        unsafe { System.dealloc(pointer, layout) };
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let replacement = unsafe { System.realloc(pointer, layout, new_size) };
        if !replacement.is_null() {
            deallocated(layout.size());
            allocated(new_size);
        }
        replacement
    }
}

#[global_allocator]
static ALLOCATOR: MeasuredAllocator = MeasuredAllocator;

const WALL_MS: u64 = 1_700_000_000_000;

fn app(index: usize, titles: bool) -> AppKey {
    let name = if titles || index.is_multiple_of(2) {
        "editor"
    } else {
        "browser"
    };
    AppKey {
        path: format!("C:/Program Files/Accounting benchmark/{name}/{name}.exe"),
        basename: format!("{name}.exe"),
        title: titles.then(|| {
            format!(
                "Document {:02} - representative project notes and source code - ",
                index % 16
            )
            .repeat(2)
        }),
    }
}

fn window(index: usize, titles: bool) -> WindowIdentity {
    let id = if titles || index.is_multiple_of(2) {
        1
    } else {
        2
    };
    WindowIdentity {
        hwnd: id as isize,
        pid: id,
    }
}

fn trace(name: &str, events: usize) -> Vec<(Observation, MonoTime)> {
    let mut trace = Vec::with_capacity(events);
    for index in 0..events {
        let (millis, sample_every, titles) = match name {
            "samples_30s" => (index as u64 * 30_000, 1, false),
            "foreground_every_2s" => (index as u64 * 2_000, 15, false),
            "titles_4_per_second" => (index as u64 * 250, 120, true),
            _ => (index as u64 * 5 / 2, 400, false),
        };
        let is_sample = index % sample_every == 0;
        // Samples validate the previous foreground identity, not an invented
        // transition; this keeps correctness assertions sensitive to real gaps.
        let identity = if name == "samples_30s" {
            0
        } else if is_sample {
            index.saturating_sub(1)
        } else {
            index
        };
        let app = Some(app(identity, titles));
        let last_input = Some(MonoTime(millis));
        let kind = if is_sample {
            ObservationKind::Sample {
                app,
                last_input,
                locked: false,
                suspended: false,
            }
        } else if titles {
            ObservationKind::Title { app, last_input }
        } else {
            ObservationKind::Foreground { app, last_input }
        };
        trace.push((
            Observation {
                at: TimePoint::new(millis, WALL_MS + millis),
                window: window(identity, titles),
                kind,
            },
            MonoTime(millis),
        ));
    }
    if name == "burst_400_per_second_reordered" {
        for batch in trace.chunks_mut(64) {
            let delivered = batch.last().expect("nonempty batch").1;
            batch.reverse();
            for (_, delivery) in batch {
                *delivery = delivered;
            }
        }
    }
    trace
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ResultDigest {
    segments: u64,
    gaps: u64,
    duration_secs: u64,
    checkpoints: u64,
    last_end: u64,
    overlap: bool,
    digest: u64,
}

impl ResultDigest {
    fn bytes(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.digest = (self.digest ^ u64::from(*byte)).wrapping_mul(1_099_511_628_211);
        }
    }

    fn consume(&mut self, output: Output, full_digest: bool) {
        self.checkpoints += u64::from(output.checkpoint);
        self.gaps += output.gaps.len() as u64;
        for segment in &output.segments {
            self.segments += 1;
            self.duration_secs += segment.duration();
            self.overlap |= self.last_end > segment.start_unix;
            self.last_end = segment.end_unix;
            if full_digest {
                self.bytes(&segment.start_unix.to_le_bytes());
                self.bytes(&segment.end_unix.to_le_bytes());
                self.bytes(segment.app_path.as_bytes());
                self.bytes(segment.app_basename.as_bytes());
                self.bytes(segment.title.as_deref().unwrap_or("").as_bytes());
            }
        }
        for gap in &output.gaps {
            if full_digest {
                self.bytes(&gap.start_unix.to_le_bytes());
                self.bytes(&gap.end_unix.to_le_bytes());
            }
        }
        black_box(output);
    }
}

fn run(trace: &[(Observation, MonoTime)], full_digest: bool) -> ResultDigest {
    let mut accounting = Accounting::new(300);
    let mut result = ResultDigest::default();
    for (observation, delivered) in trace {
        result.consume(accounting.push(black_box(observation.clone())), full_digest);
        result.consume(accounting.drain_ready(black_box(*delivered)), full_digest);
    }
    let end = trace
        .iter()
        .map(|(event, _)| event.at.monotonic.0)
        .max()
        .unwrap_or(0)
        + 1_000;
    result.consume(
        accounting.finish(TimePoint::new(end, WALL_MS + end), Some(MonoTime(end))),
        full_digest,
    );
    assert_eq!(
        result.gaps, 0,
        "workload must not overflow or invent missing data"
    );
    assert!(
        !result.overlap,
        "workload must not duplicate accounted time"
    );
    assert_eq!(
        result.duration_secs,
        end / 1_000,
        "all elapsed time must be accounted exactly once"
    );
    black_box(result)
}

fn main() {
    let events: usize = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(100_000);
    assert!(events >= 400);
    for name in [
        "samples_30s",
        "foreground_every_2s",
        "titles_4_per_second",
        "burst_400_per_second_ordered",
        "burst_400_per_second_reordered",
    ] {
        let trace = trace(name, events);
        black_box(run(&trace, false)); // warm the code and allocator
        let mut trials = Vec::with_capacity(7);
        for _ in 0..7 {
            let started = Instant::now();
            black_box(run(&trace, false));
            trials.push(started.elapsed().as_nanos() as u64);
        }
        let mut sorted = trials.clone();
        sorted.sort_unstable();
        ALLOCATIONS.store(0, Ordering::Relaxed);
        ALLOCATED_BYTES.store(0, Ordering::Relaxed);
        LIVE_BYTES.store(0, Ordering::Relaxed);
        PEAK_BYTES.store(0, Ordering::Relaxed);
        MEASURE.store(true, Ordering::Relaxed);
        let verified = run(&trace, true);
        MEASURE.store(false, Ordering::Relaxed);
        assert_eq!(
            LIVE_BYTES.load(Ordering::Relaxed),
            0,
            "measured run leaked live allocations"
        );
        println!(
            "{}",
            serde_json::json!({
                "workload": name, "events": events, "trials_ns": trials,
                "median_ns": sorted[3], "ns_per_observation": sorted[3] as f64 / events as f64,
                "allocations": ALLOCATIONS.load(Ordering::Relaxed),
                "allocated_bytes": ALLOCATED_BYTES.load(Ordering::Relaxed),
                "peak_extra_live_bytes": PEAK_BYTES.load(Ordering::Relaxed),
                "accounting_size_bytes": std::mem::size_of::<Accounting>(),
                "observation_size_bytes": std::mem::size_of::<Observation>(),
                "max_pending_bound": if name.ends_with("reordered") { 865 } else if name.starts_with("burst") { 802 } else if name.starts_with("titles") { 10 } else { 3 },
                "segments": verified.segments, "gaps": verified.gaps, "duration_secs": verified.duration_secs,
                "checkpoints": verified.checkpoints, "overlap": verified.overlap, "output_digest": verified.digest.to_string(),
            })
        );
    }
}
