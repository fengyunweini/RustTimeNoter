//! Query UTC-sharded v1 logs using local calendar-day semantics.

use std::io;

use chrono::NaiveDate;

use super::crypto::Cipher;
use super::log::LogReader;
use super::writer::{unix_to_utc_date, utc_midnight_unix};
use crate::local_time::Calendar;
use crate::paths::AppPaths;

pub(crate) const MAX_QUERY_DAYS: u64 = 3_660;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalRecordSlice {
    pub local_date: NaiveDate,
    pub start_unix: u64,
    pub duration_secs: u32,
    pub app_id: u32,
    pub title_id: u32,
    pub flags: u8,
}

#[derive(Debug, Clone, Copy)]
struct DayBoundary {
    date: NaiveDate,
    start_unix: u64,
}

/// Visit records overlapping the inclusive local-date range `from..=to`.
///
/// Local dates are converted to one half-open UTC interval. Each physical UTC
/// shard is read and released before the next shard, and records are clipped
/// and split at the precomputed local-day boundaries.
pub fn visit_local_date_range<C, F>(
    paths: &AppPaths,
    cipher: &Cipher,
    calendar: &C,
    from: NaiveDate,
    to: NaiveDate,
    mut visitor: F,
) -> io::Result<()>
where
    C: Calendar + ?Sized,
    F: FnMut(LocalRecordSlice) -> io::Result<()>,
{
    let days = build_day_boundaries(calendar, from, to)?;
    let range_start = days[0].start_unix;
    let range_end = days[days.len() - 1].start_unix;

    let first_utc_date = unix_to_utc_date(range_start);
    let mut shard_start = utc_midnight_unix(first_utc_date);

    while shard_start < range_end {
        let shard_date = unix_to_utc_date(shard_start);
        let path = paths.log_file_for_day(shard_date.year, shard_date.month, shard_date.day);
        let mut records = LogReader::new(cipher.clone(), shard_date).read_all(&path)?;
        records.sort_by_key(|record| record.start_offset_secs);
        let mut shard_slices = Vec::new();

        for record in records {
            let record_start = shard_start
                .checked_add(record.start_offset_secs as u64)
                .ok_or_else(|| invalid_data("record start timestamp overflow"))?;
            let record_end = record_start
                .checked_add(record.duration_secs as u64)
                .ok_or_else(|| invalid_data("record end timestamp overflow"))?;

            // A v1 record belongs wholly to its UTC shard. Enforcing this keeps
            // shard-by-shard visitation globally chronological.
            let shard_end = shard_start
                .checked_add(86_400)
                .ok_or_else(|| invalid_data("UTC shard timestamp overflow"))?;
            if record.start_offset_secs >= 86_400 || record_end > shard_end {
                return Err(invalid_data("record extends outside its UTC shard"));
            }
            if record_start >= range_end || record_end <= range_start {
                continue;
            }

            let clipped_start = record_start.max(range_start);
            let clipped_end = record_end.min(range_end);
            if clipped_start >= clipped_end {
                continue;
            }

            let mut day_index = days
                .partition_point(|day| day.start_unix <= clipped_start)
                .saturating_sub(1);
            while day_index + 1 < days.len() {
                let day_start = days[day_index].start_unix;
                let day_end = days[day_index + 1].start_unix;
                if day_start >= clipped_end {
                    break;
                }

                let piece_start = clipped_start.max(day_start);
                let piece_end = clipped_end.min(day_end);
                if piece_start < piece_end {
                    shard_slices.push(LocalRecordSlice {
                        local_date: days[day_index].date,
                        start_unix: piece_start,
                        duration_secs: (piece_end - piece_start) as u32,
                        app_id: record.app_id,
                        title_id: record.title_id,
                        flags: record.flags,
                    });
                }
                if clipped_end <= day_end {
                    break;
                }
                day_index += 1;
            }
        }

        // A record can overlap another record while also crossing a local-day
        // boundary. Sorting the generated slices, rather than only their source
        // records, preserves the visitor's chronological contract.
        shard_slices.sort_by_key(|slice| slice.start_unix);
        for slice in shard_slices {
            visitor(slice)?;
        }

        shard_start = shard_start
            .checked_add(86_400)
            .ok_or_else(|| invalid_data("UTC shard timestamp overflow"))?;
    }

    Ok(())
}

