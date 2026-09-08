//! Cross-layer data integrity checks. These use temporary DPAPI-protected data
//! and query-only CLI commands; no daemon, installer, or autostart is invoked.
#![cfg(windows)]

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

use chrono::NaiveDate;
use tracker::local_time::{Calendar, SystemCalendar};
use tracker::paths::{AppPaths, InstallScope};
use tracker::storage::crypto::{load_or_create_master_key, Cipher};
use tracker::storage::log::LogReader;
use tracker::storage::query::{visit_local_date_range, LocalRecordSlice, QuerySummary};
use tracker::storage::writer::{self, now_unix, unix_to_utc_date, WriterConfig, WriterMsg};
use tracker::storage::Segment;

static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    directory: tempfile::TempDir,
    paths: AppPaths,
    date: NaiveDate,
    start: u64,
    instance: String,
}

impl Fixture {
    fn new(date: NaiveDate) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_root(directory.path());
        // Keep this short fixture well inside its local calendar day.
        let start = SystemCalendar::new().day_start(date).unwrap() + 12 * 3600;
        Self {
            directory,
            paths,
            date,
            start,
            instance: format!(
                "integrity-{}-{}",
                std::process::id(),
                NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed)
            ),
        }
    }

    fn activity(&self, from: u64, to: u64, app: &str) -> WriterMsg {
        WriterMsg::Segment(Segment {
            app_path: format!("C:/{app}.exe"),
            app_basename: format!("{app}.exe"),
            title: Some("integrity-title".to_owned()),
            start_unix: self.start + from,
            end_unix: self.start + to,
        })
    }

    fn gap(&self, from: u64, to: u64) -> WriterMsg {
        WriterMsg::Gap {
            start_unix: self.start + from,
            end_unix: self.start + to,
        }
    }

    fn write(&self, messages: impl IntoIterator<Item = WriterMsg>) {
        let (tx, rx) = mpsc::channel();
        for message in messages {
            tx.send(message).unwrap();
        }
        let (ack_tx, ack_rx) = mpsc::channel();
        tx.send(WriterMsg::FlushAndAck(ack_tx)).unwrap();
        tx.send(WriterMsg::Shutdown).unwrap();
        writer::run(
            WriterConfig {
                paths: self.paths.clone(),
                scope: InstallScope::User,
                // Individual blocks make the corruption fixture precise.
                flush_block_records: 1,
                flush_interval_secs: 60,
            },
            rx,
        )
        .unwrap();
        assert_eq!(ack_rx.recv().unwrap(), Ok(()));
    }

    fn query(&self) -> (Vec<LocalRecordSlice>, QuerySummary) {
        let key = load_or_create_master_key(&self.paths.key_file, false).unwrap();
        let mut slices = Vec::new();
        let summary = visit_local_date_range(
            &self.paths,
            &Cipher::new(&key),
            &SystemCalendar::new(),
            self.date,
            self.date,
            |slice| {
                slices.push(slice);
                Ok(())
            },
        )
        .unwrap();
        (slices, summary)
    }

    fn command(&self) -> Command {
        use std::os::windows::process::CommandExt;

        let mut command = Command::new(env!("CARGO_BIN_EXE_tracker"));
        command
            .env("RUSTTIMENOTER_TEST_ROOT", self.directory.path())
            .env("RUSTTIMENOTER_TEST_INSTANCE", &self.instance)
            .env("RUSTTIMENOTER_SCOPE", "user")
            .stdin(Stdio::null())
            .creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        command
    }

    fn export(&self, format: &str, path: &Path) -> Output {
        let mut command = self.command();
        command.args([
            "export",
            "--format",
            format,
            "--from",
            &self.date.to_string(),
        ]);
        command.args(["--to", &self.date.to_string(), "--out"]);
        command.arg(path);
        checked_output(&mut command)
    }
}

