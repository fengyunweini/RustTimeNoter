//! Bounded capture -> event-time accounting -> durable storage.
//! Windows callbacks never wait for workers or disk.
use super::accounting::{Accounting, Observation, ObservationKind, Output};
use super::aggregator::{AppKey, MonoTime, TimePoint};
use super::hook::{self, HookEvent, HookKind, MessageWindow, WinHook, WindowSnapshot};
use super::resolver;
use crate::config::Config;
use crate::paths::{AppPaths, InstallScope};
use crate::storage::writer::{self, WriterConfig, WriterMsg};
use std::io;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{BOOL, HANDLE};
use windows_sys::Win32::System::Console::{
    SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT,
    CTRL_SHUTDOWN_EVENT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentThreadId, SetEvent, WaitForSingleObject, INFINITE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW, TranslateMessage, MSG,
    PM_NOREMOVE, WM_QUIT,
};

const EVENT_CAPACITY: usize = 1_024;
const WRITER_CAPACITY: usize = 256;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

pub fn run(scope: InstallScope) -> io::Result<()> {
    // Service control can request a stop before any capture sender or named
    // event exists. The reserved terminal snapshot is the durable stop intent.
    if hook::shutdown_requested() {
        return Ok(());
    }
    let paths = AppPaths::for_scope(scope)?;
    let _mutex = single_instance_guard()?;
    paths.ensure_dirs()?;
    let cfg = Config::load(&paths.config_file)?;
    let main_tid = unsafe { GetCurrentThreadId() };
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
    }
    let _console = CtrlHandlerGuard::install(main_tid)?;
    let stop = Arc::new(create_stop_event()?);
    let (writer_tx, writer_rx) = mpsc::sync_channel(WRITER_CAPACITY);
    let writer_cfg = WriterConfig {
        paths,
        scope,
        flush_block_records: cfg.flush_block_records.max(1),
        flush_interval_secs: cfg.flush_interval_secs.max(1),
    };
    let writer = thread::Builder::new()
        .name("rtn-writer".into())
        .spawn(move || {
            let _quit = QuitOnExit(main_tid);
            writer::run(writer_cfg, writer_rx)
        })?;
    if let Err(error) = flush_and_wait(&writer_tx) {
        let _ = send_until(&writer_tx, WriterMsg::Shutdown);
        let _ = join_until(writer, "writer");
        return Err(error);
    }
    if hook::shutdown_requested() {
        let sent = send_until(&writer_tx, WriterMsg::Shutdown);
        let completed = join_until(writer, "writer");
        completed?;
        return sent;
    }
    let (event_tx, event_rx) = mpsc::sync_channel(EVENT_CAPACITY);
    if let Err(error) = hook::set_sender(event_tx) {
        let _ = send_until(&writer_tx, WriterMsg::Shutdown);
        let _ = join_until(writer, "writer");
        return Err(error);
    }
    let consumer_writer = writer_tx.clone();
    let consumer_cfg = cfg.clone();
    let consumer = match thread::Builder::new()
        .name("rtn-accounting".into())
        .spawn(move || {
            let _quit = QuitOnExit(main_tid);
            accounting_loop(consumer_cfg, event_rx, consumer_writer)
        }) {
        Ok(worker) => worker,
        Err(error) => {
            let _ = send_until(&writer_tx, WriterMsg::Shutdown);
            let _ = join_until(writer, "writer");
            return Err(error);
        }
    };
    let waiter_stop = Arc::clone(&stop);
    let waiter = thread::Builder::new()
        .name("rtn-stop-wait".into())
        .spawn(move || {
            unsafe {
                WaitForSingleObject(waiter_stop.0, INFINITE);
            }
            // Preserve and wake accounting before relying on the main UI
            // queue. A delayed message pump must not delay the final flush.
            hook::request_shutdown();
            post_quit(main_tid);
            Ok(())
        });
    let loop_result = if waiter.is_ok() {
        (|| {
            if hook::shutdown_requested() {
                return Ok(());
            }
            let _hook = WinHook::install(cfg.capture_titles)?;
            let _window =
                MessageWindow::create(cfg.idle_tick_secs.min(cfg.flush_interval_secs).max(1))?;
            hook::publish_sample();
            let tray = std::env::current_exe().ok().and_then(|exe| {
                AppPaths::for_scope(scope)
                    .ok()
                    .and_then(|paths| super::tray::spawn(exe, paths.root))
            });
            let result = message_loop();
            // Freeze the accounting boundary before optional UI cleanup. The
            // independent consumer can now persist it while the tray closes.
            hook::request_shutdown();
            if let Some(tray) = tray {
                tray.shutdown();
            }
            result
        })()
    } else {
        Err(io::Error::other("cannot start stop waiter"))
    };
    // Also cover initialization/message-loop failures. Repeated requests keep
    // the first terminal boundary, and the admission barrier stops new capture.
    hook::request_shutdown();
    let consumer_result = join_until(consumer, "accounting");
    let _ = send_until(&writer_tx, WriterMsg::Shutdown);
    let writer_result = join_until(writer, "writer");
    unsafe {
        SetEvent(stop.0);
    }
    if let Ok(waiter) = waiter {
        let _ = join_until(waiter, "stop waiter");
    }
    writer_result?;
    consumer_result?;
    loop_result
}

