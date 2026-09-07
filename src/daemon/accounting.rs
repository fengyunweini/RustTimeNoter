//! Bounded event-time accounting with an adjustable uncommitted tail.
//!
//! Samples are part of the same timeline as transitions. A consumer must never
//! checkpoint to its own current time: only an ordered Sample commits progress.
//! Events older than committed ordering information become explicit gaps.

use std::collections::VecDeque;
use std::time::Duration;

use super::aggregator::{Aggregator, AppKey, MonoTime, TimePoint};
use crate::storage::{model::Gap, Segment};

pub const DEFAULT_PENDING_CAPACITY: usize = 1_024;
pub const DEFAULT_TAIL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct Observation {
    pub at: TimePoint,
    pub kind: ObservationKind,
}

#[derive(Debug, Clone)]
pub enum ObservationKind {
    Foreground {
        app: Option<AppKey>,
        last_input: Option<MonoTime>,
    },
    Title {
        app: Option<AppKey>,
        last_input: Option<MonoTime>,
    },
    Sample {
        app: Option<AppKey>,
        last_input: Option<MonoTime>,
        locked: bool,
        suspended: bool,
    },
    Lock(bool),
    Suspend(bool),
}

#[derive(Debug, Default)]
pub struct Output {
    pub segments: Vec<Segment>,
    pub gaps: Vec<Gap>,
    /// A durability barrier is needed after samples, corrections and shutdown.
    pub checkpoint: bool,
}

impl Output {
    pub fn extend(&mut self, other: Self) {
        // Most observations produce one owned batch. Reuse that allocation
        // while passing it through accounting instead of allocating a copy.
        self.extend_segments(other.segments);
        if self.gaps.is_empty() {
            self.gaps = other.gaps;
        } else {
            self.gaps.extend(other.gaps);
        }
        self.checkpoint |= other.checkpoint;
    }

    fn extend_segments(&mut self, segments: Vec<Segment>) {
        if self.segments.is_empty() {
            self.segments = segments;
        } else {
            self.segments.extend(segments);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty() && self.gaps.is_empty() && !self.checkpoint
    }
}

#[derive(Debug)]
pub struct Accounting {
    aggregator: Aggregator,
    afk_millis: u64,
    pending: VecDeque<Observation>,
    capacity: usize,
    tail_millis: u64,
    processed: Option<TimePoint>,
    // One session mapping keeps invalidations aligned with previously written
    // timestamps even if the system clock is corrected during the session.
    wall_anchor: Option<TimePoint>,
    last_confirmed: Option<TimePoint>,
    waiting_sample: bool,
    gap_start: Option<TimePoint>,
    idle_observed: Option<TimePoint>,
    locked: bool,
    suspended: bool,
    finished: bool,
}

impl Accounting {
    pub fn new(afk_threshold_secs: u64) -> Self {
        Self::with_limits(afk_threshold_secs, DEFAULT_TAIL, DEFAULT_PENDING_CAPACITY)
    }

    pub fn with_limits(afk_threshold_secs: u64, tail: Duration, capacity: usize) -> Self {
        Self {
            aggregator: Aggregator::new(afk_threshold_secs),
            afk_millis: afk_threshold_secs.max(1).saturating_mul(1_000),
            pending: VecDeque::new(),
            capacity: capacity.clamp(1, DEFAULT_PENDING_CAPACITY),
            tail_millis: u64::try_from(tail.as_millis()).unwrap_or(u64::MAX),
            processed: None,
            wall_anchor: None,
            last_confirmed: None,
            waiting_sample: true,
            gap_start: None,
            idle_observed: None,
            locked: false,
            suspended: false,
            finished: false,
        }
    }

    pub fn push(&mut self, observation: Observation) -> Output {
        if self.finished {
            return Output::default();
        }
        if let Some(processed) = self.processed {
            if observation.at.monotonic < processed.monotonic {
                return self.invalidate_interval(observation.at, processed);
            }
        }
        if self.pending.len() >= self.capacity {
            let first = self
                .pending
                .front()
                .map(|item| item.at)
                .unwrap_or(observation.at);
            let from = self.processed.unwrap_or(first);
            let from = if observation.at.monotonic < from.monotonic {
                observation.at
            } else {
                from
            };
            let last = self
                .pending
                .back()
                .map(|item| item.at)
                .unwrap_or(observation.at);
            let to = if last.monotonic > observation.at.monotonic {
                last
            } else {
                observation.at
            };
            return self.invalidate(from, to);
        }
        // Delivery is normally ordered. A ring keeps this path O(1) at both
        // ends; only genuinely out-of-order arrivals need insertion work.
        if self
            .pending
            .back()
            .is_none_or(|item| item.at.monotonic <= observation.at.monotonic)
        {
            self.pending.push_back(observation);
        } else {
            let index = self
                .pending
                .partition_point(|item| item.at.monotonic <= observation.at.monotonic);
            self.pending.insert(index, observation);
        }
        Output::default()
    }

    pub fn drain_ready(&mut self, now: MonoTime) -> Output {
        if self.finished {
            return Output::default();
        }
        let Some(cutoff) = now.0.checked_sub(self.tail_millis) else {
            return Output::default();
        };
        self.drain_through(MonoTime(cutoff))
    }

