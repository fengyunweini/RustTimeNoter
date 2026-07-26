//! System-local calendar operations used by user-facing queries.

use std::fmt::Display;
use std::io;

use chrono::{DateTime, Duration, Local, LocalResult, NaiveDate, SecondsFormat, TimeZone};

/// Calendar behavior needed by report, view, export, status, and tail.
///
/// Unix timestamps remain the source of truth. Implementations only decide how
/// those instants map to user-facing calendar dates and clock times.
pub trait Calendar {
    fn today_at(&self, unix: u64) -> io::Result<NaiveDate> {
        self.date_at(unix)
    }

    fn date_at(&self, unix: u64) -> io::Result<NaiveDate>;

    /// Return the local-day boundary at or after `date`.
    ///
    /// A wholly skipped civil date shares the next representable date's
    /// boundary and therefore denotes a zero-length query interval.
    fn day_start(&self, date: NaiveDate) -> io::Result<u64>;

    /// Format a local wall-clock time as `HH:MM:SS`.
    fn format_time(&self, unix: u64) -> io::Result<String>;

    /// Format an instant as seconds-precision RFC 3339 with a numeric offset.
    fn format_rfc3339(&self, unix: u64) -> io::Result<String>;

    fn format_time_and_rfc3339(&self, unix: u64) -> io::Result<(String, String)> {
        Ok((self.format_time(unix)?, self.format_rfc3339(unix)?))
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemCalendar;

impl SystemCalendar {
    pub const fn new() -> Self {
        Self
    }
}

impl Calendar for SystemCalendar {
    fn date_at(&self, unix: u64) -> io::Result<NaiveDate> {
        date_at_in(&Local, unix)
    }

    fn day_start(&self, date: NaiveDate) -> io::Result<u64> {
        day_start_in(&Local, date)
    }

    fn format_time(&self, unix: u64) -> io::Result<String> {
        format_time_in(&Local, unix)
    }

    fn format_rfc3339(&self, unix: u64) -> io::Result<String> {
        format_rfc3339_in(&Local, unix)
    }

    fn format_time_and_rfc3339(&self, unix: u64) -> io::Result<(String, String)> {
        let datetime = datetime_at(&Local, unix)?;
        Ok((
            datetime.format("%H:%M:%S").to_string(),
            datetime.to_rfc3339_opts(SecondsFormat::Secs, false),
        ))
    }
}

fn date_at_in<Tz: TimeZone>(tz: &Tz, unix: u64) -> io::Result<NaiveDate> {
    Ok(datetime_at(tz, unix)?.date_naive())
}

fn datetime_at<Tz: TimeZone>(tz: &Tz, unix: u64) -> io::Result<DateTime<Tz>> {
    let unix = i64::try_from(unix).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "timestamp is outside chrono's range",
        )
    })?;
    match tz.timestamp_opt(unix, 0) {
        LocalResult::Single(dt) => Ok(dt),
        LocalResult::Ambiguous(a, b) => {
            if a.timestamp() <= b.timestamp() {
                Ok(a)
            } else {
                Ok(b)
            }
        }
        LocalResult::None => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "timestamp is outside the calendar's range",
        )),
    }
}

fn day_start_in<Tz: TimeZone>(tz: &Tz, date: NaiveDate) -> io::Result<u64> {
    let midnight = date.and_hms_opt(0, 0, 0).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "invalid local calendar date")
    })?;

    // Midnight can be skipped by a transition, and an offset jump can skip a
    // whole civil date. Probe at hour-sized intervals, then locate the first
    // representable wall-clock second with a binary search. The three-day
    // horizon covers a skipped date plus a transition at the following
    // midnight without doing 86,400 time-zone lookups per boundary.
    const PROBE_STEP_SECS: i64 = 3_600;
    const MAX_PROBE_SECS: i64 = 3 * 86_400;

    if let Some(unix) = local_timestamp(tz, midnight) {
        return nonnegative_timestamp(unix);
    }

    let mut last_missing = 0i64;
    let mut probe = PROBE_STEP_SECS;
    while probe <= MAX_PROBE_SECS {
        let local = midnight
            .checked_add_signed(Duration::seconds(probe))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "date overflow"))?;
        if local_timestamp(tz, local).is_some() {
            let mut low = last_missing + 1;
            let mut high = probe;
            while low < high {
                let mid = low + (high - low) / 2;
                let candidate = midnight
                    .checked_add_signed(Duration::seconds(mid))
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "date overflow"))?;
                if local_timestamp(tz, candidate).is_some() {
                    high = mid;
                } else {
                    low = mid + 1;
                }
            }

            let first = midnight
                .checked_add_signed(Duration::seconds(low))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "date overflow"))?;
            let unix = local_timestamp(tz, first).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "local day boundary search was inconsistent",
                )
            })?;
            return nonnegative_timestamp(unix);
        }
        last_missing = probe;
        probe += PROBE_STEP_SECS;
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("no representable local instant found after {date}"),
    ))
}

