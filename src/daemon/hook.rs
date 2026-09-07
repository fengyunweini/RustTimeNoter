//! Fixed-size Win32 observations. Callbacks never wait for consumer capacity
//! and never resolve executable paths or window titles.

#![cfg(windows)]

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_CLASS_ALREADY_EXISTS, HWND, LPARAM, LRESULT, WPARAM,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Power::{
    RegisterSuspendResumeNotification, UnregisterSuspendResumeNotification, HPOWERNOTIFY,
};
use windows_sys::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetForegroundWindow, KillTimer,
    PostQuitMessage, RegisterClassExW, SetTimer, DEVICE_NOTIFY_WINDOW_HANDLE,
    EVENT_OBJECT_NAMECHANGE, EVENT_SYSTEM_FOREGROUND, PBT_APMRESUMEAUTOMATIC,
    PBT_APMRESUMECRITICAL, PBT_APMRESUMESUSPEND, PBT_APMSUSPEND, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_DESTROY, WM_ENDSESSION, WM_POWERBROADCAST, WM_QUERYENDSESSION,
    WM_TIMER, WM_WTSSESSION_CHANGE, WNDCLASSEXW, WS_OVERLAPPED, WTS_SESSION_LOCK,
    WTS_SESSION_UNLOCK,
};

use super::aggregator::{MonoTime, TimePoint};
use crate::platform::windows as platform;

/// An opaque identity, never a pointer to dereference on the consumer thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowSnapshot {
    pub hwnd: isize,
    pub pid: u32,
}

impl WindowSnapshot {
    fn capture(hwnd: HWND) -> Self {
        Self {
            hwnd: hwnd as isize,
            pid: platform::window_pid(hwnd).unwrap_or(0),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct HookEvent {
    pub at: TimePoint,
    pub kind: HookKind,
}

#[derive(Debug, Clone, Copy)]
pub enum HookKind {
    Foreground {
        window: WindowSnapshot,
        last_input: Option<MonoTime>,
    },
    Title {
        window: WindowSnapshot,
        last_input: Option<MonoTime>,
    },
    Sample {
        window: Option<WindowSnapshot>,
        last_input: Option<MonoTime>,
        locked: bool,
        suspended: bool,
    },
    Lock(bool),
    Suspend(bool),
    Shutdown {
        last_input: Option<MonoTime>,
    },
}

static SENDER: OnceLock<SyncSender<HookEvent>> = OnceLock::new();
static SESSION_LOCKED: AtomicBool = AtomicBool::new(false);
static SUSPENDED: AtomicBool = AtomicBool::new(false);
// A gap closes on the monotonic boundary. Project wall time when consumed,
// avoiding a multi-field timestamp lock inside the callbacks.
static FIRST_DROPPED: AtomicU64 = AtomicU64::new(u64::MAX);
static SHUTDOWN: ShutdownState = ShutdownState::new();
static CAPTURE: CaptureState = CaptureState::new();

struct CaptureState {
    in_flight: AtomicUsize,
}

impl CaptureState {
    const fn new() -> Self {
        Self {
            in_flight: AtomicUsize::new(0),
        }
    }

    fn enter(&self) -> CaptureGuard<'_> {
        // The shutdown state and this count share a sequentially consistent
        // order: a capture that saw "running" cannot hide from the terminal drain.
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        CaptureGuard(self)
    }

    fn begin(&self, shutdown: &ShutdownState) -> Option<CaptureGuard<'_>> {
        let guard = self.enter();
        // Admission precedes all observation reads. Once admitted, a producer
        // must finish its enqueue attempt even if shutdown is requested while
        // it is collecting the HWND/input snapshot.
        (shutdown.state.load(Ordering::SeqCst) == 0).then_some(guard)
    }

    fn quiescent(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst) == 0
    }
}

struct CaptureGuard<'a>(&'a CaptureState);

impl CaptureGuard<'_> {
    fn send(&self, event: HookEvent) {
        if let Some(sender) = SENDER.get() {
            try_send(sender, event, &FIRST_DROPPED);
        }
    }
}

impl Drop for CaptureGuard<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// After observing a shutdown request, a true result means every previously
/// admitted capture has finished collecting and enqueueing its observations.
/// Later callbacks reject admission before reading any observation state.
pub fn capture_quiescent() -> bool {
    CAPTURE.quiescent()
}

