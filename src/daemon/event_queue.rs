//! Bounded hand-off between Win32 callbacks and the aggregator thread.
//!
//! Foreground and control events are lossless and FIFO. Their producer applies
//! backpressure at a fixed timeline capacity, with one producer-owned overflow
//! slot preserving the order of the foreground event that triggers pressure.
//! Title notifications are deliberately lower priority: at most one
//! current-foreground title is kept, and storms are coalesced over a short,
//! non-sliding window.

#![cfg(windows)]

use std::collections::VecDeque;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::HWND;

use super::aggregator::{MonoTime, TimePoint};

pub const TIMELINE_CAPACITY: usize = 1_024;
const TITLE_COALESCE_WINDOW: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueDisconnected;

impl std::fmt::Display for QueueDisconnected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("event queue receiver disconnected")
    }
}

impl std::error::Error for QueueDisconnected {}

/// An HWND represented as an integer so callback payloads are safe to move to
/// the aggregator thread. It is an opaque lookup key, never dereferenced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowId(pub isize);

impl WindowId {
    pub fn from_hwnd(hwnd: HWND) -> Self {
        Self(hwnd as isize)
    }

    pub fn as_hwnd(self) -> HWND {
        self.0 as HWND
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundEvent {
    pub window: WindowId,
    pub pid_at_event: u32,
    pub raw_event_millis: u32,
    pub at: TimePoint,
    /// Last input only when it is known not to postdate this event. A delayed
    /// callback can observe newer input; that must remain `None`.
    pub last_input: Option<MonoTime>,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleEvent {
    pub window: WindowId,
    pub raw_event_millis: u32,
    pub at: TimePoint,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookEvent {
    Foreground(ForegroundEvent),
    Title(TitleEvent),
    IdleTick {
        at: TimePoint,
        last_input: Option<MonoTime>,
    },
    SessionLock {
        at: TimePoint,
        last_input: Option<MonoTime>,
    },
    SessionUnlock {
        at: TimePoint,
    },
    Suspend {
        at: TimePoint,
        last_input: Option<MonoTime>,
    },
    Resume {
        at: TimePoint,
    },
    Shutdown {
        at: TimePoint,
        last_input: Option<MonoTime>,
    },
}

#[derive(Debug, Clone, Copy)]
struct CurrentForeground {
    window: WindowId,
    generation: u64,
}

#[derive(Debug)]
struct PendingTitle {
    event: TitleEvent,
    ready_at: Instant,
}

#[derive(Debug)]
struct State {
    timeline: VecDeque<HookEvent>,
    pending_title: Option<PendingTitle>,
    current_foreground: Option<CurrentForeground>,
    next_generation: u64,
    sender_count: usize,
    receiver_alive: bool,
}

#[derive(Debug)]
struct Shared {
    capacity: usize,
    title_coalesce_window: Duration,
    state: Mutex<State>,
    available: Condvar,
    space: Condvar,
}

#[derive(Debug)]
pub struct EventSender {
    shared: Arc<Shared>,
}

impl Clone for EventSender {
    fn clone(&self) -> Self {
        let mut state = lock(&self.shared.state);
        state.sender_count = state.sender_count.saturating_add(1);
        drop(state);
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl Drop for EventSender {
    fn drop(&mut self) {
        let mut state = lock(&self.shared.state);
        state.sender_count = state.sender_count.saturating_sub(1);
        let disconnected = state.sender_count == 0;
        drop(state);
        if disconnected {
            self.shared.available.notify_all();
        }
    }
}

impl EventSender {
    /// Queue a foreground event without loss. One producer-owned overflow slot
    /// records the event before applying backpressure, so a later control
    /// event can never overtake a foreground transition that already happened.
    ///
    /// WinEvent callbacks are serialized on the dedicated WinEvent thread, so
    /// at most one event can occupy this overflow slot.
    pub fn send_foreground(&self, mut event: ForegroundEvent) -> Result<(), QueueDisconnected> {
        let mut state = lock(&self.shared.state);
        while state.timeline.len() > self.shared.capacity && state.receiver_alive {
            state = wait(&self.shared.space, state);
        }
        if !state.receiver_alive {
            return Err(QueueDisconnected);
        }

        state.next_generation = state.next_generation.wrapping_add(1).max(1);
        event.generation = state.next_generation;
        state.current_foreground = Some(CurrentForeground {
            window: event.window,
            generation: event.generation,
        });
        // A title from the previous foreground can no longer be relevant.
        state.pending_title = None;
        state.timeline.push_back(HookEvent::Foreground(event));
        self.shared.available.notify_one();

        while state.timeline.len() > self.shared.capacity && state.receiver_alive {
            state = wait(&self.shared.space, state);
        }
        if state.receiver_alive {
            Ok(())
        } else {
            Err(QueueDisconnected)
        }
    }

    /// Keep only the latest title for the current foreground generation.
    ///
    /// This never blocks a WinEvent callback. `false` means the notification
    /// was stale/background or the receiver has already stopped.
    pub fn send_title(&self, window: WindowId, raw_event_millis: u32, at: TimePoint) -> bool {
        let mut state = lock(&self.shared.state);
        if !state.receiver_alive {
            return false;
        }
        let Some(current) = state.current_foreground else {
            return false;
        };
        if current.window != window {
            return false;
        }

        let event = TitleEvent {
            window,
            raw_event_millis,
            at,
            generation: current.generation,
        };
        if let Some(pending) = &mut state.pending_title {
            // Non-sliding window: replace the payload, retain the first
            // notification's deadline so a continuous storm cannot starve.
            pending.event = event;
        } else {
            state.pending_title = Some(PendingTitle {
                event,
                ready_at: Instant::now()
                    .checked_add(self.shared.title_coalesce_window)
                    .unwrap_or_else(Instant::now),
            });
        }
        drop(state);
        self.shared.available.notify_one();
        true
    }

    /// Queue a non-title event in the lossless timeline.
    pub fn send_timeline(&self, event: HookEvent) -> Result<(), QueueDisconnected> {
        debug_assert!(!matches!(
            event,
            HookEvent::Foreground(_) | HookEvent::Title(_)
        ));
        let mut state = lock(&self.shared.state);
        while state.timeline.len() >= self.shared.capacity && state.receiver_alive {
            state = wait(&self.shared.space, state);
        }
        if !state.receiver_alive {
            return Err(QueueDisconnected);
        }
        state.timeline.push_back(event);
        drop(state);
        self.shared.available.notify_one();
        Ok(())
    }

    #[cfg(test)]
    fn pending_counts(&self) -> (usize, usize) {
        let state = lock(&self.shared.state);
        (
            state.timeline.len(),
            usize::from(state.pending_title.is_some()),
        )
    }
}

#[derive(Debug)]
pub struct EventReceiver {
    shared: Arc<Shared>,
}

impl EventReceiver {
    pub fn recv_timeout(&self, timeout: Duration) -> Result<HookEvent, RecvTimeoutError> {
        let started = Instant::now();
        let deadline = started.checked_add(timeout);
        let mut state = lock(&self.shared.state);

        loop {
            if let Some(event) = state.timeline.pop_front() {
                drop(state);
                self.shared.space.notify_all();
                return Ok(event);
            }

            let now = Instant::now();
            if state
                .pending_title
                .as_ref()
                .is_some_and(|pending| pending.ready_at <= now)
            {
                return Ok(HookEvent::Title(
                    state.pending_title.take().expect("checked above").event,
                ));
            }
            if state.sender_count == 0 {
                return Err(RecvTimeoutError::Disconnected);
            }

            let remaining = match deadline {
                Some(deadline) => deadline.saturating_duration_since(now),
                None => timeout.saturating_sub(now.saturating_duration_since(started)),
            };
            if remaining.is_zero() {
                return Err(RecvTimeoutError::Timeout);
            }
            let wait_for = state
                .pending_title
                .as_ref()
                .map(|pending| {
                    pending
                        .ready_at
                        .saturating_duration_since(now)
                        .min(remaining)
                })
                .unwrap_or(remaining);
            let (next_state, _) = wait_timeout(&self.shared.available, state, wait_for);
            state = next_state;
        }
    }
}

impl Drop for EventReceiver {
    fn drop(&mut self) {
        let mut state = lock(&self.shared.state);
        state.receiver_alive = false;
        drop(state);
        self.shared.space.notify_all();
    }
}

pub fn channel(capacity: usize) -> (EventSender, EventReceiver) {
    channel_with_title_window(capacity, TITLE_COALESCE_WINDOW)
}

fn channel_with_title_window(
    capacity: usize,
    title_coalesce_window: Duration,
) -> (EventSender, EventReceiver) {
    assert!(capacity > 0, "event timeline capacity must be positive");
    let shared = Arc::new(Shared {
        capacity,
        title_coalesce_window,
        state: Mutex::new(State {
            timeline: VecDeque::with_capacity(capacity),
            pending_title: None,
            current_foreground: None,
            next_generation: 0,
            sender_count: 1,
            receiver_alive: true,
        }),
        available: Condvar::new(),
        space: Condvar::new(),
    });
    (
        EventSender {
            shared: Arc::clone(&shared),
        },
        EventReceiver { shared },
    )
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wait<'a, T>(condvar: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    condvar
        .wait(guard)
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wait_timeout<'a, T>(
    condvar: &Condvar,
    guard: MutexGuard<'a, T>,
    timeout: Duration,
) -> (MutexGuard<'a, T>, std::sync::WaitTimeoutResult) {
    condvar
        .wait_timeout(guard, timeout)
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    fn foreground(window: isize, event_millis: u32) -> ForegroundEvent {
        ForegroundEvent {
            window: WindowId(window),
            pid_at_event: window as u32 + 100,
            raw_event_millis: event_millis,
            at: TimePoint::new(event_millis as u64, event_millis as u64 + 10_000),
            last_input: Some(MonoTime::from_millis(event_millis as u64)),
            generation: 0,
        }
    }

    #[test]
    fn foreground_fifo_is_lossless_and_backpressures_when_full() {
        let (sender, receiver) = channel_with_title_window(2, Duration::ZERO);
        let (filled_tx, filled_rx) = mpsc::channel();
        let (third_tx, third_rx) = mpsc::channel();
        let producer = thread::spawn(move || {
            sender.send_foreground(foreground(1, 10)).unwrap();
            sender.send_foreground(foreground(2, 20)).unwrap();
            filled_tx.send(()).unwrap();
            sender.send_foreground(foreground(3, 30)).unwrap();
            third_tx.send(()).unwrap();
        });

        filled_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(third_rx.try_recv(), Err(mpsc::TryRecvError::Empty));

        let first = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        third_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let second = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        let third = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        producer.join().unwrap();

        let observed = [first, second, third].map(|event| match event {
            HookEvent::Foreground(event) => (
                event.window,
                event.pid_at_event,
                event.raw_event_millis,
                event.at.monotonic.0,
            ),
            other => panic!("unexpected event: {other:?}"),
        });
        assert_eq!(
            observed,
            [
                (WindowId(1), 101, 10, 10),
                (WindowId(2), 102, 20, 20),
                (WindowId(3), 103, 30, 30),
            ]
        );
    }

    #[test]
    fn foreground_is_enqueued_before_backpressure_so_control_cannot_overtake_it() {
        let (sender, receiver) = channel_with_title_window(2, Duration::ZERO);
        sender.send_foreground(foreground(1, 10)).unwrap();
        sender.send_foreground(foreground(2, 20)).unwrap();

        let foreground_sender = sender.clone();
        let (foreground_returned_tx, foreground_returned_rx) = mpsc::channel();
        let foreground_producer = thread::spawn(move || {
            foreground_sender
                .send_foreground(foreground(3, 30))
                .unwrap();
            foreground_returned_tx.send(()).unwrap();
        });

        let deadline = Instant::now() + Duration::from_secs(1);
        while sender.pending_counts().0 != 3 {
            assert!(
                Instant::now() < deadline,
                "foreground never occupied its bounded overflow slot"
            );
            thread::yield_now();
        }
        assert_eq!(
            foreground_returned_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        );

        let control_sender = sender.clone();
        let (control_returned_tx, control_returned_rx) = mpsc::channel();
        let control_producer = thread::spawn(move || {
            control_sender
                .send_timeline(HookEvent::Suspend {
                    at: TimePoint::new(40, 10_040),
                    last_input: Some(MonoTime::from_millis(30)),
                })
                .unwrap();
            control_returned_tx.send(()).unwrap();
        });

        let mut observed = Vec::new();
        for _ in 0..4 {
            observed.push(receiver.recv_timeout(Duration::from_secs(1)).unwrap());
        }
        foreground_returned_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        control_returned_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        foreground_producer.join().unwrap();
        control_producer.join().unwrap();

        assert!(matches!(
            &observed[..],
            [
                HookEvent::Foreground(ForegroundEvent {
                    window: WindowId(1),
                    ..
                }),
                HookEvent::Foreground(ForegroundEvent {
                    window: WindowId(2),
                    ..
                }),
                HookEvent::Foreground(ForegroundEvent {
                    window: WindowId(3),
                    ..
                }),
                HookEvent::Suspend { .. }
            ]
        ));
    }

    #[test]
    fn title_storm_is_single_slot_and_stale_windows_are_discarded() {
        let (sender, receiver) = channel_with_title_window(4, Duration::ZERO);
        sender.send_foreground(foreground(1, 10)).unwrap();
        let first = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        let first_generation = match first {
            HookEvent::Foreground(event) => event.generation,
            other => panic!("unexpected event: {other:?}"),
        };

        for tick in 11..1_011 {
            assert!(sender.send_title(WindowId(1), tick, TimePoint::new(tick as u64, 0)));
        }
        assert_eq!(sender.pending_counts(), (0, 1));

        sender.send_foreground(foreground(2, 2_000)).unwrap();
        assert!(!sender.send_title(WindowId(1), 2_001, TimePoint::new(2_001, 0)));
        assert_eq!(sender.pending_counts(), (1, 0));
        let second = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        let second_generation = match second {
            HookEvent::Foreground(event) => event.generation,
            other => panic!("unexpected event: {other:?}"),
        };
        assert!(second_generation > first_generation);

        for tick in 2_002..3_002 {
            assert!(sender.send_title(WindowId(2), tick, TimePoint::new(tick as u64, 0)));
        }
        assert_eq!(sender.pending_counts(), (0, 1));
        let latest = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        match latest {
            HookEvent::Title(event) => {
                assert_eq!(event.window, WindowId(2));
                assert_eq!(event.generation, second_generation);
                assert_eq!(event.raw_event_millis, 3_001);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
