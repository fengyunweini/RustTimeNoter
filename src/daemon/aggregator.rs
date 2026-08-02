//! Segment aggregation state machine. This module deliberately has no Windows
//! dependencies so the runtime time model can be tested deterministically.
//!
//! Elapsed time and AFK deadlines use [`MonoTime`]. Wall time is only an
//! anchor for persisted Unix timestamps. While an activity chain is live, wall
//! clock corrections are ignored: each next segment starts at the previous
//! segment's projected end. Once tracking is inactive, the next foreground
//! event establishes a fresh wall-clock anchor.

use crate::storage::Segment;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppKey {
    pub path: String,
    pub basename: String,
    pub title: Option<String>,
}

/// Milliseconds elapsed since Windows boot.
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

/// One observation of the monotonic clock and wall clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimePoint {
    pub monotonic: MonoTime,
    pub wall_unix_millis: u64,
}

impl TimePoint {
    pub const fn new(monotonic_millis: u64, wall_unix_millis: u64) -> Self {
        Self {
            monotonic: MonoTime::from_millis(monotonic_millis),
            wall_unix_millis,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    /// Foreground or foreground-title transition.
    ///
    /// `last_input` is the OS-reported last real input on the same monotonic
    /// timeline. It may legitimately predate this segment.
    Foreground {
        app: AppKey,
        at: TimePoint,
        last_input: MonoTime,
    },
    IdleTick {
        at: TimePoint,
        last_input: MonoTime,
    },
    /// Persist progress for the current app, then immediately continue it.
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

#[derive(Debug, Clone)]
struct Active {
    app: AppKey,
    started: TimePoint,
    /// Last confirmed keyboard/mouse input. This is global activity state, not
    /// a segment start, so it is allowed to be earlier than `started`.
    last_input: MonoTime,
}

impl Active {
    fn projected_wall_at(&self, monotonic: MonoTime) -> u64 {
        self.started
            .wall_unix_millis
            .saturating_add(monotonic.elapsed_millis_since(self.started.monotonic))
    }
}

#[derive(Debug, Default)]
struct SuppressionReasons {
    session_locked: bool,
    suspended: bool,
}

impl SuppressionReasons {
    fn any(&self) -> bool {
        self.session_locked || self.suspended
    }
}

#[derive(Debug)]
pub struct Aggregator {
    afk_threshold: u64,
    active: Option<Active>,
    suppression: SuppressionReasons,
}

impl Aggregator {
    pub fn new(afk_threshold_secs: u64) -> Self {
        Self {
            afk_threshold: afk_threshold_secs.max(1).saturating_mul(1_000),
            active: None,
            suppression: SuppressionReasons::default(),
        }
    }

    /// Process one event. Most events emit zero or one segment.
    pub fn handle(&mut self, event: Event) -> Vec<Segment> {
        match event {
            Event::Foreground {
                app,
                at,
                last_input,
            } => self.handle_foreground(app, at, Some(last_input)),
            Event::IdleTick { at, last_input } => {
                self.update_idle_and_maybe_close(at, Some(last_input))
            }
            Event::Checkpoint { at, last_input } => self.checkpoint(at, Some(last_input)),
            Event::SessionLock { at } => {
                let output = self.close_active(at.monotonic).into_iter().collect();
                self.suppression.session_locked = true;
                output
            }
            Event::SessionUnlock { at: _ } => {
                self.suppression.session_locked = false;
                Vec::new()
            }
            Event::Suspend { at } => {
                let output = self.close_active(at.monotonic).into_iter().collect();
                self.suppression.suspended = true;
                output
            }
            Event::Resume { at: _ } => {
                self.suppression.suspended = false;
                Vec::new()
            }
            Event::Shutdown { at } => self.close_active(at.monotonic).into_iter().collect(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.is_some() && !self.suppression.any()
    }

    pub fn is_suppressed(&self) -> bool {
        self.suppression.any()
    }

    pub(crate) fn handle_observed_idle(
        &mut self,
        at: TimePoint,
        last_input: Option<MonoTime>,
    ) -> Vec<Segment> {
        self.update_idle_and_maybe_close(at, last_input)
    }

    pub(crate) fn handle_observed_checkpoint(
        &mut self,
        at: TimePoint,
        last_input: Option<MonoTime>,
    ) -> Vec<Segment> {
        self.checkpoint(at, last_input)
    }

    /// Stop attributing time when a real foreground transition cannot be
    /// resolved. A later resolvable foreground starts a fresh chain.
    pub(crate) fn close_for_gap(&mut self, at: TimePoint) -> Vec<Segment> {
        self.close_active(at.monotonic).into_iter().collect()
    }

    /// Handle a callback-time foreground observation. `None` means the
    /// callback was delayed long enough to observe input newer than the event;
    /// in that case an active segment retains its previous real input.
    pub(crate) fn handle_observed_foreground(
        &mut self,
        app: AppKey,
        at: TimePoint,
        last_input: Option<MonoTime>,
    ) -> Vec<Segment> {
        self.handle_foreground(app, at, last_input)
    }

    fn handle_foreground(
        &mut self,
        app: AppKey,
        at: TimePoint,
        last_input: Option<MonoTime>,
    ) -> Vec<Segment> {
        if self.suppression.any() {
            return Vec::new();
        }

        let last_input = match (last_input, self.active.as_mut()) {
            (Some(last_input), active) => {
                let last_input = last_input.min(at.monotonic);
                if let Some(active) = active {
                    // Trust a causal OS observation even if it is earlier than
                    // the prior value. Foreground/title is never itself input.
                    active.last_input = last_input;
                }
                last_input
            }
            (None, Some(active)) => active.last_input.min(at.monotonic),
            // Starting from an unknown input time would implicitly treat the
            // segment start as input and let a delayed focus event defeat AFK.
            (None, None) => return Vec::new(),
        };

        if at.monotonic.elapsed_millis_since(last_input) >= self.afk_threshold {
            let deadline = last_input
                .saturating_add_millis(self.afk_threshold)
                .min(at.monotonic);
            return self.close_active(deadline).into_iter().collect();
        }

        let continued_start = self.active.as_ref().map(|active| {
            let monotonic = at.monotonic.max(active.started.monotonic);
            TimePoint {
                monotonic,
                wall_unix_millis: active.projected_wall_at(monotonic),
            }
        });
        let output = self.close_active(at.monotonic).into_iter().collect();
        self.active = Some(Active {
            app,
            started: continued_start.unwrap_or(at),
            last_input,
        });
        output
    }

    fn update_idle_and_maybe_close(
        &mut self,
        at: TimePoint,
        last_input: Option<MonoTime>,
    ) -> Vec<Segment> {
        let Some(active) = &mut self.active else {
            return Vec::new();
        };
        let last_input = if let Some(last_input) = last_input {
            let last_input = last_input.min(at.monotonic);
            active.last_input = last_input;
            last_input
        } else {
            active.last_input.min(at.monotonic)
        };
        if at.monotonic.elapsed_millis_since(last_input) < self.afk_threshold {
            return Vec::new();
        }

        let deadline = last_input
            .saturating_add_millis(self.afk_threshold)
            .min(at.monotonic);
        self.close_active(deadline).into_iter().collect()
    }

    fn checkpoint(&mut self, at: TimePoint, last_input: Option<MonoTime>) -> Vec<Segment> {
        if self.suppression.any() {
            return Vec::new();
        }

        let Some(active) = &mut self.active else {
            return Vec::new();
        };
        let last_input = if let Some(last_input) = last_input {
            let last_input = last_input.min(at.monotonic);
            active.last_input = last_input;
            last_input
        } else {
            active.last_input.min(at.monotonic)
        };
        if at.monotonic.elapsed_millis_since(last_input) >= self.afk_threshold {
            let deadline = last_input
                .saturating_add_millis(self.afk_threshold)
                .min(at.monotonic);
            return self.close_active(deadline).into_iter().collect();
        }

        self.checkpoint_active(at.monotonic).into_iter().collect()
    }

    fn close_active(&mut self, requested_end: MonoTime) -> Option<Segment> {
        let active = self.active.take()?;
        let end_mono = requested_end.max(active.started.monotonic);
        let end_unix = active.projected_wall_at(end_mono);
        if end_unix <= active.started.wall_unix_millis {
            return None;
        }
        let start_unix = active.started.wall_unix_millis / 1_000;
        let end_unix = end_unix / 1_000;
        if end_unix <= start_unix {
            return None;
        }
        Some(Segment {
            app_path: active.app.path,
            app_basename: active.app.basename,
            title: active.app.title,
            start_unix,
            end_unix,
        })
    }

    fn checkpoint_active(&mut self, requested_end: MonoTime) -> Option<Segment> {
        let active = self.active.as_mut()?;
        let end_mono = requested_end.max(active.started.monotonic);
        let end_unix = active.projected_wall_at(end_mono);
        if end_unix <= active.started.wall_unix_millis {
            return None;
        }

        let start_unix = active.started.wall_unix_millis / 1_000;
        let segment_end_unix = end_unix / 1_000;
        let segment = Segment {
            app_path: active.app.path.clone(),
            app_basename: active.app.basename.clone(),
            title: active.app.title.clone(),
            start_unix,
            end_unix: segment_end_unix,
        };
        if segment.end_unix <= segment.start_unix {
            // Keep the sub-second remainder in the active segment so a very
            // early checkpoint cannot erase it before v1's second boundary.
            return None;
        }
        active.started = TimePoint {
            monotonic: end_mono,
            wall_unix_millis: end_unix,
        };
        Some(segment)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WALL: u64 = 10_000;

    fn app(name: &str) -> AppKey {
        AppKey {
            path: format!("C:/{name}.exe"),
            basename: format!("{name}.exe"),
            title: None,
        }
    }

    fn titled_app(name: &str, title: &str) -> AppKey {
        AppKey {
            title: Some(title.to_string()),
            ..app(name)
        }
    }

    fn at(monotonic: u64) -> TimePoint {
        TimePoint::new(
            monotonic.saturating_mul(1_000),
            (WALL + monotonic).saturating_mul(1_000),
        )
    }

    fn at_wall(monotonic: u64, wall_unix: u64) -> TimePoint {
        TimePoint::new(
            monotonic.saturating_mul(1_000),
            wall_unix.saturating_mul(1_000),
        )
    }

    fn mono(seconds: u64) -> MonoTime {
        MonoTime::from_millis(seconds.saturating_mul(1_000))
    }

    fn fg(name: &str, monotonic: u64) -> Event {
        Event::Foreground {
            app: app(name),
            at: at(monotonic),
            last_input: mono(monotonic),
        }
    }

    fn total_duration(segments: &[Segment]) -> u64 {
        segments.iter().map(Segment::duration).sum()
    }

    #[test]
    fn switch_emits_segment() {
        let mut aggregator = Aggregator::new(300);
        assert!(aggregator.handle(fg("a", 100)).is_empty());
        let segments = aggregator.handle(fg("b", 150));
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].duration(), 50);
        assert_eq!(segments[0].app_basename, "a.exe");
    }

    #[test]
    fn afk_cuts_segment_at_real_last_input_deadline() {
        let mut aggregator = Aggregator::new(300);
        aggregator.handle(fg("a", 0));
        let segments = aggregator.handle(Event::IdleTick {
            at: at(400),
            last_input: mono(10),
        });
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].end_unix, WALL + 310);
    }

    #[test]
    fn lock_suspend_resume_unlock_keeps_independent_suppression_reasons() {
        let mut aggregator = Aggregator::new(300);
        aggregator.handle(fg("a", 0));
        let segments = aggregator.handle(Event::SessionLock { at: at(50) });
        assert_eq!(total_duration(&segments), 50);

        aggregator.handle(Event::Suspend { at: at(60) });
        aggregator.handle(Event::Resume { at: at(1_000) });
        assert!(aggregator.is_suppressed());
        assert!(aggregator.handle(fg("ignored", 1_010)).is_empty());

        aggregator.handle(Event::SessionUnlock { at: at(1_020) });
        assert!(!aggregator.is_suppressed());
        aggregator.handle(fg("c", 1_030));
        let segments = aggregator.handle(Event::Shutdown { at: at(1_040) });
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].app_basename, "c.exe");
        assert_eq!(segments[0].duration(), 10);
    }

