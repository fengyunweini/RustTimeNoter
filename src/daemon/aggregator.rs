//! Ordered segment aggregation. Monotonic time determines durations; wall time
//! labels persisted records. The accounting layer orders and validates events.
use crate::storage::Segment;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppKey {
    pub path: String,
    pub basename: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MonoTime(pub u64);

impl MonoTime {
    pub const fn from_millis(milliseconds: u64) -> Self {
        Self(milliseconds)
    }
    pub fn elapsed_millis_since(self, earlier: Self) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
    pub fn saturating_add_millis(self, milliseconds: u64) -> Self {
        Self(self.0.saturating_add(milliseconds))
    }
    pub fn min(self, other: Self) -> Self {
        Self(self.0.min(other.0))
    }
    pub fn max(self, other: Self) -> Self {
        Self(self.0.max(other.0))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimePoint {
    pub monotonic: MonoTime,
    pub wall_unix_millis: u64,
}

impl TimePoint {
    pub const fn new(monotonic_millis: u64, wall_unix_millis: u64) -> Self {
        Self {
            monotonic: MonoTime(monotonic_millis),
            wall_unix_millis,
        }
    }

    pub fn project(self, monotonic: MonoTime) -> Self {
        let wall_unix_millis = if monotonic >= self.monotonic {
            self.wall_unix_millis
                .saturating_add(monotonic.elapsed_millis_since(self.monotonic))
        } else {
            self.wall_unix_millis
                .saturating_sub(self.monotonic.elapsed_millis_since(monotonic))
        };
        Self {
            monotonic,
            wall_unix_millis,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Foreground {
        app: AppKey,
        at: TimePoint,
        last_input: MonoTime,
    },
    IdleTick {
        at: TimePoint,
        last_input: MonoTime,
    },
    Checkpoint {
        at: TimePoint,
        last_input: MonoTime,
    },
    SessionLock {
        at: TimePoint,
    },
    SessionUnlock {
        at: TimePoint,
    },
    Suspend {
        at: TimePoint,
    },
    Resume {
        at: TimePoint,
    },
    Shutdown {
        at: TimePoint,
    },
}

#[derive(Debug)]
struct Active {
    app: AppKey,
    started: TimePoint,
    last_input: MonoTime,
}

#[derive(Debug)]
pub struct Aggregator {
    afk_millis: u64,
    active: Option<Active>,
    // Input belongs to the session, not to one resolved foreground segment.
    // A gap/reset must not make a later regressing OS sample erase evidence.
    input_seen: Option<MonoTime>,
    locked: bool,
    suspended: bool,
}

impl Aggregator {
    pub fn new(afk_threshold_secs: u64) -> Self {
        Self {
            afk_millis: afk_threshold_secs.max(1).saturating_mul(1_000),
            active: None,
            input_seen: None,
            locked: false,
            suspended: false,
        }
    }

    pub fn handle(&mut self, event: Event) -> Vec<Segment> {
        match event {
            Event::Foreground {
                app,
                at,
                last_input,
            } => self.observe_foreground(app, at, last_input),
            Event::IdleTick { at, last_input } => self.observe_input(at, last_input),
            Event::Checkpoint { at, last_input } => {
                let mut output = self.observe_input(at, last_input);
                output.extend(self.checkpoint(at));
                output
            }
            Event::SessionLock { at } => self.set_suppression(at, true, self.suspended),
            Event::SessionUnlock { at } => self.set_suppression(at, false, self.suspended),
            Event::Suspend { at } => self.set_suppression(at, self.locked, true),
            Event::Resume { at } => self.set_suppression(at, self.locked, false),
            Event::Shutdown { at } => self.close(at),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.is_some() && !self.is_suppressed()
    }
    pub fn is_suppressed(&self) -> bool {
        self.locked || self.suspended
    }
    pub fn current_app(&self) -> Option<&AppKey> {
        self.active.as_ref().map(|active| &active.app)
    }

    /// A delayed callback may have no causal new input sample. Previously
    /// confirmed input still proves activity through its original deadline;
    /// reusing it here does not refresh or extend that deadline.
    pub fn confirmed_input(&self, at: TimePoint) -> Option<MonoTime> {
        self.input_seen.filter(|input| {
            *input <= at.monotonic && at.monotonic.elapsed_millis_since(*input) <= self.afk_millis
        })
    }

    pub(crate) fn input_seen(&self) -> Option<MonoTime> {
        self.input_seen
    }

    pub fn confirmed_through(&self) -> Option<MonoTime> {
        self.active
            .as_ref()
            .map(|active| active.last_input.saturating_add_millis(self.afk_millis))
    }

    /// GetLastInputInfo can return a timestamp older than a previous reading.
    /// Retain a causal input already witnessed by this session without
    /// refreshing its deadline or hiding a failed/future observation.
    pub(crate) fn monotonic_input(
        &self,
        at: TimePoint,
        observed: Option<MonoTime>,
    ) -> Option<MonoTime> {
        observed
            .filter(|input| *input <= at.monotonic)
            .map(|input| {
                self.input_seen
                    .filter(|seen| *seen <= at.monotonic)
                    .map_or(input, |seen| input.max(seen))
            })
    }

    /// Remember causal input without attributing activity to any window.
    pub(crate) fn remember_input(&mut self, at: TimePoint, input: MonoTime) {
        if input <= at.monotonic {
            self.input_seen = Some(self.input_seen.map_or(input, |seen| seen.max(input)));
        }
    }

    /// Only a causal OS input observation changes the idle deadline.
    pub fn observe_input(&mut self, at: TimePoint, last_input: MonoTime) -> Vec<Segment> {
        let Some(last_input) = self.monotonic_input(at, Some(last_input)) else {
            return Vec::new();
        };
        self.remember_input(at, last_input);
        let Some(active) = self.active.as_mut() else {
            return Vec::new();
        };
        active.last_input = last_input;
        if at.monotonic.elapsed_millis_since(last_input) >= self.afk_millis {
            return self.close(at);
        }
        Vec::new()
    }

    pub fn observe_foreground(
        &mut self,
        app: AppKey,
        at: TimePoint,
        last_input: MonoTime,
    ) -> Vec<Segment> {
        if self.is_suppressed() {
            return Vec::new();
        }
        let Some(last_input) = self.monotonic_input(at, Some(last_input)) else {
            return Vec::new();
        };
        let output = self.observe_input(at, last_input);
        if at.monotonic.elapsed_millis_since(last_input) >= self.afk_millis {
            return output;
        }
        if self.current_app() == Some(&app) {
            return output;
        }
        let started = self
            .active
            .as_ref()
            .map(|active| active.started.project(at.monotonic))
            .unwrap_or(at);
        // Reaching this branch means observe_input did not close for AFK, so
        // it produced no segment. Transfer the closing batch without a copy.
        let output = self.close(at);
        self.active = Some(Active {
            app,
            started,
            last_input,
        });
        output
    }

    pub fn set_suppression(
        &mut self,
        at: TimePoint,
        locked: bool,
        suspended: bool,
    ) -> Vec<Segment> {
        let output = if locked || suspended {
            self.close(at)
        } else {
            Vec::new()
        };
        self.locked = locked;
        self.suspended = suspended;
        output
    }

    /// Persist only through this already-ordered observation, preserving the
    /// fractional second remainder in the continuation.
    pub fn checkpoint(&mut self, at: TimePoint) -> Vec<Segment> {
        let Some(active) = self.active.as_ref() else {
            return Vec::new();
        };
        let end = at
            .monotonic
            .min(active.last_input.saturating_add_millis(self.afk_millis));
        if end < at.monotonic {
            return self.close(at);
        }
        let Some(segment) = Self::segment(active, end) else {
            return Vec::new();
        };
        let active = self.active.as_mut().expect("checked above");
        active.started = active.started.project(end);
        vec![segment]
    }

    /// Closing without a fresh sample never extends past the last confirmed
    /// input deadline. Callers must supply events in monotonic order.
    pub fn close(&mut self, at: TimePoint) -> Vec<Segment> {
        let Some(active) = self.active.take() else {
            return Vec::new();
        };
        let end = at
            .monotonic
            .min(active.last_input.saturating_add_millis(self.afk_millis));
        let Some((start_unix, end_unix)) = Self::segment_bounds(&active, end) else {
            return Vec::new();
        };
        // A closed segment owns the strings already. Only checkpoints need
        // copies because they retain an active continuation of the same app.
        vec![Segment {
            app_path: active.app.path,
            app_basename: active.app.basename,
            title: active.app.title,
            start_unix,
            end_unix,
        }]
    }

    pub fn reset(&mut self) {
        self.active = None;
    }

    fn segment(active: &Active, end: MonoTime) -> Option<Segment> {
        let (start_unix, end_unix) = Self::segment_bounds(active, end)?;
        Some(Segment {
            app_path: active.app.path.clone(),
            app_basename: active.app.basename.clone(),
            title: active.app.title.clone(),
            start_unix,
            end_unix,
        })
    }

    fn segment_bounds(active: &Active, end: MonoTime) -> Option<(u64, u64)> {
        if end <= active.started.monotonic {
            return None;
        }
        let start_unix = active.started.wall_unix_millis / 1_000;
        let end_unix = active.started.project(end).wall_unix_millis / 1_000;
        (end_unix > start_unix).then_some((start_unix, end_unix))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn app(name: &str) -> AppKey {
        AppKey {
            path: name.into(),
            basename: name.into(),
            title: None,
        }
    }
    fn at(seconds: u64) -> TimePoint {
        TimePoint::new(seconds * 1_000, 1_000_000 + seconds * 1_000)
    }
    fn mono(seconds: u64) -> MonoTime {
        MonoTime(seconds * 1_000)
    }

    #[test]
    fn afk_uses_real_input_even_when_it_predates_a_segment() {
        let mut agg = Aggregator::new(300);
        agg.observe_foreground(app("a"), at(0), mono(0));
        let mut out = agg.observe_foreground(app("b"), at(250), mono(0));
        out.extend(agg.observe_input(at(310), mono(0)));
        assert_eq!(out.iter().map(Segment::duration).sum::<u64>(), 300);
        assert!(!agg.is_active());
    }

    #[test]
    fn lock_and_suspend_have_independent_lifetimes() {
        let mut agg = Aggregator::new(300);
        agg.observe_foreground(app("a"), at(0), mono(0));
        assert_eq!(
            agg.handle(Event::SessionLock { at: at(5) })[0].duration(),
            5
        );
        agg.handle(Event::Suspend { at: at(6) });
        agg.handle(Event::Resume { at: at(100) });
        assert!(agg.is_suppressed());
        agg.observe_foreground(app("b"), at(101), mono(100));
        assert!(!agg.is_active());
        agg.handle(Event::SessionUnlock { at: at(102) });
        assert!(!agg.is_suppressed());
        assert!(!agg.is_active());
    }

    #[test]
    fn checkpoints_keep_fractional_remainder_and_monotonic_duration() {
        let mut agg = Aggregator::new(300);
        agg.observe_foreground(app("a"), TimePoint::new(0, 1_000_200), mono(0));
        assert!(agg.checkpoint(TimePoint::new(200, 99_000_000)).is_empty());
        let mut out = agg.checkpoint(TimePoint::new(1_100, 1));
        out.extend(agg.checkpoint(TimePoint::new(1_900, 999_000_000)));
        out.extend(agg.close(TimePoint::new(2_900, 0)));
        assert_eq!(out.iter().map(Segment::duration).sum::<u64>(), 3);
        assert!(out
            .windows(2)
            .all(|pair| pair[0].end_unix == pair[1].start_unix));
    }

    #[test]
    fn title_transition_accepts_a_real_input_without_using_title_as_input() {
        let mut agg = Aggregator::new(300);
        agg.observe_foreground(app("a"), at(0), mono(0));
        agg.observe_input(at(300), mono(5));
        let mut titled = app("a");
        titled.title = Some("edited".into());
        let mut out = agg.observe_foreground(titled, at(306), mono(304));
        out.extend(agg.observe_input(at(330), mono(329)));
        out.extend(agg.close(at(340)));
        assert_eq!(out.iter().map(Segment::duration).sum::<u64>(), 340);
    }
}