fn accounting_loop(
    cfg: Config,
    receiver: mpsc::Receiver<HookEvent>,
    writer: SyncSender<WriterMsg>,
) -> io::Result<()> {
    let mut accounting = Accounting::new(cfg.afk_threshold_secs());
    let session_start = hook::sample().at;
    let mut resolver = CaptureResolver {
        cfg: &cfg,
        windows: resolver::Resolver::default(),
    };
    let mut discarded: Option<(super::aggregator::TimePoint, super::aggregator::TimePoint)> = None;
    loop {
        let now = MonoTime::from_millis(crate::platform::windows::monotonic_millis());
        let wait = accounting
            .next_wait(now)
            .unwrap_or(Duration::from_secs(86_400));
        let mut event = receive_event(&receiver, wait, hook::shutdown_event())?;
        let terminal = event
            .filter(|event| matches!(event.kind, HookKind::Shutdown { .. }))
            .or_else(hook::shutdown_event);
        if let Some(terminal) = terminal {
            return finish_capture(
                &mut resolver,
                &receiver,
                &writer,
                &mut accounting,
                terminal,
                event,
                session_start,
            );
        }
        if let Some(from) = hook::take_overflow() {
            let mut current = hook::sample();
            if let Some(terminal) = hook::shutdown_event() {
                current.at = terminal.at;
            }
            let from = event
                .as_ref()
                .filter(|event| event.at.monotonic < from.monotonic)
                .map(|event| event.at)
                .unwrap_or(from);
            emit(&writer, accounting.invalidate(from, current.at))?;
            discarded = Some((from, current.at));
            // Stale queued events cannot restart tracking before this sample.
            if !hook::shutdown_requested() {
                emit(&writer, accounting.push(resolver.observation(current)))?;
            }
        }
        if let (Some((from, to)), Some(queued)) = (discarded, event.as_ref()) {
            if queued.at.monotonic < to.monotonic
                && !matches!(queued.kind, HookKind::Shutdown { .. })
            {
                if queued.at.monotonic < from.monotonic {
                    emit(&writer, accounting.invalidate(queued.at, to))?;
                    discarded = Some((queued.at, to));
                    if !hook::shutdown_requested() {
                        emit(
                            &writer,
                            accounting.push(resolver.observation(hook::sample())),
                        )?;
                    }
                }
                event = None;
            }
        }
        if let Some(event) = event {
            emit(&writer, accounting.push(resolver.observation(event)))?;
        }
        if let Some(terminal) = hook::shutdown_event() {
            return finish_capture(
                &mut resolver,
                &receiver,
                &writer,
                &mut accounting,
                terminal,
                None,
                session_start,
            );
        }
        let now = MonoTime::from_millis(crate::platform::windows::monotonic_millis());
        let output = accounting.drain_ready(now);
        emit(&writer, output)?;
    }
}