    pub fn next_wait(&self, now: MonoTime) -> Option<Duration> {
        if self.finished {
            return None;
        }
        self.pending.front().map(|item| {
            Duration::from_millis(
                item.at
                    .monotonic
                    .0
                    .saturating_add(self.tail_millis)
                    .saturating_sub(now.0),
            )
        })
    }

    /// An overflow notification invalidates any queued tail as well. Keep the
    /// uncertain interval open until a new complete Sample establishes state.
    pub fn invalidate(&mut self, from: TimePoint, to: TimePoint) -> Output {
        if self.finished {
            return Output::default();
        }
        let mut from = from;
        let mut to = to;
        if let Some(first) = self.pending.front() {
            if first.at.monotonic < from.monotonic {
                from = first.at;
            }
        }
        if let Some(last) = self.pending.back() {
            if last.at.monotonic > to.monotonic {
                to = last.at;
            }
        }
        if let Some(processed) = self.processed {
            if processed.monotonic > to.monotonic {
                to = processed;
            }
        }
        self.pending.clear();
        let output = self.invalidate_interval(from, to);
        self.processed = Some(self.normalize(to));
        output
    }

    /// Called once after producers stop. Drain the remaining tail in order and
    /// close it at shutdown; repeated shutdown calls cannot write duplicates.
    pub fn finish(&mut self, at: TimePoint, last_input: Option<MonoTime>) -> Output {
        if self.finished {
            return Output::default();
        }
        let mut output = self.drain_through(at.monotonic);
        output.checkpoint = true;
        if let Some(processed) = self.processed {
            if at.monotonic < processed.monotonic {
                output.extend(self.invalidate(at, processed));
                self.finished = true;
                return output;
            }
        }
        let at = self.normalize(at);
        let last_input = self.aggregator.monotonic_input(at, last_input);
        if let Some(input) = last_input {
            self.correct_input_discontinuity(at, input, &mut output);
        }
        if last_input.is_none()
            && self.aggregator.confirmed_input(at).is_none()
            && !self.locked
            && !self.suspended
            && !self.waiting_sample
        {
            output.extend(self.invalidate_interval(self.uncertain_since(at), at));
        }
        if let Some(last_input) = last_input {
            output.extend_segments(self.aggregator.observe_input(at, last_input));
        }
        output.extend_segments(self.aggregator.close(at));
        if let Some(input) = last_input.filter(|input| self.known_idle(at, *input)) {
            self.close_idle_window(at, input, &mut output);
            self.close_gap(
                at.project(input.saturating_add_millis(self.afk_millis)),
                &mut output,
            );
        } else {
            if let Some(from) = self.idle_observed.take() {
                Self::add_gap(from, at, &mut output);
            }
            self.close_gap(at, &mut output);
        }
        self.pending.clear();
        self.processed = Some(at);
        self.finished = true;
        output
    }

    fn drain_through(&mut self, cutoff: MonoTime) -> Output {
        let mut output = Output::default();
        while self
            .pending
            .front()
            .is_some_and(|item| item.at.monotonic <= cutoff)
        {
            let observation = self.pending.pop_front().expect("checked above");
            let at = self.normalize(observation.at);
            output.extend(self.process(at, observation.kind));
            self.processed = Some(at);
        }
        output
    }

    fn process(&mut self, at: TimePoint, kind: ObservationKind) -> Output {
        match kind {
            ObservationKind::Foreground { app, last_input }
            | ObservationKind::Title { app, last_input } => {
                if self.locked || self.suspended {
                    return Output::default();
                }
                let observed_input = self.aggregator.monotonic_input(at, last_input);
                let effective_input =
                    observed_input.or_else(|| self.aggregator.confirmed_input(at));
                let mut output = Output::default();
                if let Some(input) = observed_input {
                    self.correct_input_discontinuity(at, input, &mut output);
                }
                // Even an unresolved HWND can carry a genuine recent input.
                // Observe it before closing, otherwise the old AFK deadline
                // can silently discard the valid prefix before the gap.
                if let Some(input) = observed_input {
                    output.extend_segments(self.aggregator.observe_input(at, input));
                }
                if let Some(input) = observed_input.filter(|input| self.known_idle(at, *input)) {
                    output.extend(self.mark_idle(at, input));
                    return output;
                }
                let Some(last_input) = effective_input else {
                    let from = self.uncertain_since(at);
                    output.extend(self.invalidate_interval(from, at));
                    return output;
                };
                let Some(app) = app else {
                    output.extend(self.open_gap(at));
                    return output;
                };
                if self.waiting_sample {
                    return output;
                }
                output.extend_segments(self.aggregator.observe_foreground(app, at, last_input));
                self.last_confirmed = Some(at);
                output
            }
            ObservationKind::Sample {
                app,
                last_input,
                locked,
                suspended,
            } => self.sample(at, app, last_input, locked, suspended),
            ObservationKind::Lock(locked) => self.control(at, locked, self.suspended),
            ObservationKind::Suspend(suspended) => self.control(at, self.locked, suspended),
        }
    }