    #[test]
    fn suspend_resume_keeps_no_segment_until_foreground() {
        let mut aggregator = Aggregator::new(300);
        aggregator.handle(fg("a", 0));
        aggregator.handle(Event::Suspend { at: at(30) });
        aggregator.handle(Event::Resume { at: at(1_000) });
        let segments = aggregator.handle(Event::Shutdown { at: at(1_010) });
        assert!(segments.is_empty());
    }

    #[test]
    fn idle_tick_with_recent_input_does_not_cut() {
        let mut aggregator = Aggregator::new(300);
        aggregator.handle(fg("a", 0));
        let segments = aggregator.handle(Event::IdleTick {
            at: at(100),
            last_input: mono(90),
        });
        assert!(segments.is_empty());
    }

    #[test]
    fn focus_steal_during_afk_does_not_reset_idle_clock() {
        let mut aggregator = Aggregator::new(300);
        aggregator.handle(Event::Foreground {
            app: app("a"),
            at: at(0),
            last_input: mono(0),
        });
        let segments = aggregator.handle(Event::Foreground {
            app: app("notification"),
            at: at(400),
            last_input: mono(0),
        });
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].app_basename, "a.exe");
        assert_eq!(segments[0].end_unix, WALL + 300);
        assert!(!aggregator.is_active());
    }

    #[test]
    fn idle_tick_trusts_os_even_if_previous_value_was_higher() {
        let mut aggregator = Aggregator::new(300);
        aggregator.handle(fg("a", 100));
        aggregator.handle(Event::IdleTick {
            at: at(200),
            last_input: mono(200),
        });
        let segments = aggregator.handle(Event::IdleTick {
            at: at(500),
            last_input: mono(100),
        });
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].end_unix, WALL + 400);
    }

    #[test]
    fn continuous_title_changes_do_not_extend_afk() {
        let mut aggregator = Aggregator::new(300);
        aggregator.handle(Event::Foreground {
            app: titled_app("a", "initial"),
            at: at(0),
            last_input: mono(0),
        });

        let mut emitted = Vec::new();
        for (time, title) in [(100, "one"), (200, "two"), (250, "three")] {
            emitted.extend(aggregator.handle(Event::Foreground {
                app: titled_app("a", title),
                at: at(time),
                last_input: mono(0),
            }));
        }
        emitted.extend(aggregator.handle(Event::IdleTick {
            at: at(310),
            last_input: mono(0),
        }));

        assert_eq!(total_duration(&emitted), 300);
        assert!(!aggregator.is_active());
        assert_eq!(
            emitted
                .windows(2)
                .map(|pair| (pair[0].end_unix, pair[1].start_unix))
                .collect::<Vec<_>>(),
            vec![
                (WALL + 100, WALL + 100),
                (WALL + 200, WALL + 200),
                (WALL + 250, WALL + 250),
            ]
        );
    }

    #[test]
    fn background_focus_events_do_not_refresh_afk() {
        let mut aggregator = Aggregator::new(300);
        aggregator.handle(Event::Foreground {
            app: app("a"),
            at: at(0),
            last_input: mono(0),
        });

        let mut emitted = aggregator.handle(Event::Foreground {
            app: app("background-notification"),
            at: at(250),
            last_input: mono(0),
        });
        emitted.extend(aggregator.handle(Event::IdleTick {
            at: at(310),
            last_input: mono(0),
        }));

        assert_eq!(total_duration(&emitted), 300);
        assert!(!aggregator.is_active());
    }

    #[test]
    fn checkpoints_are_contiguous_and_ignore_wall_clock_jumps() {
        let mut aggregator = Aggregator::new(300);
        aggregator.handle(Event::Foreground {
            app: app("a"),
            at: at_wall(0, 1_000),
            last_input: mono(0),
        });

        let mut emitted = aggregator.handle(Event::Checkpoint {
            at: at_wall(100, 5_000),
            last_input: mono(90),
        });
        emitted.extend(aggregator.handle(Event::Checkpoint {
            at: at_wall(200, 100),
            last_input: mono(190),
        }));
        emitted.extend(aggregator.handle(Event::Shutdown {
            at: at_wall(250, 50_000),
        }));

        assert_eq!(total_duration(&emitted), 250);
        assert_eq!(
            emitted
                .iter()
                .map(|segment| (segment.start_unix, segment.end_unix))
                .collect::<Vec<_>>(),
            vec![(1_000, 1_100), (1_100, 1_200), (1_200, 1_250)]
        );
    }

    #[test]
    fn checkpoint_preserves_last_input_across_afk_boundary() {
        let mut aggregator = Aggregator::new(300);
        aggregator.handle(Event::Foreground {
            app: app("a"),
            at: at(0),
            last_input: mono(0),
        });

        let mut emitted = aggregator.handle(Event::Checkpoint {
            at: at(250),
            last_input: mono(0),
        });
        emitted.extend(aggregator.handle(Event::Checkpoint {
            at: at(310),
            last_input: mono(0),
        }));

        assert_eq!(total_duration(&emitted), 300);
        assert_eq!(emitted[0].end_unix, emitted[1].start_unix);
        assert!(!aggregator.is_active());
    }

    #[test]
    fn wall_clock_rollback_and_forward_jump_never_change_elapsed_time() {
        let mut aggregator = Aggregator::new(300);
        aggregator.handle(Event::Foreground {
            app: app("a"),
            at: at_wall(100, 5_000),
            last_input: mono(100),
        });
        let mut emitted = aggregator.handle(Event::Foreground {
            app: app("b"),
            at: at_wall(120, 1),
            last_input: mono(120),
        });
        emitted.extend(aggregator.handle(Event::Shutdown {
            at: at_wall(130, 50_000),
        }));

        assert_eq!(total_duration(&emitted), 30);
        assert_eq!(
            emitted
                .iter()
                .map(|segment| (segment.start_unix, segment.end_unix))
                .collect::<Vec<_>>(),
            vec![(5_000, 5_020), (5_020, 5_030)]
        );
    }

    #[test]
    fn out_of_order_monotonic_input_does_not_panic_or_create_negative_time() {
        let mut aggregator = Aggregator::new(300);
        aggregator.handle(Event::Foreground {
            app: app("a"),
            at: at_wall(100, 1_000),
            last_input: mono(100),
        });
        assert!(aggregator
            .handle(Event::IdleTick {
                at: at_wall(90, 900),
                last_input: mono(90),
            })
            .is_empty());
        let segments = aggregator.handle(Event::Shutdown {
            at: at_wall(90, 900),
        });
        assert!(segments.is_empty());
    }

    #[test]
    fn checkpoint_crosses_utc_midnight_without_gap_or_overlap() {
        let mut aggregator = Aggregator::new(300);
        aggregator.handle(Event::Foreground {
            app: app("a"),
            at: at_wall(0, 86_390),
            last_input: mono(0),
        });
        let mut emitted = aggregator.handle(Event::Checkpoint {
            at: at_wall(20, 1_000_000),
            last_input: mono(10),
        });
        emitted.extend(aggregator.handle(Event::Shutdown {
            at: at_wall(30, 100),
        }));

        assert_eq!(
            emitted
                .iter()
                .map(|segment| (segment.start_unix, segment.end_unix))
                .collect::<Vec<_>>(),
            vec![(86_390, 86_410), (86_410, 86_420)]
        );
        assert_eq!(total_duration(&emitted), 30);
    }
}