fn checked_output(command: &mut Command) -> Output {
    let output = command.output().expect("run isolated query command");
    assert!(
        output.status.success(),
        "query command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn late_overlapping_gaps_correct_previously_durable_activity_once() {
    let fixture = Fixture::new(NaiveDate::from_ymd_opt(2026, 9, 5).unwrap());
    fixture.write([fixture.activity(0, 120, "editor")]);
    // Restart the writer: the original estimate is already durable when the
    // corrections arrive, including an exact duplicate and a separate gap.
    fixture.write([
        fixture.gap(20, 50),
        fixture.gap(40, 80),
        fixture.gap(20, 50),
        fixture.gap(150, 160),
    ]);

    let (slices, summary) = fixture.query();
    let timeline: Vec<_> = slices
        .iter()
        .map(|slice| {
            (
                slice.start_unix - fixture.start,
                slice.duration_secs,
                slice.is_gap(),
            )
        })
        .collect();
    assert_eq!(
        timeline,
        vec![
            (0, 20, false),
            (20, 60, true),
            (80, 40, false),
            (150, 10, true)
        ]
    );
    assert_eq!(summary.gap_seconds, 70);
    assert_eq!(summary.damaged_files, 0);
    assert_eq!(
        slices
            .iter()
            .filter(|slice| !slice.is_gap())
            .map(|slice| slice.duration_secs)
            .sum::<u32>(),
        60
    );

    // The writer retained all corrections; deduplication belongs to querying.
    let date = unix_to_utc_date(fixture.start);
    let key = load_or_create_master_key(&fixture.paths.key_file, false).unwrap();
    let raw = LogReader::new(Cipher::new(&key), date)
        .read_day(
            &fixture
                .paths
                .log_file_for_day(date.year, date.month, date.day),
        )
        .unwrap();
    assert_eq!(raw.records.len(), 5);
}

#[test]
fn capture_timeout_corrects_activity_accepted_before_consumer_start() {
    use tracker::daemon::accounting::{Accounting, Observation, ObservationKind, WindowIdentity};
    use tracker::daemon::aggregator::{AppKey, MonoTime, TimePoint};

    let fixture = Fixture::new(NaiveDate::from_ymd_opt(2026, 9, 8).unwrap());
    let at = |seconds| TimePoint::new(seconds * 1_000, (fixture.start + seconds) * 1_000);
    let sample = |seconds| Observation {
        at: at(seconds),
        window: WindowIdentity { hwnd: 1, pid: 1 },
        kind: ObservationKind::Sample {
            app: Some(AppKey {
                path: "C:/editor.exe".to_owned(),
                basename: "editor.exe".to_owned(),
                title: None,
            }),
            last_input: Some(MonoTime(0)),
            locked: false,
            suspended: false,
        },
    };
    let messages = |output: tracker::daemon::accounting::Output| {
        output
            .gaps
            .into_iter()
            .map(|gap| WriterMsg::Gap {
                start_unix: gap.start_unix,
                end_unix: gap.end_unix,
            })
            .chain(output.segments.into_iter().map(WriterMsg::Segment))
    };

    // Model a consumer first scheduled at t=3 after samples at t=0 and t=2
    // were accepted. Persist its first checkpoint before shutdown begins.
    // This replay tests the runtime timeout helper and storage/query boundary,
    // without relying on an actual delayed thread or foreground window.
    let mut accounting = Accounting::new(300);
    assert!(accounting.push(sample(0)).is_empty());
    assert!(accounting.push(sample(2)).is_empty());
    fixture.write(messages(accounting.drain_ready(at(4).monotonic)));
    let (before, _) = fixture.query();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].start_unix, fixture.start);
    assert_eq!(before[0].duration_secs, 2);
    assert!(!before[0].is_gap());

    // An admitted producer has not settled. The final matching sample cannot
    // prove that no foreground transition was lost earlier in the session.
    assert!(accounting.push(sample(4)).is_empty());
    let mut correction = accounting.invalidate_session(at(3), at(4));
    correction.extend(accounting.finish(at(4), Some(MonoTime(0))));
    fixture.write(messages(correction));

    let (after, summary) = fixture.query();
    assert_eq!(
        after
            .iter()
            .map(|slice| (
                slice.start_unix - fixture.start,
                slice.duration_secs,
                slice.is_gap(),
            ))
            .collect::<Vec<_>>(),
        vec![(0, 4, true)]
    );
    assert_eq!(summary.gap_seconds, 4);
    assert_eq!(summary.damaged_files, 0);
}