fn receive_event(
    receiver: &mpsc::Receiver<HookEvent>,
    wait: Duration,
    terminal: Option<HookEvent>,
) -> io::Result<Option<HookEvent>> {
    // A shutdown requested before SENDER registration has no queue wakeup.
    // Check its reserved snapshot before a potentially long idle receive.
    if let Some(terminal) = terminal {
        return Ok(Some(terminal));
    }
    match receiver.recv_timeout(wait) {
        Ok(event) => Ok(Some(event)),
        Err(RecvTimeoutError::Timeout) => Ok(None),
        Err(RecvTimeoutError::Disconnected) => {
            Err(io::Error::other("capture disconnected without shutdown"))
        }
    }
}

fn finish_capture(
    resolver: &mut CaptureResolver<'_>,
    receiver: &mpsc::Receiver<HookEvent>,
    writer: &SyncSender<WriterMsg>,
    accounting: &mut Accounting,
    terminal: HookEvent,
    first: Option<HookEvent>,
    session_start: TimePoint,
) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut first = first;
    let mut uncertain_start = session_start;
    let mut timed_out = false;
    loop {
        // Once quiescent, shutdown's publication prevents any future accepted
        // observations. Drain after this check to include the final sender.
        let quiescent = hook::capture_quiescent();
        while let Some(queued) = first.take().or_else(|| receiver.try_recv().ok()) {
            if queued.at.monotonic < uncertain_start.monotonic {
                uncertain_start = queued.at;
            }
            if queued.at.monotonic <= terminal.at.monotonic
                && !matches!(queued.kind, HookKind::Shutdown { .. })
            {
                emit(writer, accounting.push(resolver.observation(queued)))?;
            }
        }
        if quiescent {
            break;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    if let Some(from) = hook::take_overflow() {
        emit(writer, accounting.invalidate(from, terminal.at))?;
    }
    if timed_out {
        emit(writer, accounting.invalidate(uncertain_start, terminal.at))?;
    }
    let last_input = match terminal.kind {
        HookKind::Shutdown { last_input } => last_input,
        _ => None,
    };
    emit(writer, accounting.finish(terminal.at, last_input))?;
    hook::mark_shutdown_complete();
    if timed_out {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "capture did not settle before shutdown; session marked incomplete",
        ))
    } else {
        Ok(())
    }
}

struct CaptureResolver<'a> {
    cfg: &'a Config,
    windows: resolver::Resolver,
}

impl CaptureResolver<'_> {
    fn observation(&mut self, event: HookEvent) -> Observation {
        observation_from_hook(event, |window| self.window(window))
    }
    fn window(&mut self, window: WindowSnapshot) -> Option<AppKey> {
        let cfg = self.cfg;
        let mut app = self.windows.resolve_window(
            window,
            cfg.capture_titles,
            (cfg.title_max_chars as usize).min(4_096),
        )?;
        if cfg.title_blacklisted(&app.basename) {
            app.title = None;
        }
        Some(app)
    }
}