struct ShutdownState {
    // 0 = unset; 1 = one caller publishing; 2 = immutable snapshot is ready.
    state: AtomicU8,
    monotonic: AtomicU64,
    wall: AtomicU64,
    last_input: AtomicU64,
    complete: AtomicBool,
}

impl ShutdownState {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(0),
            monotonic: AtomicU64::new(0),
            wall: AtomicU64::new(0),
            last_input: AtomicU64::new(u64::MAX),
            complete: AtomicBool::new(false),
        }
    }

    fn request(&self, at: TimePoint, last_input: Option<MonoTime>) -> bool {
        if self
            .state
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return false;
        }
        self.monotonic.store(at.monotonic.0, Ordering::Relaxed);
        self.wall.store(at.wall_unix_millis, Ordering::Relaxed);
        self.last_input.store(
            last_input.map(|input| input.0).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        self.state.store(2, Ordering::SeqCst);
        true
    }

    fn event(&self) -> Option<HookEvent> {
        if self.state.load(Ordering::SeqCst) != 2 {
            return None;
        }
        let input = self.last_input.load(Ordering::Relaxed);
        Some(HookEvent {
            at: TimePoint::new(
                self.monotonic.load(Ordering::Relaxed),
                self.wall.load(Ordering::Relaxed),
            ),
            kind: HookKind::Shutdown {
                last_input: (input != u64::MAX).then_some(MonoTime::from_millis(input)),
            },
        })
    }
}

/// Retain the terminal boundary even if the ordinary capture queue is full.
/// Repeated requests keep the first boundary and never wait on another caller.
pub fn request_shutdown() {
    let capture = CAPTURE.enter();
    let at = time_point();
    let last_input = last_input_at(at);
    publish_terminal(&SHUTDOWN, at, last_input, sample_at, |event| {
        capture.send(event);
    });
}

fn publish_terminal(
    shutdown: &ShutdownState,
    at: TimePoint,
    last_input: Option<MonoTime>,
    snapshot: impl FnOnce(TimePoint, Option<MonoTime>) -> HookEvent,
    mut publish: impl FnMut(HookEvent),
) -> bool {
    if shutdown.request(at, last_input) {
        // The reserved terminal is visible already, but the caller retains
        // its capture guard through both sends. Final drain therefore waits
        // for this identity check even when it sees shutdown out of band.
        publish(snapshot(at, last_input));
        publish(HookEvent {
            at,
            kind: HookKind::Shutdown { last_input },
        });
        true
    } else {
        false
    }
}

pub fn shutdown_requested() -> bool {
    SHUTDOWN.state.load(Ordering::SeqCst) == 2
}

pub fn shutdown_event() -> Option<HookEvent> {
    SHUTDOWN.event()
}

/// Called after accounting's final durable flush, including normal stop paths.
pub fn mark_shutdown_complete() {
    SHUTDOWN.complete.store(true, Ordering::Release);
}

/// Only process-termination callbacks may wait. Foreground, title, timer,
/// lock and power callbacks always return without waiting for workers.
pub fn wait_shutdown_complete(timeout: Duration) -> bool {
    wait_for_shutdown(&SHUTDOWN, timeout)
}

fn wait_for_shutdown(shutdown: &ShutdownState, timeout: Duration) -> bool {
    let started = Instant::now();
    loop {
        if shutdown.complete.load(Ordering::Acquire) {
            return true;
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return false;
        }
        std::thread::sleep(remaining.min(Duration::from_millis(5)));
    }
}

pub fn set_sender(sender: SyncSender<HookEvent>) -> std::io::Result<()> {
    SENDER.set(sender).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "hook event sender is already registered",
        )
    })
}

/// Publish the initial sample and timer observations under the same admission
/// guard as WinEvent callbacks, including all snapshot reads before try_send.
pub fn publish_sample() {
    if let Some(capture) = CAPTURE.begin(&SHUTDOWN) {
        capture.send(sample());
    }
}

fn try_send(sender: &SyncSender<HookEvent>, event: HookEvent, first_dropped: &AtomicU64) {
    if let Err(TrySendError::Full(event)) = sender.try_send(event) {
        first_dropped.fetch_min(event.at.monotonic.0, Ordering::AcqRel);
    }
}