    fn sample(
        &mut self,
        at: TimePoint,
        app: Option<AppKey>,
        last_input: Option<MonoTime>,
        locked: bool,
        suspended: bool,
    ) -> Output {
        let mut output = Output {
            checkpoint: true,
            ..Output::default()
        };
        let last_input = self.aggregator.monotonic_input(at, last_input);
        let effective_input = last_input.or_else(|| self.aggregator.confirmed_input(at));
        if let Some(input) = last_input {
            self.correct_input_discontinuity(at, input, &mut output);
        }
        if effective_input.is_none() && !self.locked && !self.suspended && !self.waiting_sample {
            let from = self.uncertain_since(at);
            output.extend(self.invalidate_interval(from, at));
        }
        if let Some(last_input) = last_input {
            output.extend_segments(self.aggregator.observe_input(at, last_input));
        }
        // Preserve a return window even if this observation immediately locks
        // or suspends the session; control() will clear the idle state.
        if let Some(input) = last_input.filter(|input| self.known_idle(at, *input)) {
            self.close_idle_window(at, input, &mut output);
        } else {
            if let Some(from) = self.idle_observed.take() {
                Self::add_gap(from, at, &mut output);
            }
        }
        output.extend(self.control(at, locked, suspended));
        if locked || suspended {
            self.close_gap(at, &mut output);
            return output;
        }
        if let Some(input) = effective_input.filter(|input| self.known_idle(at, *input)) {
            output.extend(self.mark_idle(at, input));
            return output;
        }
        let Some((app, last_input)) = app.zip(effective_input) else {
            output.extend(self.open_gap(at));
            return output;
        };

        // A periodic snapshot can discover a missed transition, but cannot say
        // when it happened. Remove the interval since the last known identity.
        // When samples are not required for recovery, the aggregator owns the
        // current identity. Keeping another AppKey here cloned every payload.
        if !self.waiting_sample
            && self
                .aggregator
                .current_app()
                .is_some_and(|known| known != &app)
        {
            let from = self.last_confirmed.or(self.processed).unwrap_or(at);
            output.extend(self.invalidate_interval(from, at));
        }
        self.close_gap(at, &mut output);
        self.waiting_sample = false;
        output.extend_segments(self.aggregator.observe_foreground(app, at, last_input));
        output.extend_segments(self.aggregator.checkpoint(at));
        self.last_confirmed = Some(at);
        output
    }

    fn control(&mut self, at: TimePoint, locked: bool, suspended: bool) -> Output {
        if self.locked == locked && self.suspended == suspended {
            return Output::default();
        }
        self.locked = locked;
        self.suspended = suspended;
        let segments = self.aggregator.set_suppression(at, locked, suspended);
        self.waiting_sample = true;
        self.last_confirmed = None;
        let idle_observed = self.idle_observed.take();
        let mut output = Output {
            segments,
            gaps: Vec::new(),
            checkpoint: true,
        };
        if let Some(from) = idle_observed {
            Self::add_gap(from, at, &mut output);
        }
        // Suppression has known semantics; it is not a missing-data interval.
        if locked || suspended {
            self.close_gap(at, &mut output);
        }
        output
    }

    fn open_gap(&mut self, at: TimePoint) -> Output {
        let segments = self.aggregator.close(at);
        self.aggregator.reset();
        self.waiting_sample = true;
        self.last_confirmed = None;
        let mut output = Output {
            segments,
            gaps: Vec::new(),
            checkpoint: true,
        };
        if let Some(from) = self.idle_observed.take() {
            Self::add_gap(from, at, &mut output);
        }
        // Persist progress through a long unknown period on every sample;
        // a crash must not erase the fact that these intervals were unknown.
        self.close_gap(at, &mut output);
        self.gap_start = Some(at);
        output
    }

    fn invalidate_interval(&mut self, from: TimePoint, to: TimePoint) -> Output {
        let from = self.normalize(from);
        let to = self.normalize(to);
        let mut output = Output {
            segments: self.aggregator.close(to),
            gaps: Vec::new(),
            checkpoint: true,
        };
        let mut from = self
            .gap_start
            .filter(|start| start.monotonic < from.monotonic)
            .unwrap_or(from);
        if let Some(idle) = self.idle_observed.take() {
            if idle.monotonic < from.monotonic {
                from = idle;
            }
        }
        Self::add_gap(from, to, &mut output);
        self.aggregator.reset();
        self.last_confirmed = None;
        self.waiting_sample = true;
        self.gap_start = Some(to);
        output
    }

    fn close_gap(&mut self, at: TimePoint, output: &mut Output) {
        if let Some(start) = self.gap_start.take() {
            Self::add_gap(start, at, output);
        }
    }

    fn add_gap(from: TimePoint, to: TimePoint, output: &mut Output) {
        if to.monotonic <= from.monotonic {
            return;
        }
        let start_unix = from.wall_unix_millis / 1_000;
        let end_unix = to.wall_unix_millis.div_ceil(1_000);
        if end_unix > start_unix {
            output.gaps.push(Gap {
                start_unix,
                end_unix,
            });
            output.checkpoint = true;
        }
    }