fn observation_from_hook(
    event: HookEvent,
    mut resolve: impl FnMut(WindowSnapshot) -> Option<AppKey>,
) -> Observation {
    // Resolution can fail or map distinct windows to the same executable.
    // Keep the captured identity regardless; only accounting has event-time
    // foreground history and can decide whether a title belongs to it.
    let (window, kind) = match event.kind {
        HookKind::Foreground { window, last_input } => (
            window,
            ObservationKind::Foreground {
                app: resolve(window),
                last_input,
            },
        ),
        HookKind::Title { window, last_input } => (
            window,
            ObservationKind::Title {
                app: resolve(window),
                last_input,
            },
        ),
        HookKind::Sample {
            window,
            last_input,
            locked,
            suspended,
        } => (
            window.unwrap_or_default(),
            ObservationKind::Sample {
                app: window.and_then(&mut resolve),
                last_input,
                locked,
                suspended,
            },
        ),
        HookKind::Lock(value) => (WindowSnapshot::default(), ObservationKind::Lock(value)),
        HookKind::Suspend(value) => (WindowSnapshot::default(), ObservationKind::Suspend(value)),
        HookKind::Shutdown { .. } => unreachable!("handled by accounting_loop"),
    };
    Observation {
        at: event.at,
        window,
        kind,
    }
}

fn emit(writer: &SyncSender<WriterMsg>, output: Output) -> io::Result<()> {
    // Corrections precede estimates: a crash between writes must never make a
    // known-unreliable estimate look trustworthy.
    let has_gaps = !output.gaps.is_empty();
    let has_segments = !output.segments.is_empty();
    for gap in output.gaps {
        send_until(
            writer,
            WriterMsg::Gap {
                start_unix: gap.start_unix,
                end_unix: gap.end_unix,
            },
        )?;
    }
    // A subsequent dictionary or segment write can fail. Make the correction
    // durable before attempting those writes, including corrections from push.
    if has_gaps {
        flush_and_wait(writer)?;
    }
    for segment in output.segments {
        send_until(writer, WriterMsg::Segment(segment))?;
    }
    if output.checkpoint && (!has_gaps || has_segments) {
        flush_and_wait(writer)?;
    }
    Ok(())
}
fn flush_and_wait(writer: &SyncSender<WriterMsg>) -> io::Result<()> {
    let (ack, result) = mpsc::channel();
    send_until(writer, WriterMsg::FlushAndAck(ack))?;
    match result.recv_timeout(SHUTDOWN_TIMEOUT) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(io::Error::other(error)),
        Err(error) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("durable write not confirmed: {error}"),
        )),
    }
}
fn send_until<T>(sender: &SyncSender<T>, mut value: T) -> io::Result<()> {
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    loop {
        match sender.try_send(value) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Disconnected(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "worker disconnected",
                ))
            }
            Err(TrySendError::Full(returned)) => value = returned,
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "worker queue stalled",
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }
}
fn join_until(worker: JoinHandle<io::Result<()>>, name: &str) -> io::Result<()> {
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    while !worker.is_finished() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{name} did not finish"),
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }
    worker
        .join()
        .map_err(|_| io::Error::other(format!("{name} panicked")))?
}
fn message_loop() -> io::Result<()> {
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        loop {
            match GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) {
                0 => return Ok(()),
                -1 => return Err(io::Error::last_os_error()),
                _ => {}
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
fn post_quit(tid: u32) {
    unsafe {
        PostThreadMessageW(tid, WM_QUIT, 0, 0);
    }
}
struct QuitOnExit(u32);
impl Drop for QuitOnExit {
    fn drop(&mut self) {
        post_quit(self.0);
    }
}
struct StopEvent(HANDLE);
unsafe impl Send for StopEvent {}
unsafe impl Sync for StopEvent {}
impl Drop for StopEvent {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
fn create_stop_event() -> io::Result<StopEvent> {
    let name: Vec<_> = crate::stop_event_name()
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let h = unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) };
    if h.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(StopEvent(h))
    }
}
pub fn signal_stop() -> io::Result<bool> {
    let name: Vec<_> = crate::stop_event_name()
        .encode_utf16()
        .chain(Some(0))
        .collect();
    signal_stop_named(&name)
}