/// Collecting convenience wrapper for short ranges and callers that need a Vec.
pub fn read_local_date_range<C: Calendar + ?Sized>(
    paths: &AppPaths,
    cipher: &Cipher,
    calendar: &C,
    from: NaiveDate,
    to: NaiveDate,
) -> io::Result<Vec<LocalRecordSlice>> {
    let mut out = Vec::new();
    visit_local_date_range(paths, cipher, calendar, from, to, |slice| {
        out.push(slice);
        Ok(())
    })?;
    Ok(out)
}

fn build_day_boundaries<C: Calendar + ?Sized>(
    calendar: &C,
    from: NaiveDate,
    to: NaiveDate,
) -> io::Result<Vec<DayBoundary>> {
    if from > to {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "`from` must not be after `to`",
        ));
    }
    let inclusive_days = u64::try_from(to.signed_duration_since(from).num_days() + 1)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid local date range"))?;
    validate_query_day_count(inclusive_days)?;

    let mut days: Vec<DayBoundary> = Vec::with_capacity(inclusive_days as usize + 1);
    let mut date = from;
    loop {
        let start_unix = calendar.day_start(date)?;
        if let Some(previous) = days.last() {
            if start_unix < previous.start_unix {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "local day boundaries are decreasing",
                ));
            }
        }
        days.push(DayBoundary { date, start_unix });

        if date > to {
            break;
        }
        date = date.succ_opt().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "local date range overflow")
        })?;
    }
    Ok(days)
}