    fn normalize(&mut self, at: TimePoint) -> TimePoint {
        self.wall_anchor.get_or_insert(at).project(at.monotonic)
    }

    fn uncertain_since(&self, at: TimePoint) -> TimePoint {
        let mut from = self.last_confirmed.or(self.processed).unwrap_or(at);
        if let Some(through) = self.aggregator.confirmed_through() {
            // Previously observed real input proves the prefix through this
            // deadline even if the latest input API observation failed.
            if through > from.monotonic {
                from = from.project(through.min(at.monotonic));
            }
        }
        from
    }

    fn known_idle(&self, at: TimePoint, input: MonoTime) -> bool {
        at.monotonic.elapsed_millis_since(input) >= self.afk_millis
    }

    fn correct_input_discontinuity(&mut self, at: TimePoint, input: MonoTime, output: &mut Output) {
        let Some(previous_deadline) = self
            .aggregator
            .confirmed_through()
            .filter(|deadline| input > *deadline)
        else {
            return;
        };
        let until = input
            .saturating_add_millis(self.afk_millis)
            .min(at.monotonic);
        output.extend(self.invalidate_interval(at.project(previous_deadline), at.project(until)));
        if until < at.monotonic {
            // The latest input proves that the suffix is idle already. Do not
            // carry an open unknown interval into lock handling or shutdown.
            self.gap_start = None;
        }
    }

    fn mark_idle(&mut self, at: TimePoint, input: MonoTime) -> Output {
        let mut output = Output {
            segments: self.aggregator.close(at),
            ..Output::default()
        };
        self.close_idle_window(at, input, &mut output);
        self.close_gap(
            at.project(input.saturating_add_millis(self.afk_millis)),
            &mut output,
        );
        self.aggregator.reset();
        self.last_confirmed = Some(at);
        self.waiting_sample = true;
        self.idle_observed = Some(at);
        output
    }