#[test]
fn renewed_input_after_idle_is_reported_as_unknown_without_attributing_an_app() {
    use tracker::daemon::accounting::{Accounting, Observation, ObservationKind, WindowIdentity};
    use tracker::daemon::aggregator::{AppKey, MonoTime, TimePoint};

    let fixture = Fixture::new(NaiveDate::from_ymd_opt(2026, 9, 8).unwrap());
    let at = |seconds| TimePoint::new(seconds * 1_000, (fixture.start + seconds) * 1_000);
    let sample = |seconds: u64, input: u64| Observation {
        at: at(seconds),
        window: WindowIdentity { hwnd: 1, pid: 1 },
        kind: ObservationKind::Sample {
            app: Some(AppKey {
                path: "C:/editor.exe".to_owned(),
                basename: "editor.exe".to_owned(),
                title: None,
            }),
            last_input: Some(MonoTime(input * 1_000)),
            locked: false,
            suspended: false,
        },
    };
    let messages = |output: tracker::daemon::accounting::Output| {
        output
            .gaps
            .into_iter()
            .map(|gap| WriterMsg::Gap {
                start_unix: gap.start_unix,
                end_unix: gap.end_unix,
            })
            .chain(output.segments.into_iter().map(WriterMsg::Segment))
    };

    let mut accounting = Accounting::new(300);
    assert!(accounting.push(sample(0, 0)).is_empty());
    assert!(accounting.push(sample(600, 0)).is_empty());
    fixture.write(messages(accounting.drain_ready(at(602).monotonic)));
    let (before, before_summary) = fixture.query();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].start_unix, fixture.start);
    assert_eq!(before[0].duration_secs, 300);
    assert!(!before[0].is_gap());
    assert_eq!(before_summary.gap_seconds, 0);

    // A later causal input reading disproves part of the already-observed
    // idle interval. Its foreground is unknown even though both samples say A.
    assert!(accounting.push(sample(1_200, 540)).is_empty());
    fixture.write(messages(accounting.drain_ready(at(1_202).monotonic)));
    let (after, summary) = fixture.query();
    assert!(
        after.iter().any(|slice| slice.is_gap()
            && slice.start_unix <= fixture.start + 540
            && slice.start_unix + u64::from(slice.duration_secs) >= fixture.start + 840),
        "input at 540 disproves idle through 840: {after:?}"
    );
    assert_eq!(
        after
            .iter()
            .filter(|slice| !slice.is_gap())
            .map(|slice| (slice.start_unix - fixture.start, slice.duration_secs))
            .collect::<Vec<_>>(),
        vec![(0, 300)]
    );
    assert_eq!(summary.gap_seconds, 540);
    assert_eq!(summary.damaged_files, 0);

    let report = checked_output(fixture.command().args([
        "report",
        "--from",
        &fixture.date.to_string(),
        "--to",
        &fixture.date.to_string(),
    ]));
    let stdout = String::from_utf8_lossy(&report.stdout);
    assert!(stdout.contains("Total in scope: 5m 00s"), "{stdout}");
    assert!(stdout.contains("editor.exe"), "{stdout}");
    assert!(String::from_utf8_lossy(&report.stderr).contains("Incomplete capture"));
}

