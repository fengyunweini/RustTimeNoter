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

/// Opaque foreground identity; zero fields mean unknown. Keeping this token
/// beside an observation avoids inferring window identity from an app path.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WindowIdentity {
    pub hwnd: isize,
    pub pid: u32,
}

impl WindowIdentity {
    fn is_known(self) -> bool {
        self.hwnd != 0 && self.pid != 0
    }
}

#[derive(Debug, Clone, Copy)]
struct SameTickForeground {
    at: MonoTime,
    previous: WindowIdentity,
    current: WindowIdentity,
}

impl SameTickForeground {
    fn is_unambiguous(self) -> bool {
        self.previous.is_known() && self.current.is_known() && self.previous != self.current
    }
}

fn same_application(left: &AppKey, right: &AppKey) -> bool {
    left.path == right.path && left.basename == right.basename
}

#[derive(Debug, Clone)]
pub struct Observation {
    pub at: TimePoint,
    pub window: WindowIdentity,
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
    current_window: WindowIdentity,
    same_tick_foreground: Option<SameTickForeground>,
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
    // Only the ongoing confirmed exclusion is retained. Older intervals are
    // not an unbounded history cache for arbitrarily late corrections.
    suppressed_since: Option<TimePoint>,
    idle_since: Option<TimePoint>,
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
            current_window: WindowIdentity::default(),
            same_tick_foreground: None,
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
            suppressed_since: None,
            idle_since: None,
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
                return self.invalidate_late(observation, processed);
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

    fn invalidate_late(&mut self, observation: Observation, processed: TimePoint) -> Output {
        let preserves_control_state = matches!(
            observation.kind,
            ObservationKind::Foreground { .. } | ObservationKind::Title { .. }
        );
        let changed_sample = match &observation.kind {
            ObservationKind::Sample {
                app,
                locked,
                suspended,
                ..
            } => {
                !self.waiting_sample
                    && !locked
                    && !suspended
                    && observation.window.is_known()
                    && (self.current_window != observation.window
                        || app
                            .as_ref()
                            .zip(self.aggregator.current_app())
                            .is_some_and(|(observed, active)| !same_application(observed, active)))
            }
            _ => false,
        };
        let unmatched_title = match &observation.kind {
            ObservationKind::Title { app, .. } => {
                !observation.window.is_known()
                    || self.current_window != observation.window
                    || !app
                        .as_ref()
                        .zip(self.aggregator.current_app())
                        .is_some_and(|(observed, active)| same_application(observed, active))
            }
            _ => false,
        };
        let input = match observation.kind {
            ObservationKind::Foreground { last_input, .. }
            | ObservationKind::Title { last_input, .. }
            | ObservationKind::Sample { last_input, .. } => last_input,
            _ => None,
        }
        .filter(|input| *input <= observation.at.monotonic);
        let seen = self.aggregator.input_seen();
        let renewed_input = input.is_some_and(|input| seen.is_none_or(|seen| input > seen));
        let mut from = observation.at;
        if changed_sample {
            // A different snapshot gives no transition timestamp. Correct
            // from the last confirmed identity, just as an ordered sample
            // does, before input handling can reset that evidence.
            if let Some(confirmed) = self
                .last_confirmed
                .filter(|confirmed| confirmed.monotonic < from.monotonic)
            {
                from = confirmed;
            }
        }
        if renewed_input {
            if let Some(deadline) = seen.map(|seen| seen.saturating_add_millis(self.afk_millis)) {
                if (self.aggregator.current_app().is_none()
                    || unmatched_title
                    || input.is_some_and(|input| input > deadline))
                    && deadline < from.monotonic
                {
                    from = from.project(deadline);
                }
            }
            if let Some(idle) = self
                .idle_since
                .filter(|idle| idle.monotonic < from.monotonic)
            {
                from = idle;
            }
        }
        let suppression = self.suppressed_since;
        let idle = self.idle_since.and_then(|idle| {
            if renewed_input {
                // New input can disprove the old idle prefix while still
                // confirming a later idle suffix at the processed boundary.
                input
                    .map(|input| input.saturating_add_millis(self.afk_millis))
                    .filter(|deadline| *deadline <= processed.monotonic)
                    .map(|deadline| processed.project(deadline))
            } else {
                Some(idle)
            }
        });
        let excluded = preserves_control_state
            .then_some(suppression.or(idle))
            .flatten();
        let idle_observed = self.idle_observed;
        let mut output = Output::default();
        if let Some(input) = input {
            // Retain genuine input globally. Any extension without a known
            // foreground boundary is covered by the correction above.
            output.extend_segments(self.aggregator.observe_input(processed, input));
        }
        if let Some(excluded) = excluded {
            if from.monotonic < excluded.monotonic {
                output.extend(self.invalidate_interval(from, excluded));
            }
            self.suppressed_since = suppression;
            self.idle_since = idle;
            self.idle_observed = idle_observed;
            self.gap_start = None;
        } else {
            // A late control/sample or new input can disprove the previous
            // exclusion. Preserve the full correction and await fresh state.
            output.extend(self.invalidate_interval(from, processed));
        }
        output
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

    /// A capture that cannot settle may hide a transition anywhere in this
    /// session, including before the consumer thread first ran.
    pub fn invalidate_session(&mut self, fallback_start: TimePoint, to: TimePoint) -> Output {
        // The mapping anchor survives checkpoints and state resets. Keep the
        // fallback too: no observation may have established an anchor yet.
        let from = self
            .wall_anchor
            .filter(|anchor| anchor.monotonic < fallback_start.monotonic)
            .unwrap_or(fallback_start);
        self.invalidate(from, to)
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
        self.current_window = WindowIdentity::default();
        self.same_tick_foreground = None;
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
        self.suppressed_since = None;
        self.idle_since = None;
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
            output.extend(self.process(at, observation.window, observation.kind));
            self.processed = Some(at);
        }
        output
    }