fn signal_stop_named(name: &[u16]) -> io::Result<bool> {
    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows_sys::Win32::System::Threading::{OpenEventW, EVENT_MODIFY_STATE};
    let h = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    if h.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) {
            Ok(false)
        } else {
            Err(error)
        };
    }
    let event = StopEvent(h);
    if unsafe { SetEvent(event.0) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(true)
    }
}
static MAIN_THREAD_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
struct CtrlHandlerGuard;
impl CtrlHandlerGuard {
    fn install(tid: u32) -> io::Result<Self> {
        MAIN_THREAD_ID.store(tid, std::sync::atomic::Ordering::SeqCst);
        if unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self)
    }
}
impl Drop for CtrlHandlerGuard {
    fn drop(&mut self) {
        unsafe {
            SetConsoleCtrlHandler(Some(ctrl_handler), 0);
        }
        MAIN_THREAD_ID.store(0, std::sync::atomic::Ordering::SeqCst);
    }
}
unsafe extern "system" fn ctrl_handler(kind: u32) -> BOOL {
    match kind {
        CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT
        | CTRL_SHUTDOWN_EVENT => {
            if matches!(
                kind,
                CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT
            ) {
                hook::request_shutdown();
            }
            post_quit(MAIN_THREAD_ID.load(std::sync::atomic::Ordering::SeqCst));
            if matches!(
                kind,
                CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT
            ) {
                hook::wait_shutdown_complete(Duration::from_secs(4));
            }
            1
        }
        _ => 0,
    }
}
struct MutexGuard(HANDLE);
impl Drop for MutexGuard {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
fn single_instance_guard() -> io::Result<MutexGuard> {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let name: Vec<_> = crate::daemon_mutex_name()
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let h = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if h.is_null() {
        return Err(io::Error::last_os_error());
    }
    let guard = MutexGuard(h);
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "daemon already running",
        ));
    }
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::model::{Gap, Segment};

    #[test]
    fn unresolved_windows_retain_capture_identity_and_time() {
        let window = WindowSnapshot { hwnd: 17, pid: 42 };
        let at = TimePoint::new(1_234, 9_876);
        let last_input = Some(MonoTime(1_200));
        for kind in [
            HookKind::Foreground { window, last_input },
            HookKind::Title { window, last_input },
            HookKind::Sample {
                window: Some(window),
                last_input,
                locked: false,
                suspended: false,
            },
        ] {
            let mut calls = 0;
            let observation = observation_from_hook(HookEvent { at, kind }, |captured| {
                assert_eq!(captured, window);
                calls += 1;
                None
            });
            assert_eq!(calls, 1);
            assert_eq!(observation.window, window);
            assert_eq!(observation.at, at);
            let (app, observed_input) = match observation.kind {
                ObservationKind::Foreground { app, last_input }
                | ObservationKind::Title { app, last_input }
                | ObservationKind::Sample {
                    app, last_input, ..
                } => (app, last_input),
                other => panic!("unexpected conversion: {other:?}"),
            };
            assert!(app.is_none());
            assert_eq!(observed_input, last_input);
        }
    }

    #[test]
    fn same_process_windows_remain_distinct_in_out_of_order_conversion() {
        let first = WindowSnapshot { hwnd: 17, pid: 42 };
        let second = WindowSnapshot { hwnd: 18, pid: 42 };
        let app = AppKey {
            path: "same.exe".into(),
            basename: "same.exe".into(),
            title: Some("same title".into()),
        };
        // Arrival order cannot decide which window was foreground at t=10.
        // The conversion must preserve both observations for accounting.
        let foreground = observation_from_hook(
            HookEvent {
                at: TimePoint::new(20_000, 20_000),
                kind: HookKind::Foreground {
                    window: second,
                    last_input: Some(MonoTime(20_000)),
                },
            },
            |_| Some(app.clone()),
        );
        let title = observation_from_hook(
            HookEvent {
                at: TimePoint::new(10_000, 10_000),
                kind: HookKind::Title {
                    window: first,
                    last_input: Some(MonoTime(10_000)),
                },
            },
            |_| Some(app.clone()),
        );
        assert_eq!(foreground.window, second);
        assert_eq!(title.window, first);
        assert_ne!(foreground.window, title.window);
        assert!(title.at.monotonic < foreground.at.monotonic);
        assert!(matches!(
            foreground.kind,
            ObservationKind::Foreground { app: Some(observed), .. } if observed == app
        ));
        assert!(matches!(
            title.kind,
            ObservationKind::Title { app: Some(observed), .. } if observed == app
        ));
    }

    #[test]
    fn controls_and_missing_samples_have_unknown_window_without_resolution() {
        let at = TimePoint::new(10_000, 20_000);
        for kind in [
            HookKind::Lock(true),
            HookKind::Suspend(false),
            HookKind::Sample {
                window: None,
                last_input: None,
                locked: true,
                suspended: false,
            },
        ] {
            let observation = observation_from_hook(HookEvent { at, kind }, |_| {
                panic!("a control or absent window cannot be resolved")
            });
            assert_eq!(observation.window, WindowSnapshot::default());
            assert_eq!(observation.at, at);
            match (kind, observation.kind) {
                (HookKind::Lock(expected), ObservationKind::Lock(actual))
                | (HookKind::Suspend(expected), ObservationKind::Suspend(actual)) => {
                    assert_eq!(expected, actual);
                }
                (
                    HookKind::Sample { .. },
                    ObservationKind::Sample {
                        app: None,
                        last_input: None,
                        locked: true,
                        suspended: false,
                    },
                ) => {}
                other => panic!("unexpected conversion: {other:?}"),
            }
        }
    }

    #[test]
    fn terminal_sample_corrects_a_missed_foreground_without_losing_the_valid_prefix() {
        let sample = |name: &str, seconds: u64| Observation {
            at: TimePoint::new(seconds * 1_000, seconds * 1_000),
            window: WindowSnapshot {
                hwnd: if name == "a.exe" { 1 } else { 2 },
                pid: 1,
            },
            kind: ObservationKind::Sample {
                app: Some(AppKey {
                    path: name.into(),
                    basename: name.into(),
                    title: None,
                }),
                last_input: Some(MonoTime(seconds * 1_000)),
                locked: false,
                suspended: false,
            },
        };
        let mut accounting = Accounting::new(60);
        accounting.push(sample("a.exe", 0));
        accounting.drain_ready(MonoTime(2_000));
        accounting.push(sample("a.exe", 10));
        let prefix = accounting.drain_ready(MonoTime(12_000));
        assert_eq!(prefix.segments.len(), 1);
        assert_eq!(prefix.segments[0].start_unix, 0);
        assert_eq!(prefix.segments[0].end_unix, 10);

        // B's WinEvent has not been dispatched; the final reliable snapshot
        // detects the mismatch before the terminal boundary closes accounting.
        accounting.push(sample("b.exe", 20));
        let final_output =
            accounting.finish(TimePoint::new(20_000, 20_000), Some(MonoTime(20_000)));
        assert_eq!(
            final_output.gaps,
            vec![Gap {
                start_unix: 10,
                end_unix: 20
            }]
        );
        // Append-only correction may retain an estimate for A, but every
        // second of it must be covered by the Gap and excluded by queries.
        // No activity is backfilled to the newly discovered B.
        assert!(final_output.segments.iter().all(|segment| {
            segment.app_path == "a.exe" && segment.start_unix >= 10 && segment.end_unix <= 20
        }));
        assert_eq!(
            prefix.segments.iter().map(Segment::duration).sum::<u64>(),
            10
        );
        assert!(final_output.checkpoint);
    }

    #[test]
    fn stop_probe_distinguishes_absent_event_from_wrong_object_type() {
        use windows_sys::Win32::System::Threading::CreateMutexW;

        // Private per-test objects never share names with a real daemon.
        let name: Vec<u16> = format!(
            "Local\\RustTimeNoter.StopProbe.{}.{}",
            std::process::id(),
            crate::platform::windows::monotonic_millis()
        )
        .encode_utf16()
        .chain(Some(0))
        .collect();
        assert!(!signal_stop_named(&name).unwrap());

        let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        assert!(!mutex.is_null());
        let mutex = MutexGuard(mutex);
        assert!(signal_stop_named(&name).is_err());
        drop(mutex);

        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) };
        assert!(!event.is_null());
        let event = StopEvent(event);
        assert!(signal_stop_named(&name).unwrap());
        assert_eq!(unsafe { WaitForSingleObject(event.0, 0) }, 0);
    }

    #[test]
    fn reserved_shutdown_does_not_need_a_capture_queue_wakeup() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let terminal = HookEvent {
            at: TimePoint::new(2_000, 2_000),
            kind: HookKind::Shutdown {
                last_input: Some(MonoTime(1_000)),
            },
        };
        // No hook sender existed when the service stop was requested.
        let event = receive_event(&receiver, Duration::ZERO, Some(terminal))
            .unwrap()
            .unwrap();
        assert!(matches!(event.kind, HookKind::Shutdown { .. }));
        assert_eq!(event.at, terminal.at);

        // The terminal shortcut leaves earlier observations for final drain.
        sender
            .send(HookEvent {
                at: TimePoint::new(1_000, 1_000),
                kind: HookKind::Lock(true),
            })
            .unwrap();
        assert!(matches!(
            receive_event(&receiver, Duration::ZERO, Some(terminal))
                .unwrap()
                .unwrap()
                .kind,
            HookKind::Shutdown { .. }
        ));
        assert!(matches!(
            receiver.try_recv().unwrap().kind,
            HookKind::Lock(true)
        ));
    }

    #[test]
    fn disconnected_capture_without_a_terminal_remains_an_error() {
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(sender);
        assert!(receive_event(&receiver, Duration::ZERO, None).is_err());
    }

    fn corrected_output() -> Output {
        Output {
            gaps: vec![Gap {
                start_unix: 10,
                end_unix: 20,
            }],
            segments: vec![Segment {
                app_path: "app.exe".into(),
                app_basename: "app.exe".into(),
                title: None,
                start_unix: 0,
                end_unix: 30,
            }],
            checkpoint: true,
        }
    }

    #[test]
    fn correction_is_durable_before_following_activity_and_checkpoint() {
        let (tx, rx) = mpsc::sync_channel(8);
        let worker = thread::spawn(move || {
            assert!(matches!(rx.recv().unwrap(), WriterMsg::Gap { .. }));
            let WriterMsg::FlushAndAck(ack) = rx.recv().unwrap() else {
                panic!("missing gap barrier")
            };
            assert!(
                rx.try_recv().is_err(),
                "activity sent before correction confirmation"
            );
            ack.send(Ok(())).unwrap();
            assert!(matches!(rx.recv().unwrap(), WriterMsg::Segment(_)));
            let WriterMsg::FlushAndAck(ack) = rx.recv().unwrap() else {
                panic!("missing checkpoint barrier")
            };
            ack.send(Ok(())).unwrap();
        });
        emit(&tx, corrected_output()).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn failed_correction_stops_before_any_following_activity() {
        let (tx, rx) = mpsc::sync_channel(8);
        let worker = thread::spawn(move || {
            assert!(matches!(rx.recv().unwrap(), WriterMsg::Gap { .. }));
            let WriterMsg::FlushAndAck(ack) = rx.recv().unwrap() else {
                panic!("missing gap barrier")
            };
            ack.send(Err("disk failure".into())).unwrap();
            assert!(rx.recv().is_err());
        });
        assert!(emit(&tx, corrected_output()).is_err());
        drop(tx);
        worker.join().unwrap();
    }
}