#[test]
fn damaged_base_and_new_part_remain_queryable_without_losing_original_bytes() {
    let fixture = Fixture::new(NaiveDate::from_ymd_opt(2026, 9, 5).unwrap());
    fixture.write([
        fixture.activity(0, 10, "before"),
        fixture.activity(20, 30, "interrupted"),
    ]);
    let date = unix_to_utc_date(fixture.start);
    let base = fixture
        .paths
        .log_file_for_day(date.year, date.month, date.day);
    let mut damaged = std::fs::read(&base).unwrap();
    damaged.truncate(damaged.len() - 5);
    std::fs::write(&base, &damaged).unwrap();

    fixture.write([fixture.activity(40, 70, "after")]);
    assert_eq!(std::fs::read(&base).unwrap(), damaged);
    let part = base.with_file_name(format!(
        "{}.part-000001.log",
        base.file_stem().unwrap().to_string_lossy()
    ));
    assert!(part.is_file());
    let (slices, summary) = fixture.query();
    assert_eq!(summary.damaged_files, 1);
    assert_eq!(summary.gap_seconds, 0);
    assert!(summary.warning().unwrap().contains("Incomplete capture"));
    assert_eq!(
        slices
            .iter()
            .map(|slice| (slice.start_unix - fixture.start, slice.duration_secs))
            .collect::<Vec<_>>(),
        vec![(0, 10), (40, 30)]
    );

    // Exercise dictionary resolution and the actual CLI against both parts.
    let json_path = fixture.directory.path().join("recovered.json");
    let output = fixture.export("json", &json_path);
    assert!(String::from_utf8_lossy(&output.stderr).contains("1 damaged log file"));
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(json_path).unwrap()).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["app_path"], "C:/before.exe");
    assert_eq!(rows[1]["app_path"], "C:/after.exe");
    assert_eq!(std::fs::read(&base).unwrap(), damaged);
}

#[test]
fn cli_outputs_agree_on_gap_types_warnings_and_usage_totals() {
    let today = SystemCalendar::new().today_at(now_unix()).unwrap();
    let fixture = Fixture::new(today);
    fixture.write([
        fixture.activity(0, 300, "integrity"),
        fixture.gap(60, 180),
        fixture.gap(90, 150),
    ]);
    let json_path = fixture.directory.path().join("records.json");
    let csv_path = fixture.directory.path().join("records.csv");
    fixture.export("json", &json_path);
    fixture.export("csv", &csv_path);
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(json_path).unwrap()).unwrap();
    assert_eq!(rows.len(), 3);
    let json_total = |kind: &str| {
        rows.iter()
            .filter(|row| row["record_type"] == kind)
            .map(|row| row["duration_secs"].as_u64().unwrap())
            .sum::<u64>()
    };
    assert_eq!(json_total("activity"), 180);
    assert_eq!(json_total("gap"), 120);
    let gap = rows.iter().find(|row| row["record_type"] == "gap").unwrap();
    assert_eq!(gap["app_path"], "");
    assert!(gap["title"].is_null());
    assert!(gap["category"].is_null());

    // Fixture values have no commas/quotes; each column can be checked directly.
    let csv = std::fs::read_to_string(csv_path).unwrap();
    let mut lines = csv.lines();
    let header: Vec<_> = lines.next().unwrap().split(',').collect();
    assert_eq!(header.last(), Some(&"record_type"));
    let exported: Vec<Vec<_>> = lines.map(|line| line.split(',').collect()).collect();
    assert_eq!(exported.len(), 3);
    for (csv_row, json_row) in exported.iter().zip(&rows) {
        assert_eq!(csv_row.len(), header.len());
        assert_eq!(
            csv_row[2].parse::<u64>().unwrap(),
            json_row["duration_secs"].as_u64().unwrap()
        );
        assert_eq!(csv_row[7], json_row["record_type"].as_str().unwrap());
    }

    let html_path = fixture.directory.path().join("report.html");
    checked_output(
        fixture
            .command()
            .args(["view", "--days", "1", "--no-open", "--out"])
            .arg(&html_path),
    );
    let html = std::fs::read_to_string(html_path).unwrap();
    assert!(html.contains("Incomplete capture"));
    assert!(html.contains("Total tracked: <b>3m 00s</b>"));
    let status = checked_output(fixture.command().arg("status"));
    let status = String::from_utf8_lossy(&status.stdout);
    assert!(
        status.contains("Today records: 2    total: 3m 00s"),
        "{status}"
    );
    assert!(status.contains("Incomplete capture"));
    let report = checked_output(fixture.command().args([
        "report",
        "--from",
        &today.to_string(),
        "--to",
        &today.to_string(),
    ]));
    assert!(String::from_utf8_lossy(&report.stdout).contains("Total in scope: 3m 00s"));
    assert!(String::from_utf8_lossy(&report.stderr).contains("Incomplete capture"));
}