    fn process(&mut self, at: TimePoint, window: WindowIdentity, kind: ObservationKind) -> Output {
        if self
            .same_tick_foreground
            .is_some_and(|evidence| evidence.at != at.monotonic)
        {
            self.same_tick_foreground = None;
        }
        if let ObservationKind::Title { ref app, .. } = kind {
            // The callback's current-foreground prefilter says nothing about
            // which window was foreground at this older event timestamp.
            // Check after ordering, before input, AFK or gap side effects.
            if self.waiting_sample
                || !self.aggregator.is_active()
                || !window.is_known()
                || self.current_window != window
                || !app
                    .as_ref()
                    .zip(self.aggregator.current_app())
                    .is_some_and(|(observed, active)| same_application(observed, active))
            {
                return Output::default();
            }
        }
        let foreground = matches!(kind, ObservationKind::Foreground { .. });
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
                let Some(app) = app.filter(|_| window.is_known()) else {
                    output.extend(self.open_gap(at));
                    return output;
                };
                if self.waiting_sample {
                    return output;
                }
                output.extend_segments(self.aggregator.observe_foreground(app, at, last_input));
                if foreground {
                    self.remember_foreground(at, self.current_window, window);
                    self.current_window = window;
                    self.last_confirmed = Some(at);
                }
                output
            }
            ObservationKind::Sample {
                app,
                last_input,
                locked,
                suspended,
            } => self.sample(at, window, app, last_input, locked, suspended),
            ObservationKind::Lock(locked) => self.control(at, locked, self.suspended),
            ObservationKind::Suspend(suspended) => self.control(at, self.locked, suspended),
        }
    }

    fn remember_foreground(
        &mut self,
        at: TimePoint,
        previous: WindowIdentity,
        current: WindowIdentity,
    ) {
        if let Some(evidence) = self
            .same_tick_foreground
            .as_mut()
            .filter(|evidence| evidence.at == at.monotonic)
        {
            // Duplicate notifications retain the original old window. More
            // than one new foreground in a coarse timestamp is ambiguous.
            if evidence.current != current {
                evidence.previous = WindowIdentity::default();
                evidence.current = WindowIdentity::default();
            }
        } else {
            self.same_tick_foreground = Some(SameTickForeground {
                at: at.monotonic,
                previous,
                current,
            });
        }
    }

    fn mark_ambiguous_tick(&mut self, at: TimePoint) {
        self.same_tick_foreground = Some(SameTickForeground {
            at: at.monotonic,
            previous: WindowIdentity::default(),
            current: WindowIdentity::default(),
        });
    }

    fn reset_foreground_evidence(&mut self, at: TimePoint) {
        if !self
            .same_tick_foreground
            .is_some_and(|evidence| evidence.at == at.monotonic && !evidence.current.is_known())
        {
            self.same_tick_foreground = None;
        }
    }

    fn matching_pending_foreground(
        &self,
        at: TimePoint,
        window: WindowIdentity,
        app: Option<&AppKey>,
    ) -> bool {
        if self.same_tick_foreground.is_some_and(|evidence| {
            evidence.at == at.monotonic
                && (!evidence.is_unambiguous() || evidence.current != window)
        }) {
            return false;
        }
        let mut matched = false;
        for observation in self
            .pending
            .iter()
            .take_while(|observation| observation.at.monotonic == at.monotonic)
        {
            if let ObservationKind::Foreground { app: observed, .. } = &observation.kind {
                let same_app = match (observed.as_ref(), app) {
                    (Some(observed), Some(app)) => same_application(observed, app),
                    (None, None) => true,
                    _ => false,
                };
                if observation.window != window || !same_app {
                    return false;
                }
                matched = true;
            }
        }
        matched
    }

    fn sample(
        &mut self,
        at: TimePoint,
        window: WindowIdentity,
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
        let old_snapshot = self.same_tick_foreground.is_some_and(|evidence| {
            evidence.at == at.monotonic
                && evidence.is_unambiguous()
                && evidence.previous == window
                && evidence.current == self.current_window
        });
        let ambiguous_snapshot = self.same_tick_foreground.is_some_and(|evidence| {
            evidence.at == at.monotonic
                && (!evidence.current.is_known()
                    || (window != evidence.previous && window != evidence.current))
        });
        // Detect a missed window/application transition before input handling can
        // close/reset the old active identity at its AFK deadline. A title
        // update is metadata, so only foreground events/samples advance the
        // identity confirmation boundary used by this correction.
        let changed = !old_snapshot
            && !ambiguous_snapshot
            && !self.waiting_sample
            && !locked
            && !suspended
            && window.is_known()
            && (self.current_window != window
                || app
                    .as_ref()
                    .zip(self.aggregator.current_app())
                    .is_some_and(|(observed, active)| !same_application(observed, active)));
        let matching_foreground =
            changed && self.matching_pending_foreground(at, window, app.as_ref());
        let previous_window = self.current_window;
        if changed && !matching_foreground {
            let from = self.last_confirmed.or(self.processed).unwrap_or(at);
            let until = effective_input
                .map(|input| {
                    input
                        .saturating_add_millis(self.afk_millis)
                        .min(at.monotonic)
                })
                .unwrap_or(at.monotonic);
            output.extend(self.invalidate_interval(from, at.project(until)));
            if until < at.monotonic {
                self.gap_start = None;
            }
        } else if let Some(input) = last_input {
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
            self.idle_since = None;
            if let Some(from) = self.idle_observed.take() {
                Self::add_gap(from, at, &mut output);
            }
        }
        output.extend(self.control(at, locked, suspended));
        if locked || suspended {
            self.close_gap(at, &mut output);
            if ambiguous_snapshot {
                self.mark_ambiguous_tick(at);
            }
            return output;
        }
        if let Some(input) = effective_input.filter(|input| self.known_idle(at, *input)) {
            output.extend(self.mark_idle(at, input));
            if ambiguous_snapshot {
                self.mark_ambiguous_tick(at);
            }
            return output;
        }
        if ambiguous_snapshot {
            output.extend(self.open_gap(at));
            self.mark_ambiguous_tick(at);
            return output;
        }
        if old_snapshot {
            // The input/control fields remain useful, but an old-window
            // sample sharing the transition's coarse timestamp cannot undo
            // its authoritative foreground identity.
            if !self.waiting_sample {
                output.extend_segments(self.aggregator.checkpoint(at));
            }
            return output;
        }
        let Some((app, last_input)) = app.filter(|_| window.is_known()).zip(effective_input) else {
            output.extend(self.open_gap(at));
            return output;
        };

        self.close_gap(at, &mut output);
        self.waiting_sample = false;
        if matching_foreground {
            self.remember_foreground(at, previous_window, window);
        }
        self.current_window = window;
        output.extend_segments(self.aggregator.observe_foreground(app, at, last_input));
        output.extend_segments(self.aggregator.checkpoint(at));
        self.last_confirmed = Some(at);
        output
    }

    fn control(&mut self, at: TimePoint, locked: bool, suspended: bool) -> Output {
        if self.locked == locked && self.suspended == suspended {
            if locked || suspended {
                self.suppressed_since.get_or_insert(at);
            }
            return Output::default();
        }
        self.idle_since = None;
        if locked || suspended {
            self.suppressed_since.get_or_insert(at);
        } else {
            self.suppressed_since = None;
        }
        self.locked = locked;
        self.suspended = suspended;
        let segments = self.aggregator.set_suppression(at, locked, suspended);
        self.waiting_sample = true;
        self.current_window = WindowIdentity::default();
        self.reset_foreground_evidence(at);
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
        self.current_window = WindowIdentity::default();
        self.reset_foreground_evidence(at);
        self.suppressed_since = None;
        self.idle_since = None;
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
        self.current_window = WindowIdentity::default();
        self.reset_foreground_evidence(to);
        self.suppressed_since = None;
        self.idle_since = None;
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
        let previous_deadline = match self.aggregator.confirmed_through() {
            Some(deadline) if input > deadline => deadline,
            Some(_) => return,
            None => {
                // A newly witnessed input may predate the observation that
                // declared us idle. That disproves its old AFK boundary even
                // if the input itself is earlier than that boundary. Keep the
                // session input evidence after active/idle state was closed
                // for a lock or suspend, and correct only before suppression.
                let Some(seen) = self.aggregator.input_seen().filter(|seen| input > *seen) else {
                    return;
                };
                if !self
                    .idle_observed
                    .is_some_and(|idle| input <= idle.monotonic)
                    && !self
                        .suppressed_since
                        .is_some_and(|since| input <= since.monotonic)
                {
                    return;
                }
                seen.saturating_add_millis(self.afk_millis)
            }
        };
        // Never invent a correction before this session first observed state.
        let from = self.wall_anchor.map_or(previous_deadline, |anchor| {
            previous_deadline.max(anchor.monotonic)
        });
        let suppression = self.suppressed_since;
        let until = input
            .saturating_add_millis(self.afk_millis)
            .min(at.monotonic)
            .min(suppression.map_or(at.monotonic, |since| since.monotonic));
        if until <= from {
            return;
        }
        output.extend(self.invalidate_interval(at.project(from), at.project(until)));
        self.suppressed_since = suppression;
        if until < at.monotonic || suppression.is_some() {
            // The latest input proves that the suffix is idle already. Do not
            // carry an open unknown interval into idle/suppressed time.
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
        self.current_window = WindowIdentity::default();
        self.reset_foreground_evidence(at);
        self.idle_since = Some(at.project(input.saturating_add_millis(self.afk_millis)));
        self.last_confirmed = Some(at);
        self.waiting_sample = true;
        self.idle_observed = Some(at);
        output
    }

    /// A later sample can already be idle again after an unobserved return.
    /// Preserve that unknown window, while excluding the confirmed idle suffix.
    fn close_idle_window(&mut self, at: TimePoint, input: MonoTime, output: &mut Output) {
        if let Some(from) = self.idle_observed.take() {
            // Inputs that disprove this observation were handled before
            // observe_input by correct_input_discontinuity. This branch only
            // covers a return after an observation that remains trustworthy.
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
    fn window(name: &str) -> WindowIdentity {
        WindowIdentity {
            hwnd: name.as_bytes()[0] as isize,
            pid: name.as_bytes()[0] as u32,
        }
    }
    fn sample(name: &str, seconds: u64, input: u64) -> Observation {
        Observation {
            window: window(name),
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
            window: name.map(window).unwrap_or_default(),
            at: at(seconds),
            kind: ObservationKind::Foreground {
                app: name.map(app),
                last_input: Some(mono(input)),
            },
        }
    }
    fn title(name: &str, value: &str, seconds: u64, input: u64) -> Observation {
        Observation {
            at: at(seconds),
            window: window(name),
            kind: ObservationKind::Title {
                app: Some(AppKey {
                    title: Some(value.into()),
                    ..app(name)
                }),
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
    fn self_review_equal_time_sample_and_foreground_preserve_both_snapshot_orders() {
        for same_app in [false, true] {
            for old_sample in [false, true] {
                for foreground_first in [false, true] {
                    let mut accounting = Accounting::new(300);
                    accounting.push(sample("a", 0, 0));
                    let name = if same_app { "a" } else { "b" };
                    let mut transition = fg(Some(name), 6, 6);
                    transition.window.hwnd += 100;
                    let next_window = transition.window;
                    let mut snapshot = sample(if old_sample { "a" } else { name }, 6, 6);
                    if !old_sample {
                        snapshot.window = next_window;
                    }
                    let expected_window = next_window;
                    let mut out = collect(
                        &mut accounting,
                        if foreground_first {
                            [transition, snapshot]
                        } else {
                            [snapshot, transition]
                        },
                    );
                    out.extend(accounting.drain_ready(mono(8)));
                    assert_eq!(accounting.current_window, expected_window);
                    out.extend(accounting.finish(at(10), Some(mono(10))));
                    assert!(
                        out.gaps.is_empty(),
                        "same_app={same_app}, old_sample={old_sample}, foreground_first={foreground_first}: {:?}",
                        out.gaps
                    );
                    assert_eq!(out.segments.iter().map(Segment::duration).sum::<u64>(), 10);
                    let last = out.segments.last().unwrap();
                    assert_eq!(last.start_unix, 1006);
                    assert_eq!(last.app_basename, name);
                }
            }
        }
    }

    #[test]
    fn self_review_unknown_foreground_retains_input_evidence_for_regressing_recovery() {
        let mut accounting = Accounting::new(300);
        let mut unknown = fg(None, 290, 290);
        unknown.window = window("b");
        let mut out = collect(
            &mut accounting,
            [sample("a", 0, 0), unknown, sample("b", 350, 0)],
        );
        out.extend(accounting.finish(at(390), Some(mono(0))));
        assert_eq!(ranges(&out), vec![("a", 0, 290), ("b", 350, 390)]);
        assert_eq!(
            out.gaps,
            vec![Gap {
                start_unix: 1290,
                end_unix: 1350
            }]
        );
    }

    #[test]
    fn same_tick_foreground_evidence_survives_duplicate_callbacks_and_split_drains() {
        for split in [false, true] {
            let mut accounting = Accounting::new(300);
            let mut out = collect(
                &mut accounting,
                [sample("a", 0, 0), fg(Some("b"), 6, 6), fg(Some("b"), 6, 6)],
            );
            if split {
                out.extend(accounting.drain_ready(mono(8)));
            }
            accounting.push(sample("a", 6, 6));
            out.extend(accounting.finish(at(10), Some(mono(10))));
            assert_eq!(ranges(&out), vec![("a", 0, 6), ("b", 6, 10)]);
            assert!(out.gaps.is_empty());
        }
    }

    #[test]
    fn a_matching_foreground_after_sample_commit_cannot_undo_an_append_only_gap() {
        let mut accounting = Accounting::new(300);
        collect(&mut accounting, [sample("a", 0, 0), sample("b", 6, 6)]);
        let mut out = accounting.drain_ready(mono(8));
        accounting.push(fg(Some("b"), 6, 6));
        out.extend(accounting.finish(at(10), Some(mono(10))));
        assert_eq!(
            out.gaps,
            vec![Gap {
                start_unix: 1000,
                end_unix: 1006
            }]
        );
        assert_eq!(ranges(&out).last().copied(), Some(("b", 6, 10)));
    }

    #[test]
    fn unrelated_or_conflicting_pending_foregrounds_do_not_validate_a_sample() {
        for conflicting in [false, true] {
            let mut accounting = Accounting::new(300);
            accounting.push(sample("a", 0, 0));
            accounting.push(sample(if conflicting { "b" } else { "c" }, 6, 6));
            accounting.push(fg(Some("b"), 6, 6));
            if conflicting {
                accounting.push(fg(Some("c"), 6, 6));
            }
            let out = accounting.finish(at(10), Some(mono(10)));
            assert_eq!(
                out.gaps,
                vec![Gap {
                    start_unix: 1000,
                    end_unix: 1006
                }]
            );
        }
    }

    #[test]
    fn ambiguous_same_tick_samples_wait_for_a_later_reliable_window() {
        for conflicting_foregrounds in [false, true] {
            for recover in [false, true] {
                let mut accounting = Accounting::new(300);
                collect(&mut accounting, [sample("a", 0, 0), fg(Some("b"), 6, 6)]);
                if conflicting_foregrounds {
                    accounting.push(fg(Some("c"), 6, 6));
                }
                accounting.push(sample(
                    if conflicting_foregrounds { "a" } else { "c" },
                    6,
                    6,
                ));
                // Neither another failed lookup, a foreground callback nor a
                // second snapshot in the ambiguous tick can resume capture.
                collect(
                    &mut accounting,
                    [fg(None, 6, 6), fg(Some("d"), 6, 6), sample("d", 6, 6)],
                );
                if recover {
                    accounting.push(sample("d", 8, 8));
                }
                let out = accounting.finish(
                    at(if recover { 10 } else { 7 }),
                    Some(mono(if recover { 10 } else { 7 })),
                );
                assert_eq!(
                    out.gaps,
                    vec![Gap {
                        start_unix: 1006,
                        end_unix: if recover { 1008 } else { 1007 }
                    }]
                );
                assert_eq!(
                    ranges(&out),
                    if recover {
                        vec![("a", 0, 6), ("d", 8, 10)]
                    } else {
                        vec![("a", 0, 6)]
                    }
                );
            }
        }
    }

    #[test]
    fn contradictory_late_control_sample_and_overflow_revoke_suppression_evidence() {
        for case in 0..3 {
            let mut accounting = Accounting::new(300);
            let mut locked = sample("a", 5, 0);
            let ObservationKind::Sample { locked: flag, .. } = &mut locked.kind else {
                unreachable!()
            };
            *flag = true;
            collect(&mut accounting, [sample("a", 0, 0), locked]);
            let mut still_locked = sample("a", 20, 0);
            let ObservationKind::Sample { locked: flag, .. } = &mut still_locked.kind else {
                unreachable!()
            };
            *flag = true;
            accounting.push(still_locked);
            accounting.drain_ready(mono(22));
            let out = match case {
                0 => accounting.push(Observation {
                    at: at(10),
                    window: WindowIdentity::default(),
                    kind: ObservationKind::Lock(false),
                }),
                1 => accounting.push(sample("b", 10, 10)),
                _ => accounting.invalidate(at(10), at(20)),
            };
            assert_eq!(
                out.gaps,
                vec![Gap {
                    start_unix: 1010,
                    end_unix: 1020
                }]
            );
            assert!(accounting.suppressed_since.is_none());
        }
    }

    #[test]
    fn a_late_changed_sample_does_not_supply_a_foreground_transition_boundary() {
        let mut accounting = Accounting::new(300);
        collect(
            &mut accounting,
            [sample("a", 0, 0), title("c", "ignored", 500, 0)],
        );
        accounting.drain_ready(mono(502));
        let out = accounting.push(sample("b", 400, 299));
        assert_eq!(
            out.gaps,
            vec![Gap {
                start_unix: 1000,
                end_unix: 1500
            }]
        );
        assert!(out
            .segments
            .iter()
            .all(|segment| segment.app_basename == "a"
                && segment.start_unix >= 1000
                && segment.end_unix <= 1500));
    }

    #[test]
    fn a_late_unmatched_title_cannot_extend_the_old_foreground_prefix() {
        for matching_window in [false, true] {
            let mut accounting = Accounting::new(300);
            collect(
                &mut accounting,
                [sample("a", 0, 0), title("c", "ignored", 500, 0)],
            );
            accounting.drain_ready(mono(502));
            let name = if matching_window { "a" } else { "c" };
            let out = accounting.push(title(name, "late", 400, 299));
            assert_eq!(
                out.gaps,
                vec![Gap {
                    start_unix: if matching_window { 1400 } else { 1300 },
                    end_unix: 1500,
                }],
                "only a matching active identity can retain the existing prefix assumption"
            );
            assert_eq!(accounting.aggregator.input_seen(), Some(mono(299)));
            assert!(out
                .segments
                .iter()
                .all(|segment| segment.app_basename == "a" && segment.title.is_none()));
        }
    }

    #[test]
    fn renewed_late_input_preserves_the_newly_confirmed_idle_suffix() {
        for input in [200, 300] {
            let mut accounting = Accounting::new(300);
            collect(&mut accounting, [sample("a", 0, 0), sample("a", 600, 0)]);
            accounting.drain_ready(mono(602));
            let out = accounting.push(fg(Some("b"), 400, input));
            assert_eq!(
                out.gaps,
                vec![Gap {
                    start_unix: 1300,
                    end_unix: 1000 + input + 300,
                }]
            );
            assert_eq!(accounting.idle_since, Some(at(input + 300)));
            accounting.push(sample("a", 650, input));
            let repeated = accounting.drain_ready(mono(652));
            assert!(repeated.gaps.is_empty());
            assert!(repeated.segments.is_empty());
            let finished = accounting.finish(at(660), Some(mono(input)));
            assert!(finished.gaps.is_empty());
            assert!(finished.segments.is_empty());
        }
    }

    #[test]
    fn renewed_late_input_revokes_idle_proof_from_the_previous_deadline() {
        let mut accounting = Accounting::new(300);
        collect(&mut accounting, [sample("a", 0, 0), sample("a", 600, 0)]);
        accounting.drain_ready(mono(602));
        let out = accounting.push(fg(Some("b"), 400, 350));
        assert_eq!(
            out.gaps,
            vec![Gap {
                start_unix: 1300,
                end_unix: 1600
            }]
        );
        assert!(accounting.idle_since.is_none());
    }

    #[test]
    fn late_input_before_lock_marks_an_already_closed_truncated_prefix_unknown() {
        let mut accounting = Accounting::new(5);
        let lock = Observation {
            at: at(10),
            window: WindowIdentity::default(),
            kind: ObservationKind::Lock(true),
        };
        let mut still_locked = sample("a", 20, 0);
        let ObservationKind::Sample { locked, .. } = &mut still_locked.kind else {
            unreachable!()
        };
        *locked = true;
        collect(&mut accounting, [sample("a", 0, 0), lock, still_locked]);
        accounting.drain_ready(mono(22));
        let out = accounting.push(fg(Some("b"), 9, 4));
        assert_eq!(
            out.gaps,
            vec![Gap {
                start_unix: 1005,
                end_unix: 1010
            }]
        );
        assert_eq!(accounting.suppressed_since, Some(at(10)));
    }

    #[test]
    fn recovery_reuses_input_without_accepting_future_or_refreshing_stale_samples() {
        for invalid_input in [None, Some(mono(900))] {
            let mut accounting = Accounting::new(300);
            let mut unknown = fg(None, 290, 290);
            unknown.window = window("b");
            let mut recovery = sample("b", 350, 0);
            let ObservationKind::Sample { last_input, .. } = &mut recovery.kind else {
                unreachable!()
            };
            *last_input = invalid_input;
            let mut out = collect(
                &mut accounting,
                [sample("a", 0, 0), unknown, recovery, sample("b", 400, 0)],
            );
            out.extend(accounting.finish(at(650), Some(mono(0))));
            assert_eq!(
                ranges(&out),
                vec![("a", 0, 290), ("b", 350, 400), ("b", 400, 590)]
            );
            assert_eq!(
                out.gaps,
                vec![Gap {
                    start_unix: 1290,
                    end_unix: 1350
                }]
            );
        }
    }

    #[test]
    fn self_review_late_foreground_preserves_confirmed_lock_suspend_and_idle() {
        for state in 0..3 {
            let mut accounting = Accounting::new(if state == 2 { 5 } else { 300 });
            let excluded = |seconds| {
                let mut observation = sample("a", seconds, 0);
                let ObservationKind::Sample {
                    locked, suspended, ..
                } = &mut observation.kind
                else {
                    unreachable!()
                };
                *locked = state == 0;
                *suspended = state == 1;
                observation
            };
            collect(
                &mut accounting,
                [sample("a", 0, 0), excluded(5), excluded(20)],
            );
            let mut out = accounting.drain_ready(mono(22));
            out.extend(accounting.push(fg(Some("b"), 4, 0)));
            accounting.push(excluded(30));
            out.extend(accounting.finish(at(30), Some(mono(0))));
            assert_eq!(
                out.gaps,
                vec![Gap {
                    start_unix: 1004,
                    end_unix: 1005
                }],
                "state={state}"
            );
        }
    }

    #[test]
    fn delayed_background_title_cannot_move_a_foreground_boundary() {
        for foreground_delivered_first in [false, true] {
            let mut accounting = Accounting::new(300);
            accounting.push(sample("b", 0, 0));
            let title = Observation {
                window: window("a"),
                at: at(5),
                kind: ObservationKind::Title {
                    app: Some(app("a")),
                    last_input: Some(mono(5)),
                },
            };
            let foreground = fg(Some("a"), 6, 6);
            let events = if foreground_delivered_first {
                [foreground, title]
            } else {
                [title, foreground]
            };
            collect(&mut accounting, events);
            accounting.push(sample("a", 8, 8));
            let out = accounting.finish(at(10), Some(mono(10)));
            assert_eq!(ranges(&out), vec![("b", 0, 6), ("a", 6, 8), ("a", 8, 10)]);
            assert!(out.gaps.is_empty());
        }
    }

    #[test]
    fn changed_window_is_checked_before_afk_can_erase_its_previous_identity() {
        for (seconds, input, until) in [(350, 350, 350), (900, 500, 800), (900, 0, 300)] {
            for same_app in [false, true] {
                let mut accounting = Accounting::new(300);
                accounting.push(sample("a", 0, 0));
                accounting.drain_ready(mono(2));
                let mut changed = sample(if same_app { "a" } else { "b" }, seconds, input);
                changed.window.hwnd += 100;
                accounting.push(changed);
                let out = accounting.finish(at(seconds), Some(mono(input)));
                assert_eq!(
                    out.gaps,
                    vec![Gap {
                        start_unix: 1000,
                        end_unix: 1000 + until
                    }]
                );
                // Any append-only estimate for A is wholly covered by its
                // correction; the changed window is never backfilled.
                assert!(out
                    .segments
                    .iter()
                    .all(|s| s.start_unix >= 1000 && s.end_unix <= 1000 + until));
            }
        }
    }

    #[test]
    fn an_unresolved_new_window_still_disproves_the_previous_attribution() {
        for (seconds, input, until) in
            [(10, 0, 10), (350, 350, 350), (900, 500, 800), (900, 0, 300)]
        {
            let mut accounting = Accounting::new(300);
            accounting.push(sample("a", 0, 0));
            accounting.drain_ready(mono(2));
            let mut unknown = sample("b", seconds, input);
            let ObservationKind::Sample { app, .. } = &mut unknown.kind else {
                unreachable!()
            };
            *app = None; // Capture succeeded; executable lookup did not.
            accounting.push(unknown);
            let out = accounting.finish(at(seconds), Some(mono(input)));
            assert_eq!(
                out.gaps,
                vec![Gap {
                    start_unix: 1000,
                    end_unix: 1000 + until
                }]
            );
            assert!(out
                .segments
                .iter()
                .all(|s| s.app_basename == "a" && s.end_unix <= 1000 + until));
        }
    }

    #[test]
    fn rejected_titles_have_no_input_idle_gap_or_confirmation_side_effects() {
        for case in 0..7 {
            let mut accounting = Accounting::new(300);
            accounting.push(sample("a", 0, 0));
            let mut ignored = title("a", "background", 290, 290);
            match case {
                0 => ignored.window.hwnd += 1, // same process, another window
                1 => ignored.window.pid += 1,  // HWND has changed owner
                2 => ignored.window = WindowIdentity::default(),
                3 => ignored.window.pid = 0,
                _ => {
                    let ObservationKind::Title { app: observed, .. } = &mut ignored.kind else {
                        unreachable!()
                    };
                    match case {
                        4 => *observed = None,
                        5 => observed.as_mut().unwrap().path = "other-uwp-child".into(),
                        _ => observed.as_mut().unwrap().basename = "different.exe".into(),
                    }
                }
            }
            accounting.push(ignored);
            let no_change = accounting.drain_ready(mono(292));
            assert!(no_change.segments.is_empty() && no_change.gaps.is_empty());
            assert_eq!(accounting.last_confirmed, Some(at(0)));
            accounting.push(sample("a", 350, 0));
            let out = accounting.finish(at(350), Some(mono(0)));
            assert_eq!(ranges(&out), vec![("a", 0, 300)], "case={case}");
            assert!(out.gaps.is_empty());
            assert!(out.segments.iter().all(|s| s.title.is_none()));
        }
    }

    #[test]
    fn same_process_windows_cannot_exchange_delayed_titles() {
        let first = window("a");
        let second = WindowIdentity {
            hwnd: first.hwnd + 1,
            pid: first.pid,
        };
        let mut accounting = Accounting::new(300);
        accounting.push(sample("a", 0, 0));
        let mut changed = fg(Some("a"), 6, 6); // AppKey unchanged
        changed.window = second;
        let mut background = title("a", "background", 5, 5);
        background.window = second;
        // Deliver foreground first: the title must still be checked at t=5.
        collect(&mut accounting, [changed, background]);
        let mut legitimate = title("a", "foreground", 7, 7);
        legitimate.window = second;
        accounting.push(legitimate);
        let out = accounting.finish(at(10), Some(mono(10)));
        assert_eq!(ranges(&out), vec![("a", 0, 7), ("a", 7, 10)]);
        assert_eq!(out.segments[0].title, None);
        assert_eq!(out.segments[1].title.as_deref(), Some("foreground"));
        assert!(out.gaps.is_empty());
    }

    #[test]
    fn a_sample_with_the_same_app_but_a_new_window_corrects_and_reestablishes_identity() {
        let mut accounting = Accounting::new(300);
        accounting.push(sample("a", 0, 0));
        let mut changed = sample("a", 5, 5);
        changed.window.hwnd += 1;
        let second = changed.window;
        accounting.push(changed);
        let mut legitimate = title("a", "new window", 6, 6);
        legitimate.window = second;
        accounting.push(legitimate);
        accounting.push(title("a", "old window", 7, 7));
        let out = accounting.finish(at(10), Some(mono(10)));
        assert_eq!(
            out.gaps,
            vec![Gap {
                start_unix: 1000,
                end_unix: 1005
            }]
        );
        assert_eq!(
            out.segments.last().unwrap().title.as_deref(),
            Some("new window")
        );
        assert_eq!(out.segments.last().unwrap().start_unix, 1006);
        assert!(out
            .segments
            .iter()
            .all(|s| s.title.as_deref() != Some("old window")));
    }

    #[test]
    fn titles_never_recover_startup_idle_unknown_or_suppressed_capture() {
        for state in 0..6 {
            let mut accounting = Accounting::new(5);
            if state != 0 {
                accounting.push(sample("a", 0, 0));
                accounting.drain_ready(mono(2));
            }
            match state {
                1 | 2 => {
                    let kind = if state == 1 {
                        ObservationKind::Lock(true)
                    } else {
                        ObservationKind::Suspend(true)
                    };
                    accounting.push(Observation {
                        at: at(3),
                        window: WindowIdentity::default(),
                        kind,
                    });
                    let kind = if state == 1 {
                        ObservationKind::Lock(false)
                    } else {
                        ObservationKind::Suspend(false)
                    };
                    accounting.push(Observation {
                        at: at(4),
                        window: WindowIdentity::default(),
                        kind,
                    });
                }
                3 => {
                    accounting.push(fg(None, 3, 3));
                }
                4 => {
                    accounting.invalidate(at(3), at(4));
                }
                5 => {
                    accounting.push(sample("a", 5, 0));
                }
                _ => {}
            }
            accounting.drain_ready(mono(7));
            accounting.push(title("a", "must not resume", 6, 6));
            assert!(accounting.drain_ready(mono(8)).is_empty(), "state={state}");
            accounting.push(sample("a", 7, 7));
            let out = accounting.finish(at(9), Some(mono(7)));
            assert_eq!(ranges(&out), vec![("a", 7, 9)], "state={state}");
        }
    }

    #[test]
    fn late_title_corrects_history_instead_of_using_the_present_window() {
        let mut accounting = Accounting::new(300);
        collect(
            &mut accounting,
            [sample("a", 0, 0), fg(Some("b"), 6, 6), sample("b", 10, 10)],
        );
        accounting.drain_ready(mono(12));
        let correction = accounting.push(title("a", "historical", 5, 5));
        assert_eq!(
            correction.gaps,
            vec![Gap {
                start_unix: 1005,
                end_unix: 1010
            }]
        );
        accounting.push(title("b", "cannot restore identity", 11, 11));
        accounting.push(sample("b", 12, 12));
        let out = accounting.finish(at(14), Some(mono(14)));
        assert_eq!(
            out.gaps,
            vec![Gap {
                start_unix: 1010,
                end_unix: 1012
            }]
        );
        assert_eq!(ranges(&out), vec![("b", 12, 14)]);
    }

    #[test]
    fn equal_timestamp_title_and_foreground_orders_never_move_the_boundary() {
        for title_first in [false, true] {
            let mut accounting = Accounting::new(300);
            accounting.push(sample("b", 0, 0));
            let foreground = fg(Some("a"), 6, 6);
            let title = title("a", "edited", 6, 6);
            collect(
                &mut accounting,
                if title_first {
                    [title, foreground]
                } else {
                    [foreground, title]
                },
            );
            let out = accounting.finish(at(10), Some(mono(10)));
            assert_eq!(ranges(&out), vec![("b", 0, 6), ("a", 6, 10)]);
            assert!(out.gaps.is_empty());
        }
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
                        window: window("a"),
                        at: at(350),
                        kind: ObservationKind::Title {
                            app: Some(AppKey {
                                title: Some("edited".into()),
                                ..app("a")
                            }),
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
                if ending == 2 {
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
            window: window("a"),
            at: at(306),
            kind: ObservationKind::Title {
                app: Some(titled.clone()),
                last_input: Some(mono(304)),
            },
        };
        let later = Observation {
            window: window("a"),
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
                    window: WindowIdentity::default(),
                    at: at(5),
                    kind: ObservationKind::Lock(true),
                },
                Observation {
                    window: WindowIdentity::default(),
                    at: at(6),
                    kind: ObservationKind::Suspend(true),
                },
                Observation {
                    window: WindowIdentity::default(),
                    at: at(100),
                    kind: ObservationKind::Suspend(false),
                },
                fg(Some("b"), 101, 101),
                Observation {
                    window: WindowIdentity::default(),
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
    fn unsettled_capture_covers_activity_before_delayed_consumer_start() {
        for checkpoint in [false, true] {
            let mut accounting = Accounting::new(300);
            // Main published A before the consumer was scheduled at t=3.
            accounting.push(sample("a", 0, 0));
            let mut out = accounting.drain_ready(mono(3));
            if checkpoint {
                accounting.push(sample("a", 2, 2));
                out.extend(accounting.drain_ready(mono(4)));
                assert_eq!(ranges(&out), vec![("a", 0, 2)]);
            }
            // An admitted B@1 callback is still stuck at shutdown. A final
            // A snapshot cannot expose the intervening A -> B -> A switch.
            accounting.push(sample("a", 4, 4));
            out.extend(accounting.invalidate_session(at(3), at(4)));
            out.extend(accounting.finish(at(4), Some(mono(4))));
            assert_eq!(
                out.gaps,
                vec![Gap {
                    start_unix: 1000,
                    end_unix: 1004,
                }],
                "checkpoint={checkpoint}"
            );
            assert!(out
                .segments
                .iter()
                .all(|segment| out.gaps.iter().any(|gap| {
                    gap.start_unix <= segment.start_unix && gap.end_unix >= segment.end_unix
                })));
            assert!(accounting.invalidate_session(at(0), at(5)).is_empty());
        }
    }

    #[test]
    fn unsettled_capture_keeps_earlier_fallback_and_pending_boundaries() {
        for state in 0..3 {
            let mut accounting = Accounting::new(300);
            if state == 1 {
                accounting.push(sample("a", 2, 2));
                accounting.drain_ready(mono(4));
            } else if state == 2 {
                accounting.push(sample("a", 0, 0));
            }
            let out = accounting.invalidate_session(at(1), at(4));
            assert_eq!(
                out.gaps,
                vec![Gap {
                    start_unix: if state == 2 { 1000 } else { 1001 },
                    end_unix: 1004,
                }],
                "state={state}"
            );
        }
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
    fn sample_title_changes_preserve_application_time() {
        for (old_title, new_title) in [
            (None, Some("new")),
            (Some("old"), None),
            (Some("old"), Some("new")),
        ] {
            let mut accounting = Accounting::new(300);
            let mut initial = sample("a", 0, 0);
            let mut confirmed = fg(Some("a"), 10, 10);
            for observation in [&mut initial, &mut confirmed] {
                match &mut observation.kind {
                    ObservationKind::Sample { app: Some(app), .. }
                    | ObservationKind::Foreground { app: Some(app), .. } => {
                        app.title = old_title.map(str::to_owned);
                    }
                    _ => unreachable!(),
                }
            }
            let mut next = sample("a", 30, 30);
            if let ObservationKind::Sample { app: Some(app), .. } = &mut next.kind {
                app.title = new_title.map(str::to_owned);
            }
            collect(&mut accounting, [initial, confirmed, next]);
            let out = accounting.finish(at(40), Some(mono(40)));
            assert!(out.gaps.is_empty(), "{old_title:?} -> {new_title:?}");
            assert_eq!(ranges(&out), vec![("a", 0, 30), ("a", 30, 40)]);
            assert_eq!(out.segments[0].title.as_deref(), old_title);
            assert_eq!(out.segments[1].title.as_deref(), new_title);
        }
    }

    #[test]
    fn same_tick_foreground_and_sample_can_read_different_titles() {
        for foreground_first in [false, true] {
            let mut accounting = Accounting::new(300);
            accounting.push(sample("a", 0, 0));
            let mut snapshot = sample("b", 10, 10);
            if let ObservationKind::Sample { app: Some(app), .. } = &mut snapshot.kind {
                app.title = Some("sample title".into());
            }
            let mut transition = fg(Some("b"), 10, 10);
            if let ObservationKind::Foreground { app: Some(app), .. } = &mut transition.kind {
                app.title = Some("foreground title".into());
            }
            collect(
                &mut accounting,
                if foreground_first {
                    [transition, snapshot]
                } else {
                    [snapshot, transition]
                },
            );
            let out = accounting.finish(at(40), Some(mono(40)));
            assert!(out.gaps.is_empty(), "foreground_first={foreground_first}");
            assert_eq!(ranges(&out), vec![("a", 0, 10), ("b", 10, 40)]);
        }
    }

    #[test]
    fn title_tolerance_still_corrects_real_window_and_application_changes() {
        for change in 0..3 {
            let mut accounting = Accounting::new(300);
            let mut changed = sample("a", 30, 30);
            match change {
                0 => changed.window.hwnd += 1,
                1 => changed.window.pid += 1,
                _ => {
                    if let ObservationKind::Sample { app: Some(app), .. } = &mut changed.kind {
                        app.path = "another.exe".into();
                        app.basename = "another.exe".into();
                    }
                }
            }
            collect(
                &mut accounting,
                [sample("a", 0, 0), fg(Some("a"), 10, 10), changed],
            );
            let out = accounting.finish(at(40), Some(mono(40)));
            assert_eq!(
                out.gaps,
                vec![Gap {
                    start_unix: 1010,
                    end_unix: 1030
                }],
                "change={change}"
            );
        }
    }

    #[test]
    fn late_title_metadata_does_not_revoke_an_earlier_application_confirmation() {
        for changed_application in [false, true] {
            let mut accounting = Accounting::new(300);
            collect(
                &mut accounting,
                [
                    sample("a", 0, 0),
                    fg(Some("a"), 10, 10),
                    title("c", "ignored", 40, 40),
                ],
            );
            accounting.drain_ready(mono(42));
            let mut late = sample("a", 20, 20);
            if let ObservationKind::Sample { app: Some(app), .. } = &mut late.kind {
                app.title = Some("new title".into());
                if changed_application {
                    app.path = "another.exe".into();
                    app.basename = "another.exe".into();
                }
            }
            let out = accounting.push(late);
            // Late observations retain the ordinary event-time correction,
            // but title metadata alone cannot disprove an earlier app prefix.
            assert_eq!(
                out.gaps,
                vec![Gap {
                    start_unix: if changed_application { 1010 } else { 1020 },
                    end_unix: 1040,
                }]
            );
        }
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
            window: window("b"),
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
            window: window("b"),
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
            window: WindowIdentity::default(),
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
            window: WindowIdentity::default(),
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
            window: WindowIdentity::default(),
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
    fn renewed_input_can_disprove_an_earlier_idle_observation() {
        for input in [200, 540, 600, 800] {
            for ending in 0..5 {
                let mut accounting = Accounting::new(300);
                collect(&mut accounting, [sample("a", 0, 0), sample("a", 600, 0)]);
                let mut out = accounting.drain_ready(mono(602));
                if ending != 0 {
                    let mut observation = if ending == 1 {
                        fg(Some("a"), 1200, input)
                    } else {
                        sample("a", 1200, input)
                    };
                    if let ObservationKind::Sample {
                        locked, suspended, ..
                    } = &mut observation.kind
                    {
                        *locked = ending == 3;
                        *suspended = ending == 4;
                    }
                    out.extend(accounting.push(observation));
                }
                out.extend(accounting.finish(at(1200), Some(mono(input))));
                assert_eq!(ranges(&out), vec![("a", 0, 300)]);
                assert_eq!(
                    out.gaps,
                    vec![Gap {
                        start_unix: 1000 + if input <= 600 { 300 } else { 600 },
                        end_unix: 1000 + input + 300,
                    }],
                    "input={input} ending={ending}"
                );
            }
        }
    }

    #[test]
    fn renewed_input_corrects_idle_before_recovery_and_keeps_waiting_for_a_sample() {
        for foreground in [false, true] {
            let mut accounting = Accounting::new(300);
            collect(&mut accounting, [sample("a", 0, 0), sample("a", 600, 0)]);
            let mut out = accounting.drain_ready(mono(602));
            out.extend(accounting.push(if foreground {
                fg(Some("b"), 700, 540)
            } else {
                sample("b", 700, 540)
            }));
            out.extend(accounting.drain_ready(mono(702)));
            assert_eq!(
                out.gaps,
                vec![Gap {
                    start_unix: 1300,
                    end_unix: 1700
                }]
            );
            accounting.push(sample("b", 720, 540));
            out.extend(accounting.finish(at(730), Some(mono(540))));
            assert_eq!(
                out.segments
                    .iter()
                    .filter(|segment| segment.app_basename == "b")
                    .map(Segment::duration)
                    .sum::<u64>(),
                if foreground { 10 } else { 30 }
            );
            if foreground {
                assert_eq!(
                    out.gaps[1],
                    Gap {
                        start_unix: 1700,
                        end_unix: 1720
                    }
                );
            }
        }
        let mut accounting = Accounting::new(300);
        collect(&mut accounting, [sample("a", 0, 0), sample("a", 600, 0)]);
        let out = accounting.finish(at(700), Some(mono(540)));
        assert_eq!(
            out.gaps,
            vec![Gap {
                start_unix: 1300,
                end_unix: 1700
            }]
        );
    }

    #[test]
    fn renewed_input_before_suppression_corrects_only_the_unproven_prefix() {
        for input in [4, 9] {
            for suspended in [false, true] {
                for finish_only in [false, true] {
                    let mut accounting = Accounting::new(5);
                    let mut suppression = sample("a", 10, 0);
                    if let ObservationKind::Sample {
                        locked,
                        suspended: sleeping,
                        ..
                    } = &mut suppression.kind
                    {
                        *locked = !suspended;
                        *sleeping = suspended;
                    }
                    collect(&mut accounting, [sample("a", 0, 0), suppression.clone()]);
                    let mut out = accounting.drain_ready(mono(12));
                    if !finish_only {
                        suppression.at = at(20);
                        if let ObservationKind::Sample { last_input, .. } = &mut suppression.kind {
                            *last_input = Some(mono(input));
                        }
                        out.extend(accounting.push(suppression));
                    }
                    out.extend(accounting.finish(at(20), Some(mono(input))));
                    assert_eq!(ranges(&out), vec![("a", 0, 5)]);
                    assert_eq!(
                        out.gaps,
                        vec![Gap {
                            start_unix: 1005,
                            end_unix: 1000 + (input + 5).min(10)
                        }],
                        "input={input} suspended={suspended} finish_only={finish_only}"
                    );
                }
            }
        }
    }

    #[test]
    fn retrospective_idle_correction_starts_at_capture_and_is_not_repeated() {
        let mut accounting = Accounting::new(300);
        accounting.push(sample("a", 600, 0));
        accounting.drain_ready(mono(602));
        let mut corrected = sample("a", 1200, 540);
        corrected.at.wall_unix_millis = 1;
        accounting.push(corrected);
        let out = accounting.drain_ready(mono(1202));
        assert!(out.segments.is_empty());
        assert_eq!(
            out.gaps,
            vec![Gap {
                start_unix: 1600,
                end_unix: 1840
            }]
        );
        for (seconds, input) in [(1210, 540), (1220, 0)] {
            let mut repeated = sample("a", seconds, input);
            repeated.at.wall_unix_millis = u64::MAX;
            accounting.push(repeated);
            let out = accounting.drain_ready(mono(seconds + 2));
            assert!(out.gaps.is_empty());
            assert!(out.segments.is_empty());
        }
        let out = accounting.finish(at(1230), Some(mono(540)));
        assert!(out.gaps.is_empty());
        assert!(out.segments.is_empty());
    }

    #[test]
    fn invalid_input_cannot_retroactively_disprove_idle() {
        for input in [None, Some(mono(0)), Some(mono(1300))] {
            let mut accounting = Accounting::new(300);
            collect(&mut accounting, [sample("a", 0, 0), sample("a", 600, 0)]);
            let mut out = accounting.drain_ready(mono(602));
            let mut next = sample("a", 1200, 0);
            if let ObservationKind::Sample { last_input, .. } = &mut next.kind {
                *last_input = input;
            }
            accounting.push(next);
            out.extend(accounting.finish(at(1200), input));
            assert_eq!(ranges(&out), vec![("a", 0, 300)]);
            // A failed/future observation can make the interval after the
            // previous sample unknown, but cannot refute earlier idle proof.
            assert!(out.gaps.iter().all(|gap| gap.start_unix >= 1600));
            if input == Some(mono(0)) {
                assert!(out.gaps.is_empty());
            }
        }
    }

    #[test]
    fn locking_while_still_idle_does_not_invent_missing_activity() {
        for renewed_input in [0, 500] {
            let mut accounting = Accounting::new(300);
            let lock = Observation {
                window: WindowIdentity::default(),
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
                        window: window("a"),
                        at: at(900),
                        kind: ObservationKind::Sample {
                            app: (ending != 5).then(|| app("a")),
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