    /// A later sample can already be idle again after an unobserved return.
    /// Preserve that unknown window, while excluding the confirmed idle suffix.
    fn close_idle_window(&mut self, at: TimePoint, input: MonoTime, output: &mut Output) {
        if let Some(from) = self.idle_observed.take() {
            if input > from.monotonic {
                Self::add_gap(
                    from,
                    at.project(input.saturating_add_millis(self.afk_millis)),
                    output,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn at(seconds: u64) -> TimePoint {
        TimePoint::new(seconds * 1_000, 1_000_000 + seconds * 1_000)
    }
    fn mono(seconds: u64) -> MonoTime {
        MonoTime(seconds * 1_000)
    }
    fn app(name: &str) -> AppKey {
        AppKey {
            path: name.into(),
            basename: name.into(),
            title: None,
        }
    }
    fn sample(name: &str, seconds: u64, input: u64) -> Observation {
        Observation {
            at: at(seconds),
            kind: ObservationKind::Sample {
                app: Some(app(name)),
                last_input: Some(mono(input)),
                locked: false,
                suspended: false,
            },
        }
    }
    fn fg(name: Option<&str>, seconds: u64, input: u64) -> Observation {
        Observation {
            at: at(seconds),
            kind: ObservationKind::Foreground {
                app: name.map(app),
                last_input: Some(mono(input)),
            },
        }
    }
    fn collect(
        accounting: &mut Accounting,
        observations: impl IntoIterator<Item = Observation>,
    ) -> Output {
        let mut output = Output::default();
        for observation in observations {
            output.extend(accounting.push(observation));
        }
        output
    }
    fn ranges(output: &Output) -> Vec<(&str, u64, u64)> {
        output
            .segments
            .iter()
            .map(|segment| {
                (
                    segment.app_basename.as_str(),
                    segment.start_unix - 1_000,
                    segment.end_unix - 1_000,
                )
            })
            .collect()
    }

    #[test]
    fn regressing_input_timestamp_keeps_the_witnessed_deadline() {
        for ending in 0..4 {
            for end in [390, 450] {
                let mut accounting = Accounting::new(300);
                let mut out = collect(&mut accounting, [sample("a", 0, 0), sample("a", 100, 100)]);
                let observation = match ending {
                    0 => None,
                    1 => Some(sample("a", 350, 0)),
                    2 => Some(fg(Some("b"), 350, 0)),
                    _ => Some(Observation {
                        at: at(350),
                        kind: ObservationKind::Title {
                            app: Some(app("b")),
                            last_input: Some(mono(0)),
                        },
                    }),
                };
                if let Some(observation) = observation {
                    out.extend(accounting.push(observation));
                }
                out.extend(accounting.finish(at(end), Some(mono(0))));
                assert_eq!(
                    out.segments.iter().map(Segment::duration).sum::<u64>(),
                    end.min(400),
                    "ending={ending}, end={end}"
                );
                assert!(out.gaps.is_empty(), "ending={ending}, end={end}");
                if ending >= 2 {
                    assert!(out
                        .segments
                        .iter()
                        .any(|segment| segment.app_basename == "b" && segment.start_unix == 1350));
                }
            }
        }
    }

    #[test]
    fn sorts_tail_before_checkpoint_and_preserves_every_foreground() {
        let mut accounting = Accounting::new(300);
        let mut out = collect(
            &mut accounting,
            [
                sample("a", 0, 0),
                sample("c", 10, 9),
                fg(Some("c"), 4, 4),
                fg(Some("b"), 2, 2),
            ],
        );
        out.extend(accounting.drain_ready(mono(12)));
        out.extend(accounting.finish(at(12), Some(mono(12))));
        assert_eq!(
            ranges(&out),
            vec![("a", 0, 2), ("b", 2, 4), ("c", 4, 10), ("c", 10, 12)]
        );
        assert!(out.gaps.is_empty());
    }

    #[test]
    fn keeps_two_seconds_uncommitted_and_never_samples_consumer_now() {
        let mut accounting = Accounting::new(300);
        accounting.push(sample("a", 0, 0));
        assert_eq!(accounting.next_wait(mono(0)), Some(Duration::from_secs(2)));
        assert!(accounting.drain_ready(mono(1)).is_empty());
        let first_sample = accounting.drain_ready(mono(2));
        assert!(first_sample.segments.is_empty());
        assert!(first_sample.gaps.is_empty());
        assert!(first_sample.checkpoint);
        accounting.push(sample("a", 5, 5));
        assert!(accounting.drain_ready(mono(6)).is_empty());
        let out = accounting.drain_ready(mono(100));
        assert_eq!(ranges(&out), vec![("a", 0, 5)]);
    }

    #[test]
    fn unknown_foreground_recovers_at_sample_without_overlapping_old_segment() {
        let mut accounting = Accounting::new(300);
        collect(
            &mut accounting,
            [sample("a", 0, 0), fg(None, 2, 1), sample("b", 3, 1)],
        );
        let mut out = accounting.drain_ready(mono(5));
        out.extend(accounting.finish(at(5), Some(mono(1))));
        assert_eq!(ranges(&out), vec![("a", 0, 2), ("b", 3, 5)]);
        assert_eq!(
            (out.gaps[0].start_unix, out.gaps[0].end_unix),
            (1_002, 1_003)
        );
        assert!(out
            .segments
            .windows(2)
            .all(|pair| pair[0].end_unix <= pair[1].start_unix));
    }

    #[test]
    fn title_input_prevents_false_afk_and_recovery_gap() {
        let mut accounting = Accounting::new(300);
        let mut titled = app("a");
        titled.title = Some("edited".into());
        let title = Observation {
            at: at(306),
            kind: ObservationKind::Title {
                app: Some(titled.clone()),
                last_input: Some(mono(304)),
            },
        };
        let later = Observation {
            at: at(330),
            kind: ObservationKind::Sample {
                app: Some(titled),
                last_input: Some(mono(329)),
                locked: false,
                suspended: false,
            },
        };
        collect(
            &mut accounting,
            [
                sample("a", 0, 0),
                sample("a", 270, 5),
                sample("a", 300, 5),
                title,
                later,
            ],
        );
        let out = accounting.finish(at(340), Some(mono(339)));
        assert_eq!(out.segments.iter().map(Segment::duration).sum::<u64>(), 340);
        assert!(out.gaps.is_empty());
    }

    #[test]
    fn very_late_foreground_invalidates_committed_time_and_waits_for_sample() {
        let mut accounting = Accounting::new(300);
        collect(&mut accounting, [sample("a", 0, 0), sample("a", 10, 10)]);
        let mut out = accounting.drain_ready(mono(12));
        out.extend(accounting.push(fg(Some("b"), 4, 4)));
        collect(
            &mut accounting,
            [fg(Some("c"), 11, 11), sample("c", 12, 12)],
        );
        out.extend(accounting.finish(at(15), Some(mono(15))));
        assert_eq!(ranges(&out), vec![("a", 0, 10), ("c", 12, 15)]);
        assert_eq!(
            out.gaps
                .iter()
                .map(|gap| (gap.start_unix, gap.end_unix))
                .collect::<Vec<_>>(),
            vec![(1_004, 1_010), (1_010, 1_012)]
        );
    }

    #[test]
    fn overflow_clears_bounded_tail_and_marks_all_discarded_time() {
        let mut accounting = Accounting::with_limits(300, Duration::from_secs(2), 2);
        accounting.push(sample("a", 0, 0));
        accounting.drain_ready(mono(2));
        accounting.push(fg(Some("b"), 3, 3));
        accounting.push(fg(Some("c"), 4, 4));
        let mut out = accounting.push(sample("c", 5, 5));
        assert!(accounting.pending.is_empty());
        accounting.push(sample("d", 7, 7));
        out.extend(accounting.finish(at(9), Some(mono(9))));
        assert_eq!(
            out.gaps
                .iter()
                .map(|gap| (gap.start_unix, gap.end_unix))
                .collect::<Vec<_>>(),
            vec![(1_000, 1_005), (1_005, 1_007)]
        );
        assert_eq!(ranges(&out).last().copied(), Some(("d", 7, 9)));
    }

    #[test]
    fn independent_suppression_requires_fresh_sample_after_both_release() {
        let mut accounting = Accounting::new(300);
        collect(
            &mut accounting,
            [
                sample("a", 0, 0),
                Observation {
                    at: at(5),
                    kind: ObservationKind::Lock(true),
                },
                Observation {
                    at: at(6),
                    kind: ObservationKind::Suspend(true),
                },
                Observation {
                    at: at(100),
                    kind: ObservationKind::Suspend(false),
                },
                fg(Some("b"), 101, 101),
                Observation {
                    at: at(102),
                    kind: ObservationKind::Lock(false),
                },
                fg(Some("b"), 103, 103),
                sample("b", 104, 103),
            ],
        );
        let out = accounting.finish(at(110), Some(mono(109)));
        assert_eq!(ranges(&out), vec![("a", 0, 5), ("b", 104, 110)]);
        assert!(out.gaps.is_empty());
    }

    #[test]
    fn invalidation_uses_same_wall_mapping_after_clock_rollback() {
        let mut accounting = Accounting::new(300);
        accounting.push(sample("a", 0, 0));
        let mut next = sample("a", 10, 10);
        next.at.wall_unix_millis = 1;
        accounting.push(next);
        let committed = accounting.drain_ready(mono(12));
        assert_eq!(ranges(&committed), vec![("a", 0, 10)]);
        let invalidated =
            accounting.invalidate(TimePoint::new(4_500, 0), TimePoint::new(10_000, 1));
        assert_eq!(
            (invalidated.gaps[0].start_unix, invalidated.gaps[0].end_unix),
            (1_004, 1_010)
        );
    }

    #[test]
    fn sample_identity_mismatch_marks_uncertain_interval_instead_of_guessing() {
        let mut accounting = Accounting::new(300);
        collect(
            &mut accounting,
            [sample("a", 0, 0), sample("a", 5, 5), sample("b", 10, 10)],
        );
        let out = accounting.finish(at(12), Some(mono(12)));
        assert_eq!(
            (out.gaps[0].start_unix, out.gaps[0].end_unix),
            (1_005, 1_010)
        );
        assert_eq!(ranges(&out).last().copied(), Some(("b", 10, 12)));
    }

    #[test]
    fn shutdown_is_idempotent_even_with_an_open_gap() {
        let mut accounting = Accounting::new(300);
        collect(&mut accounting, [sample("a", 0, 0), fg(None, 2, 1)]);
        let first = accounting.finish(at(5), Some(mono(1)));
        assert_eq!(ranges(&first), vec![("a", 0, 2)]);
        assert_eq!(
            (first.gaps[0].start_unix, first.gaps[0].end_unix),
            (1_002, 1_005)
        );
        assert!(accounting.finish(at(6), Some(mono(6))).is_empty());
        assert!(accounting.push(sample("b", 7, 7)).is_empty());
        assert!(accounting.drain_ready(mono(9)).is_empty());
    }

    #[test]
    fn late_invalidation_preserves_the_uncommitted_valid_prefix() {
        let mut accounting = Accounting::new(300);
        collect(
            &mut accounting,
            [sample("a", 0, 0), sample("a", 5, 5), fg(Some("a"), 10, 10)],
        );
        let committed = accounting.drain_ready(mono(12));
        assert_eq!(ranges(&committed), vec![("a", 0, 5)]);
        let correction = accounting.push(fg(Some("b"), 8, 8));
        // The query subtracts [8,10], preserving the not-yet-written [5,8]
        // prefix as well as the already-checkpointed [0,5].
        assert_eq!(ranges(&correction), vec![("a", 5, 10)]);
        assert_eq!(
            (correction.gaps[0].start_unix, correction.gaps[0].end_unix),
            (1_008, 1_010)
        );
        assert!(correction.checkpoint);
    }

    #[test]
    fn unresolved_foreground_observes_real_input_before_closing_prefix() {
        let mut accounting = Accounting::new(300);
        collect(
            &mut accounting,
            [
                sample("a", 0, 0),
                sample("a", 270, 5),
                sample("a", 300, 5),
                fg(None, 310, 304),
                sample("a", 320, 319),
            ],
        );
        let out = accounting.finish(at(330), Some(mono(329)));
        assert_eq!(out.segments.iter().map(Segment::duration).sum::<u64>(), 320);
        assert!(ranges(&out).contains(&("a", 300, 310)));
        assert_eq!(
            (out.gaps[0].start_unix, out.gaps[0].end_unix),
            (1_310, 1_320)
        );
    }

    #[test]
    fn delayed_callback_can_reuse_confirmed_input_without_refreshing_it_or_fsync() {
        let mut accounting = Accounting::new(300);
        accounting.push(sample("a", 0, 0));
        assert!(accounting.drain_ready(mono(2)).checkpoint);
        accounting.push(Observation {
            at: at(1),
            kind: ObservationKind::Foreground {
                app: Some(app("b")),
                last_input: None,
            },
        });
        let transition = accounting.drain_ready(mono(3));
        assert_eq!(ranges(&transition), vec![("a", 0, 1)]);
        assert!(transition.gaps.is_empty());
        assert!(!transition.checkpoint);
        accounting.push(sample("b", 301, 0));
        let idle = accounting.drain_ready(mono(303));
        assert_eq!(ranges(&idle), vec![("b", 1, 300)]);
        assert!(idle.gaps.is_empty());
        assert!(idle.checkpoint);
    }

    #[test]
    fn expired_input_cache_marks_unknown_time_instead_of_silently_trimming_it() {
        let mut accounting = Accounting::new(300);
        let delayed = Observation {
            at: at(310),
            kind: ObservationKind::Foreground {
                app: Some(app("b")),
                last_input: None,
            },
        };
        collect(
            &mut accounting,
            [
                sample("a", 0, 0),
                sample("a", 270, 0),
                delayed,
                sample("b", 330, 329),
            ],
        );
        let out = accounting.finish(at(340), Some(mono(339)));
        assert_eq!(
            ranges(&out),
            vec![("a", 0, 270), ("a", 270, 300), ("b", 330, 340)]
        );
        assert_eq!(
            out.gaps
                .iter()
                .map(|gap| (gap.start_unix, gap.end_unix))
                .collect::<Vec<_>>(),
            vec![(1_300, 1_310), (1_310, 1_330)]
        );
    }

    #[test]
    fn unknown_samples_commit_gap_progress_before_recovery_or_shutdown() {
        let mut accounting = Accounting::new(300);
        let unknown = |seconds, input| Observation {
            at: at(seconds),
            kind: ObservationKind::Sample {
                app: None,
                last_input: Some(mono(input)),
                locked: false,
                suspended: false,
            },
        };
        collect(
            &mut accounting,
            [sample("a", 0, 0), fg(None, 2, 1), unknown(10, 9)],
        );
        let first = accounting.drain_ready(mono(12));
        assert_eq!(
            (first.gaps[0].start_unix, first.gaps[0].end_unix),
            (1_002, 1_010)
        );
        assert!(first.checkpoint);
        accounting.push(unknown(20, 19));
        let second = accounting.drain_ready(mono(22));
        assert_eq!(
            (second.gaps[0].start_unix, second.gaps[0].end_unix),
            (1_010, 1_020)
        );
        assert!(second.checkpoint);
    }

    #[test]
    fn confirmed_afk_with_no_foreground_is_not_missing_data() {
        let mut accounting = Accounting::new(300);
        let idle = |seconds| Observation {
            at: at(seconds),
            kind: ObservationKind::Sample {
                app: None,
                last_input: Some(mono(0)),
                locked: false,
                suspended: false,
            },
        };
        collect(&mut accounting, [sample("a", 0, 0), idle(600), idle(900)]);
        let out = accounting.finish(at(910), Some(mono(0)));
        assert_eq!(ranges(&out), vec![("a", 0, 300)]);
        assert!(out.gaps.is_empty());
    }

    #[test]
    fn afk_recovery_reports_uncertain_return_window_without_backfilling_activity() {
        let mut accounting = Accounting::new(300);
        collect(
            &mut accounting,
            [
                sample("a", 0, 0),
                sample("a", 300, 0),
                sample("a", 600, 0),
                sample("a", 630, 629),
            ],
        );
        let out = accounting.finish(at(640), Some(mono(639)));
        assert_eq!(ranges(&out), vec![("a", 0, 300), ("a", 630, 640)]);
        assert_eq!(
            (out.gaps[0].start_unix, out.gaps[0].end_unix),
            (1_600, 1_630)
        );
    }

    #[test]
    fn input_beyond_old_afk_deadline_does_not_fill_an_unobserved_idle_return() {
        let mut accounting = Accounting::new(300);
        collect(
            &mut accounting,
            [
                sample("a", 0, 0),
                sample("a", 270, 0),
                sample("a", 330, 329),
            ],
        );
        let out = accounting.finish(at(340), Some(mono(339)));
        assert_eq!(
            ranges(&out),
            vec![("a", 0, 270), ("a", 270, 300), ("a", 330, 340)]
        );
        assert_eq!(
            (out.gaps[0].start_unix, out.gaps[0].end_unix),
            (1_300, 1_330)
        );
    }

    #[test]
    fn shutdown_after_unobserved_afk_return_records_gap_instead_of_losing_it() {
        let mut accounting = Accounting::new(300);
        collect(&mut accounting, [sample("a", 0, 0), sample("a", 300, 0)]);
        let out = accounting.finish(at(330), Some(mono(329)));
        assert_eq!(ranges(&out), vec![("a", 0, 300)]);
        assert_eq!(
            (out.gaps[0].start_unix, out.gaps[0].end_unix),
            (1_300, 1_330)
        );
        assert!(out.checkpoint);
        assert!(accounting.finish(at(331), Some(mono(330))).is_empty());
    }

    #[test]
    fn locking_sample_preserves_an_unobserved_afk_return_window() {
        let mut accounting = Accounting::new(300);
        let lock = Observation {
            at: at(330),
            kind: ObservationKind::Sample {
                app: None,
                last_input: Some(mono(329)),
                locked: true,
                suspended: false,
            },
        };
        collect(
            &mut accounting,
            [sample("a", 0, 0), sample("a", 300, 0), lock],
        );
        let out = accounting.finish(at(600), Some(mono(329)));
        assert_eq!(ranges(&out), vec![("a", 0, 300)]);
        assert_eq!(
            (out.gaps[0].start_unix, out.gaps[0].end_unix),
            (1_300, 1_330)
        );
        assert_eq!(out.gaps.len(), 1);
    }

    #[test]
    fn renewed_input_between_idle_observations_preserves_the_missed_return_window() {
        for terminal_is_sample in [false, true] {
            let mut accounting = Accounting::new(300);
            collect(&mut accounting, [sample("a", 0, 0), sample("a", 300, 0)]);
            if terminal_is_sample {
                accounting.push(sample("a", 900, 500));
            }
            let out = accounting.finish(at(901), Some(mono(500)));
            assert_eq!(ranges(&out), vec![("a", 0, 300)]);
            assert_eq!(
                out.gaps
                    .iter()
                    .map(|gap| (gap.start_unix, gap.end_unix))
                    .collect::<Vec<_>>(),
                vec![(1_300, 1_800)],
                "new input proves an unknown return before the new idle deadline; sample={terminal_is_sample}"
            );
        }
    }

    #[test]
    fn locking_while_still_idle_does_not_invent_missing_activity() {
        for renewed_input in [0, 500] {
            let mut accounting = Accounting::new(300);
            let lock = Observation {
                at: at(900),
                kind: ObservationKind::Sample {
                    app: None,
                    last_input: Some(mono(renewed_input)),
                    locked: true,
                    suspended: false,
                },
            };
            collect(
                &mut accounting,
                [sample("a", 0, 0), sample("a", 300, 0), lock],
            );
            let out = accounting.finish(at(910), Some(mono(renewed_input)));
            assert_eq!(ranges(&out), vec![("a", 0, 300)]);
            let expected = if renewed_input == 0 {
                Vec::new()
            } else {
                vec![(1_300, 1_800)]
            };
            assert_eq!(
                out.gaps
                    .iter()
                    .map(|gap| (gap.start_unix, gap.end_unix))
                    .collect::<Vec<_>>(),
                expected,
                "the latest confirmed idle suffix must stay excluded"
            );
        }
    }

    #[test]
    fn renewed_idle_gap_uses_session_clock_and_waits_for_the_pending_tail() {
        for foreground in [false, true] {
            let mut accounting = Accounting::new(300);
            accounting.push(sample("a", 0, 0));
            accounting.drain_ready(mono(2));
            accounting.push(sample("a", 300, 0));
            let prefix = accounting.drain_ready(mono(302));
            assert_eq!(ranges(&prefix), vec![("a", 0, 300)]);

            let mut returned_then_idle = if foreground {
                fg(Some("a"), 900, 500)
            } else {
                sample("a", 900, 500)
            };
            returned_then_idle.at.wall_unix_millis = 1;
            assert!(accounting.push(returned_then_idle).is_empty());
            assert!(accounting.drain_ready(mono(901)).is_empty());
            let correction = accounting.drain_ready(mono(902));
            assert!(correction.segments.is_empty());
            assert!(correction.checkpoint);
            assert_eq!(
                correction
                    .gaps
                    .iter()
                    .map(|gap| (gap.start_unix, gap.end_unix))
                    .collect::<Vec<_>>(),
                vec![(1_300, 1_800)]
            );

            let mut still_idle = sample("a", 930, 500);
            still_idle.at.wall_unix_millis = u64::MAX;
            accounting.push(still_idle);
            let repeated = accounting.drain_ready(mono(932));
            assert!(repeated.gaps.is_empty());
            assert!(repeated.segments.is_empty());
            let finished = accounting.finish(at(940), Some(mono(500)));
            assert!(finished.gaps.is_empty());
            assert!(finished.segments.is_empty());
        }
    }

    #[test]
    fn long_sampling_pause_keeps_the_latest_confirmed_idle_suffix_out_of_gaps() {
        for ending in 0..6 {
            let mut accounting = Accounting::new(300);
            accounting.push(sample("a", 0, 0));
            let mut out = accounting.drain_ready(mono(2));
            // There was no intermediate sample to close the old AFK window.
            // The new input proves an unknown return, followed by known idle
            // from 800 onwards. This holds even if the next action is a lock.
            if ending != 0 {
                let mut observation = if ending == 1 {
                    fg(Some("b"), 900, 500)
                } else {
                    Observation {
                        at: at(900),
                        kind: ObservationKind::Sample {
                            app: (ending != 5).then(|| app("b")),
                            last_input: Some(mono(500)),
                            locked: ending == 3,
                            suspended: ending == 4,
                        },
                    }
                };
                observation.at.wall_unix_millis = 1;
                out.extend(accounting.push(observation));
            }
            out.extend(accounting.finish(at(910), Some(mono(500))));
            assert_eq!(ranges(&out), vec![("a", 0, 300)]);
            assert_eq!(
                out.gaps
                    .iter()
                    .map(|g| (g.start_unix, g.end_unix))
                    .collect::<Vec<_>>(),
                vec![(1_300, 1_800)],
                "known idle must not be reported as missing: ending={ending}"
            );
            assert!(accounting.finish(at(920), Some(mono(500))).is_empty());
        }
    }
}