pub(crate) fn validate_query_day_count(days: u64) -> io::Result<()> {
    if days == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "local date range must contain at least one day",
        ));
    }
    if days > MAX_QUERY_DAYS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("local date range contains {days} days; maximum is {MAX_QUERY_DAYS}"),
        ));
    }
    Ok(())
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use chrono::{Days, NaiveDate, TimeZone, Utc};
    use chrono_tz::Tz;

    use super::{
        read_local_date_range, validate_query_day_count, visit_local_date_range, MAX_QUERY_DAYS,
    };
    use crate::local_time::{Calendar, TestCalendar};
    use crate::paths::AppPaths;
    use crate::storage::crypto::{Cipher, MasterKey};
    use crate::storage::log::LogWriter;
    use crate::storage::model::Record;
    use crate::storage::writer::{unix_to_utc_date, utc_midnight_unix};

    fn unix(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> u64 {
        Utc.with_ymd_and_hms(y, m, d, h, min, s)
            .single()
            .unwrap()
            .timestamp() as u64
    }

    fn cipher() -> Cipher {
        Cipher::new(&MasterKey([7; 32]))
    }

    fn write_v1_record(
        paths: &AppPaths,
        cipher: &Cipher,
        start_unix: u64,
        duration_secs: u32,
        app_id: u32,
    ) {
        let date = unix_to_utc_date(start_unix);
        let day_start = utc_midnight_unix(date);
        let record = Record {
            start_offset_secs: (start_unix - day_start) as u32,
            duration_secs,
            app_id,
            title_id: app_id + 100,
            flags: 3,
        };
        let path = paths.log_file_for_day(date.year, date.month, date.day);
        let mut writer = LogWriter::open(&path, cipher.clone(), date).unwrap();
        writer.write_block(&[record]).unwrap();
    }

    fn write_v1_interval(
        paths: &AppPaths,
        cipher: &Cipher,
        start_unix: u64,
        end_unix: u64,
        app_id: u32,
    ) {
        let mut start = start_unix;
        while start < end_unix {
            let date = unix_to_utc_date(start);
            let shard_end = utc_midnight_unix(date) + 86_400;
            let end = end_unix.min(shard_end);
            write_v1_record(paths, cipher, start, (end - start) as u32, app_id);
            start = end;
        }
    }

    #[test]
    fn reads_east_and_west_local_dates_from_adjacent_utc_shards() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_root(dir.path());
        let cipher = cipher();

        let shanghai_start = unix(2026, 7, 25, 17, 0, 0);
        write_v1_record(&paths, &cipher, shanghai_start, 60, 1);
        let shanghai = TestCalendar("Asia/Shanghai".parse::<Tz>().unwrap());
        let date = NaiveDate::from_ymd_opt(2026, 7, 26).unwrap();
        let slices = read_local_date_range(&paths, &cipher, &shanghai, date, date).unwrap();
        assert_eq!(slices.len(), 1);
        assert_eq!(slices[0].local_date, date);
        assert_eq!(slices[0].start_unix, shanghai_start);

        let west_start = unix(2026, 7, 27, 7, 0, 0);
        write_v1_record(&paths, &cipher, west_start, 60, 2);
        let gmt_minus_8 = TestCalendar("Etc/GMT+8".parse::<Tz>().unwrap());
        let slices = read_local_date_range(&paths, &cipher, &gmt_minus_8, date, date).unwrap();
        assert_eq!(slices.len(), 1);
        assert_eq!(slices[0].local_date, date);
        assert_eq!(slices[0].start_unix, west_start);
    }

    #[test]
    fn clips_and_splits_at_local_midnight() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_root(dir.path());
        let cipher = cipher();
        let calendar = TestCalendar("Asia/Shanghai".parse::<Tz>().unwrap());

        let start = unix(2026, 7, 26, 15, 59, 50);
        write_v1_record(&paths, &cipher, start, 20, 7);
        let first = NaiveDate::from_ymd_opt(2026, 7, 26).unwrap();
        let second = first.succ_opt().unwrap();

        let slices = read_local_date_range(&paths, &cipher, &calendar, first, second).unwrap();
        assert_eq!(slices.len(), 2);
        assert_eq!(slices[0].local_date, first);
        assert_eq!(slices[0].duration_secs, 10);
        assert_eq!(slices[1].local_date, second);
        assert_eq!(slices[1].duration_secs, 10);
        assert_eq!(slices[1].start_unix, start + 10);

        let first_only = read_local_date_range(&paths, &cipher, &calendar, first, first).unwrap();
        assert_eq!(first_only.len(), 1);
        assert_eq!(first_only[0].duration_secs, 10);
    }

    #[test]
    fn streams_in_chronological_order_and_reads_v1_header_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_root(dir.path());
        let cipher = cipher();
        let calendar = TestCalendar("Asia/Shanghai".parse::<Tz>().unwrap());
        let later = unix(2026, 7, 26, 3, 0, 0);
        let earlier = unix(2026, 7, 26, 2, 0, 0);

        // Write out of order to prove the visitor's chronological contract.
        write_v1_record(&paths, &cipher, later, 30, 2);
        write_v1_record(&paths, &cipher, earlier, 30, 1);

        let shard_date = unix_to_utc_date(earlier);
        let bytes = std::fs::read(paths.log_file_for_day(
            shard_date.year,
            shard_date.month,
            shard_date.day,
        ))
        .unwrap();
        assert_eq!(&bytes[0..4], b"RTNL");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 1);

        let date = NaiveDate::from_ymd_opt(2026, 7, 26).unwrap();
        let mut starts = Vec::new();
        visit_local_date_range(&paths, &cipher, &calendar, date, date, |slice| {
            starts.push(slice.start_unix);
            Ok(())
        })
        .unwrap();
        assert_eq!(starts, vec![earlier, later]);
    }

    #[test]
    fn overlapping_records_remain_chronological_after_local_day_splitting() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_root(dir.path());
        let cipher = cipher();
        let calendar = TestCalendar("Asia/Shanghai".parse::<Tz>().unwrap());
        let crossing = unix(2026, 7, 26, 15, 59, 50);
        let overlap = unix(2026, 7, 26, 15, 59, 55);

        write_v1_record(&paths, &cipher, crossing, 20, 1);
        write_v1_record(&paths, &cipher, overlap, 1, 2);

        let first = NaiveDate::from_ymd_opt(2026, 7, 26).unwrap();
        let second = first.succ_opt().unwrap();
        let slices = read_local_date_range(&paths, &cipher, &calendar, first, second).unwrap();
        let starts: Vec<_> = slices.iter().map(|slice| slice.start_unix).collect();

        assert_eq!(starts, vec![crossing, overlap, crossing + 10]);
    }

    #[test]
    fn skipped_apia_date_is_an_empty_query_interval() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_root(dir.path());
        let cipher = cipher();
        let calendar = TestCalendar("Pacific/Apia".parse::<Tz>().unwrap());
        let skipped = NaiveDate::from_ymd_opt(2011, 12, 30).unwrap();
        let next = skipped.succ_opt().unwrap();
        let next_start = calendar.day_start(next).unwrap();
        write_v1_record(&paths, &cipher, next_start, 60, 1);

        let skipped_only =
            read_local_date_range(&paths, &cipher, &calendar, skipped, skipped).unwrap();
        assert!(skipped_only.is_empty());

        let including_next =
            read_local_date_range(&paths, &cipher, &calendar, skipped, next).unwrap();
        assert_eq!(including_next.len(), 1);
        assert_eq!(including_next[0].local_date, next);
        assert_eq!(including_next[0].start_unix, next_start);
    }

    #[test]
    fn new_york_queries_clip_to_23_and_25_hour_local_days() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_root(dir.path());
        let cipher = cipher();
        let calendar = TestCalendar("America/New_York".parse::<Tz>().unwrap());

        for (date, expected_start, expected_end, app_id) in [
            (
                NaiveDate::from_ymd_opt(2024, 3, 10).unwrap(),
                unix(2024, 3, 10, 5, 0, 0),
                unix(2024, 3, 11, 4, 0, 0),
                1,
            ),
            (
                NaiveDate::from_ymd_opt(2024, 11, 3).unwrap(),
                unix(2024, 11, 3, 4, 0, 0),
                unix(2024, 11, 4, 5, 0, 0),
                2,
            ),
        ] {
            write_v1_interval(
                &paths,
                &cipher,
                expected_start - 3_600,
                expected_end + 3_600,
                app_id,
            );

            let slices = read_local_date_range(&paths, &cipher, &calendar, date, date).unwrap();
            let total: u64 = slices.iter().map(|slice| slice.duration_secs as u64).sum();
            assert_eq!(slices.first().unwrap().start_unix, expected_start);
            assert_eq!(
                slices.last().unwrap().start_unix + slices.last().unwrap().duration_secs as u64,
                expected_end
            );
            assert_eq!(total, expected_end - expected_start);
        }
    }

    #[test]
    fn rejects_ranges_longer_than_ten_year_budget() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_root(dir.path());
        let cipher = cipher();
        let calendar = TestCalendar("UTC".parse::<Tz>().unwrap());
        let from = NaiveDate::from_ymd_opt(2020, 1, 1).unwrap();
        let to = from.checked_add_days(Days::new(3_660)).unwrap();

        let err = read_local_date_range(&paths, &cipher, &calendar, from, to).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("maximum is 3660"));
    }

    #[test]
    fn query_day_limit_is_inclusive() {
        validate_query_day_count(MAX_QUERY_DAYS).unwrap();
        let err = validate_query_day_count(MAX_QUERY_DAYS + 1).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn rejects_reversed_range() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_root(dir.path());
        let cipher = cipher();
        let calendar = TestCalendar("Asia/Shanghai".parse::<Tz>().unwrap());
        let from = NaiveDate::from_ymd_opt(2026, 7, 27).unwrap();
        let to = NaiveDate::from_ymd_opt(2026, 7, 26).unwrap();

        let err = read_local_date_range(&paths, &cipher, &calendar, from, to).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}