/// Consume the earliest lost observation. Runtime must end attribution there
/// and establish a fresh sample before accounting for the uncertain interval.
pub fn take_overflow() -> Option<TimePoint> {
    take_dropped(&FIRST_DROPPED).map(|mono| project_time_point(time_point(), mono))
}

fn take_dropped(first_dropped: &AtomicU64) -> Option<MonoTime> {
    let millis = first_dropped.swap(u64::MAX, Ordering::AcqRel);
    (millis != u64::MAX).then_some(MonoTime::from_millis(millis))
}

/// Current observation for startup, timer ticks and overload recovery.
/// Control bits are updated before enqueueing, so overflow cannot lose them.
pub fn sample() -> HookEvent {
    let at = time_point();
    sample_at(at, last_input_at(at))
}

fn sample_at(at: TimePoint, last_input: Option<MonoTime>) -> HookEvent {
    HookEvent {
        at,
        kind: HookKind::Sample {
            window: platform::foreground_window().map(WindowSnapshot::capture),
            last_input,
            locked: SESSION_LOCKED.load(Ordering::Acquire),
            suspended: SUSPENDED.load(Ordering::Acquire),
        },
    }
}

fn time_point() -> TimePoint {
    let monotonic = MonoTime::from_millis(platform::monotonic_millis());
    let wall_unix_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    TimePoint {
        monotonic,
        wall_unix_millis,
    }
}

fn event_time_point(raw_event_millis: u32) -> TimePoint {
    let observed = time_point();
    let event_millis = platform::extend_tick_count_32(raw_event_millis, observed.monotonic.0);
    project_time_point(observed, MonoTime::from_millis(event_millis))
}

fn project_time_point(observed: TimePoint, monotonic: MonoTime) -> TimePoint {
    TimePoint {
        monotonic,
        wall_unix_millis: observed
            .wall_unix_millis
            .saturating_sub(observed.monotonic.0.saturating_sub(monotonic.0)),
    }
}

fn last_input_at(at: TimePoint) -> Option<MonoTime> {
    platform::last_input_monotonic_millis(at.monotonic.0).map(MonoTime::from_millis)
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _id_event_thread: u32,
    raw_event_millis: u32,
) {
    if event != EVENT_SYSTEM_FOREGROUND
        && !(event == EVENT_OBJECT_NAMECHANGE && id_object == 0 && id_child == 0 && !hwnd.is_null())
    {
        return;
    }
    let Some(capture) = CAPTURE.begin(&SHUTDOWN) else {
        return;
    };
    if event == EVENT_OBJECT_NAMECHANGE && unsafe { GetForegroundWindow() } != hwnd {
        return;
    }
    let at = event_time_point(raw_event_millis);
    let window = WindowSnapshot::capture(hwnd);
    let last_input = last_input_at(at);
    let kind = if event == EVENT_SYSTEM_FOREGROUND {
        // Preserve invalid/destroyed foreground identities as gap boundaries.
        HookKind::Foreground { window, last_input }
    } else {
        HookKind::Title { window, last_input }
    };
    capture.send(HookEvent { at, kind });
}

/// Kept on the same message thread as the session/power window.
pub struct WinHook {
    foreground: HWINEVENTHOOK,
    title: HWINEVENTHOOK,
}

impl WinHook {
    pub fn install(capture_titles: bool) -> std::io::Result<Self> {
        let foreground = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                std::ptr::null_mut(),
                Some(win_event_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };
        if foreground.is_null() {
            return Err(registration_error("SetWinEventHook(foreground)"));
        }
        let mut hooks = Self {
            foreground,
            title: std::ptr::null_mut(),
        };
        if capture_titles {
            hooks.title = unsafe {
                SetWinEventHook(
                    EVENT_OBJECT_NAMECHANGE,
                    EVENT_OBJECT_NAMECHANGE,
                    std::ptr::null_mut(),
                    Some(win_event_proc),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                )
            };
            if hooks.title.is_null() {
                return Err(registration_error("SetWinEventHook(title)"));
            }
        }
        Ok(hooks)
    }
}

impl Drop for WinHook {
    fn drop(&mut self) {
        unsafe {
            if !self.foreground.is_null() {
                UnhookWinEvent(self.foreground);
            }
            if !self.title.is_null() {
                UnhookWinEvent(self.title);
            }
        }
    }
}