fn local_timestamp<Tz: TimeZone>(tz: &Tz, local: chrono::NaiveDateTime) -> Option<i64> {
    match tz.from_local_datetime(&local) {
        LocalResult::Single(dt) => Some(dt.timestamp()),
        LocalResult::Ambiguous(a, b) => Some(a.timestamp().min(b.timestamp())),
        LocalResult::None => None,
    }
}

fn nonnegative_timestamp(unix: i64) -> io::Result<u64> {
    u64::try_from(unix).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "local day begins before the Unix epoch",
        )
    })
}

fn format_time_in<Tz>(tz: &Tz, unix: u64) -> io::Result<String>
where
    Tz: TimeZone,
    Tz::Offset: Display,
{
    Ok(datetime_at(tz, unix)?.format("%H:%M:%S").to_string())
}

fn format_rfc3339_in<Tz>(tz: &Tz, unix: u64) -> io::Result<String>
where
    Tz: TimeZone,
    Tz::Offset: Display,
{
    Ok(datetime_at(tz, unix)?.to_rfc3339_opts(SecondsFormat::Secs, false))
}

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct TestCalendar(pub chrono_tz::Tz);

#[cfg(test)]
impl Calendar for TestCalendar {
    fn date_at(&self, unix: u64) -> io::Result<NaiveDate> {
        date_at_in(&self.0, unix)
    }

    fn day_start(&self, date: NaiveDate) -> io::Result<u64> {
        day_start_in(&self.0, date)
    }

    fn format_time(&self, unix: u64) -> io::Result<String> {
        format_time_in(&self.0, unix)
    }

    fn format_rfc3339(&self, unix: u64) -> io::Result<String> {
        format_rfc3339_in(&self.0, unix)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveDate, TimeZone, Utc};
    use chrono_tz::Tz;

    use super::{Calendar, TestCalendar};

    fn unix(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> u64 {
        Utc.with_ymd_and_hms(y, m, d, h, min, s)
            .single()
            .unwrap()
            .timestamp() as u64
    }

    #[test]
    fn positive_and_negative_offsets_map_to_local_dates() {
        let shanghai = TestCalendar("Asia/Shanghai".parse::<Tz>().unwrap());
        assert_eq!(
            shanghai.date_at(unix(2026, 7, 25, 17, 0, 0)).unwrap(),
            NaiveDate::from_ymd_opt(2026, 7, 26).unwrap()
        );

        let gmt_minus_8 = TestCalendar("Etc/GMT+8".parse::<Tz>().unwrap());
        assert_eq!(
            gmt_minus_8.date_at(unix(2026, 7, 27, 7, 0, 0)).unwrap(),
            NaiveDate::from_ymd_opt(2026, 7, 26).unwrap()
        );
    }

    #[test]
    fn new_york_days_cover_23_and_25_elapsed_hours() {
        let calendar = TestCalendar("America/New_York".parse::<Tz>().unwrap());

        let spring = NaiveDate::from_ymd_opt(2024, 3, 10).unwrap();
        let spring_end = spring.succ_opt().unwrap();
        assert_eq!(
            calendar.day_start(spring_end).unwrap() - calendar.day_start(spring).unwrap(),
            23 * 3600
        );

        let fall = NaiveDate::from_ymd_opt(2024, 11, 3).unwrap();
        let fall_end = fall.succ_opt().unwrap();
        assert_eq!(
            calendar.day_start(fall_end).unwrap() - calendar.day_start(fall).unwrap(),
            25 * 3600
        );
    }

    #[test]
    fn repeated_wall_time_has_distinct_rfc3339_offsets() {
        let calendar = TestCalendar("America/New_York".parse::<Tz>().unwrap());
        let first = unix(2024, 11, 3, 5, 30, 0);
        let second = unix(2024, 11, 3, 6, 30, 0);

        assert_eq!(calendar.format_time(first).unwrap(), "01:30:00");
        assert_eq!(calendar.format_time(second).unwrap(), "01:30:00");
        assert_eq!(
            calendar.format_rfc3339(first).unwrap(),
            "2024-11-03T01:30:00-04:00"
        );
        assert_eq!(
            calendar.format_rfc3339(second).unwrap(),
            "2024-11-03T01:30:00-05:00"
        );
    }

    #[test]
    fn skipped_apia_date_has_the_next_dates_boundary() {
        let calendar = TestCalendar("Pacific/Apia".parse::<Tz>().unwrap());
        let skipped = NaiveDate::from_ymd_opt(2011, 12, 30).unwrap();
        let next = skipped.succ_opt().unwrap();

        assert_eq!(
            calendar.day_start(skipped).unwrap(),
            calendar.day_start(next).unwrap()
        );
    }
}