const TIMER_ID: usize = 1;

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION => {
            if wparam != 0 {
                request_shutdown();
                // Windows may terminate the process as soon as this handler
                // returns. Give the independent consumer a bounded opportunity
                // to acknowledge its final durable flush before returning.
                let _ = wait_shutdown_complete(Duration::from_secs(4));
                unsafe {
                    PostQuitMessage(0);
                }
            }
            0
        }
        WM_TIMER if wparam == TIMER_ID => {
            publish_sample();
            0
        }
        WM_WTSSESSION_CHANGE => {
            let Some(capture) = CAPTURE.begin(&SHUTDOWN) else {
                return 0;
            };
            let locked = match wparam as u32 {
                WTS_SESSION_LOCK => Some(true),
                WTS_SESSION_UNLOCK => Some(false),
                _ => None,
            };
            if let Some(locked) = locked {
                SESSION_LOCKED.store(locked, Ordering::Release);
                let observation = sample();
                let at = observation.at;
                capture.send(observation);
                capture.send(HookEvent {
                    at,
                    kind: HookKind::Lock(locked),
                });
            }
            0
        }
        WM_POWERBROADCAST => {
            let Some(capture) = CAPTURE.begin(&SHUTDOWN) else {
                return 1;
            };
            let suspended = match wparam as u32 {
                PBT_APMSUSPEND => Some(true),
                PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMECRITICAL | PBT_APMRESUMESUSPEND => {
                    Some(false)
                }
                _ => None,
            };
            if let Some(suspended) = suspended {
                SUSPENDED.store(suspended, Ordering::Release);
                let observation = sample();
                let at = observation.at;
                capture.send(observation);
                capture.send(HookEvent {
                    at,
                    kind: HookKind::Suspend(suspended),
                });
            }
            1
        }
        WM_DESTROY => 0,
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

pub struct MessageWindow {
    pub hwnd: HWND,
    timer_set: bool,
    wts_registered: bool,
    power_handle: HPOWERNOTIFY,
}

impl MessageWindow {
    pub fn create(idle_tick_secs: u32) -> std::io::Result<Self> {
        let class_name: Vec<u16> = "RustTimeNoterMsgWindow\0".encode_utf16().collect();
        let hinst = unsafe { GetModuleHandleW(std::ptr::null()) };
        if hinst.is_null() {
            return Err(registration_error("GetModuleHandleW"));
        }
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        let atom = unsafe { RegisterClassExW(&wc) };
        if atom == 0 && unsafe { GetLastError() } != ERROR_CLASS_ALREADY_EXISTS {
            return Err(registration_error("RegisterClassExW"));
        }
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                class_name.as_ptr(),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinst,
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err(registration_error("CreateWindowExW"));
        }
        let mut window = Self {
            hwnd,
            timer_set: false,
            wts_registered: false,
            power_handle: 0,
        };
        if unsafe {
            SetTimer(
                hwnd,
                TIMER_ID,
                idle_tick_secs.saturating_mul(1_000).max(1_000),
                None,
            )
        } == 0
        {
            return Err(registration_error("SetTimer"));
        }
        window.timer_set = true;
        if unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) } == 0 {
            return Err(registration_error("WTSRegisterSessionNotification"));
        }
        window.wts_registered = true;
        // Keep the top-level window hidden: it also receives session-ending
        // broadcasts that HWND_MESSAGE windows cannot receive. Explicit power
        // registration also covers modern standby notifications.
        window.power_handle =
            unsafe { RegisterSuspendResumeNotification(hwnd, DEVICE_NOTIFY_WINDOW_HANDLE) };
        if window.power_handle == 0 {
            return Err(registration_error("RegisterSuspendResumeNotification"));
        }
        Ok(window)
    }
}

impl Drop for MessageWindow {
    fn drop(&mut self) {
        unsafe {
            if self.timer_set {
                KillTimer(self.hwnd, TIMER_ID);
            }
            if self.wts_registered {
                WTSUnRegisterSessionNotification(self.hwnd);
            }
            if self.power_handle != 0 {
                UnregisterSuspendResumeNotification(self.power_handle);
            }
            if !self.hwnd.is_null() {
                DestroyWindow(self.hwnd);
            }
        }
    }
}

fn registration_error(operation: &str) -> std::io::Error {
    let source = std::io::Error::last_os_error();
    std::io::Error::new(source.kind(), format!("{operation} failed: {source}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn event(millis: u64) -> HookEvent {
        HookEvent {
            at: TimePoint::new(millis, millis + 10_000),
            kind: HookKind::Lock(true),
        }
    }

    #[test]
    fn full_queue_returns_and_retains_earliest_dropped_boundary() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let dropped = AtomicU64::new(u64::MAX);
        try_send(&sender, event(1_000), &dropped);
        try_send(&sender, event(3_000), &dropped);
        try_send(&sender, event(2_000), &dropped);
        assert_eq!(take_dropped(&dropped), Some(MonoTime::from_millis(2_000)));
        assert_eq!(take_dropped(&dropped), None);
        assert_eq!(receiver.try_recv().unwrap().at.monotonic.0, 1_000);
        try_send(&sender, event(4_000), &dropped);
        assert_eq!(receiver.try_recv().unwrap().at.monotonic.0, 4_000);
    }

    #[test]
    fn dropped_boundary_is_newly_recorded_after_recovery() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let dropped = AtomicU64::new(u64::MAX);
        try_send(&sender, event(1_000), &dropped);
        try_send(&sender, event(2_000), &dropped);
        assert_eq!(take_dropped(&dropped), Some(MonoTime::from_millis(2_000)));
        try_send(&sender, event(3_000), &dropped);
        assert_eq!(take_dropped(&dropped), Some(MonoTime::from_millis(3_000)));
    }

    #[test]
    fn delayed_event_keeps_its_original_monotonic_boundary() {
        assert_eq!(
            project_time_point(TimePoint::new(10_000, 50_000), MonoTime::from_millis(9_000)),
            TimePoint::new(9_000, 49_000)
        );
    }

    #[test]
    fn terminal_boundary_survives_a_full_queue_and_repeated_requests() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let dropped = AtomicU64::new(u64::MAX);
        try_send(&sender, event(1_000), &dropped);
        let shutdown = ShutdownState::new();
        let at = TimePoint::new(2_000, 12_000);
        assert!(publish_terminal(
            &shutdown,
            at,
            Some(MonoTime::from_millis(1_500)),
            terminal_sample,
            |event| try_send(&sender, event, &dropped),
        ));
        assert_eq!(take_dropped(&dropped), Some(MonoTime::from_millis(2_000)));
        assert!(!publish_terminal(
            &shutdown,
            TimePoint::new(3_000, 13_000),
            None,
            |_, _| panic!("repeated shutdown must not sample again"),
            |_| panic!("repeated shutdown must not publish again"),
        ));
        let terminal = shutdown.event().unwrap();
        assert_eq!(terminal.at, at);
        assert!(matches!(
            terminal.kind,
            HookKind::Shutdown {
                last_input: Some(MonoTime(1_500))
            }
        ));
    }

    fn terminal_sample(at: TimePoint, last_input: Option<MonoTime>) -> HookEvent {
        HookEvent {
            at,
            kind: HookKind::Sample {
                window: Some(WindowSnapshot {
                    hwnd: 123,
                    pid: 456,
                }),
                last_input,
                locked: true,
                suspended: false,
            },
        }
    }

    #[test]
    fn reserved_terminal_waits_for_its_sample_before_capture_becomes_quiescent() {
        let capture = CaptureState::new();
        let shutdown = ShutdownState::new();
        let (sender, receiver) = mpsc::sync_channel(2);
        let (reserved_tx, reserved_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let at = TimePoint::new(2_000, 12_000);
        let input = Some(MonoTime(1_500));

        std::thread::scope(|scope| {
            let capture_ref = &capture;
            let shutdown_ref = &shutdown;
            let producer = scope.spawn(move || {
                let _guard = capture_ref.enter();
                assert!(publish_terminal(
                    shutdown_ref,
                    at,
                    input,
                    |at, input| {
                        reserved_tx.send(()).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                        terminal_sample(at, input)
                    },
                    |event| sender.try_send(event).unwrap(),
                ));
            });
            reserved_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(shutdown.event().unwrap().at, at);
            assert!(!capture.quiescent());
            assert!(receiver.try_recv().is_err());
            release_tx.send(()).unwrap();
            producer.join().unwrap();
        });

        assert!(capture.quiescent());
        let sample = receiver.try_recv().unwrap();
        assert_eq!(sample.at, at);
        assert!(matches!(
            sample.kind,
            HookKind::Sample {
                window: Some(WindowSnapshot {
                    hwnd: 123,
                    pid: 456
                }),
                last_input: Some(MonoTime(1_500)),
                locked: true,
                suspended: false,
            }
        ));
        let terminal = receiver.try_recv().unwrap();
        assert_eq!(terminal.at, at);
        assert!(matches!(
            terminal.kind,
            HookKind::Shutdown {
                last_input: Some(MonoTime(1_500)),
            }
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn terminal_wait_is_bounded_and_observes_completion() {
        let shutdown = ShutdownState::new();
        assert!(!wait_for_shutdown(&shutdown, Duration::ZERO));
        shutdown.complete.store(true, Ordering::Release);
        assert!(wait_for_shutdown(&shutdown, Duration::ZERO));
    }

    #[test]
    fn terminal_drain_observes_a_capture_paused_before_enqueue() {
        let capture = CaptureState::new();
        let shutdown = ShutdownState::new();
        let dropped = AtomicU64::new(u64::MAX);
        let (sender, receiver) = mpsc::sync_channel(4);
        let (admitted_tx, admitted_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        std::thread::scope(|scope| {
            let capture_ref = &capture;
            let shutdown_ref = &shutdown;
            let dropped_ref = &dropped;
            let producer_sender = sender.clone();
            let producer = scope.spawn(move || {
                let _guard = capture_ref.begin(shutdown_ref).unwrap();
                // Pause after constructing the observation, before its send.
                // The entire capture was admitted before any snapshot reads.
                let observation = event(1_000);
                admitted_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                assert!(shutdown_ref.event().is_some());
                // Admission is not checked again: this earlier observation
                // remains accepted after terminal publication.
                try_send(&producer_sender, observation, dropped_ref);
            });

            admitted_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(shutdown.request(TimePoint::new(2_000, 12_000), None));
            try_send(&sender, shutdown.event().unwrap(), &dropped);
            // The terminal message can precede a previously admitted event;
            // the consumer must not finalize while that producer is in flight.
            assert!(!capture.quiescent());
            release_tx.send(()).unwrap();
            producer.join().unwrap();
        });

        assert!(capture.quiescent());
        assert!(matches!(
            receiver.try_recv().unwrap().kind,
            HookKind::Shutdown { .. }
        ));
        assert_eq!(receiver.try_recv().unwrap().at.monotonic, MonoTime(1_000));

        // A producer entering after quiescence sees the terminal state and
        // cannot append another ordinary event to the already-drained queue.
        assert!(capture.begin(&shutdown).is_none());
        assert!(capture.quiescent());
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn shutdown_during_capture_retains_full_queue_loss_before_becoming_quiescent() {
        let capture = CaptureState::new();
        let shutdown = ShutdownState::new();
        let dropped = AtomicU64::new(u64::MAX);
        let (sender, receiver) = mpsc::sync_channel(1);
        try_send(&sender, event(500), &dropped);
        let (captured_tx, captured_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        std::thread::scope(|scope| {
            let capture_ref = &capture;
            let shutdown_ref = &shutdown;
            let dropped_ref = &dropped;
            let producer_sender = sender.clone();
            let producer = scope.spawn(move || {
                let _guard = capture_ref.begin(shutdown_ref).unwrap();
                let observation = event(1_000);
                captured_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                try_send(&producer_sender, observation, dropped_ref);
            });

            captured_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(shutdown.request(TimePoint::new(2_000, 12_000), None));
            try_send(&sender, shutdown.event().unwrap(), &dropped);
            assert!(!capture.quiescent());
            release_tx.send(()).unwrap();
            producer.join().unwrap();
        });

        assert!(capture.quiescent());
        assert_eq!(take_dropped(&dropped), Some(MonoTime(1_000)));
        assert_eq!(receiver.try_recv().unwrap().at.monotonic, MonoTime(500));
        assert!(capture.begin(&shutdown).is_none());
        assert!(capture.quiescent());
    }

    #[test]
    fn capture_is_rejected_while_terminal_snapshot_is_being_published() {
        let capture = CaptureState::new();
        let shutdown = ShutdownState::new();
        shutdown.state.store(1, Ordering::SeqCst);
        assert!(capture.begin(&shutdown).is_none());
        assert!(capture.quiescent());
    }
}
