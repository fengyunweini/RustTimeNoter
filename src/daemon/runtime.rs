//! Convert bounded Win32 callback events into aggregator events and writer work.

#![cfg(windows)]

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{BOOL, HANDLE};
use windows_sys::Win32::System::Console::{
    SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT,
    CTRL_SHUTDOWN_EVENT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentThreadId, SetEvent, WaitForSingleObject, INFINITE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PeekMessageW, PostQuitMessage, PostThreadMessageW,
    TranslateMessage, MSG, PM_NOREMOVE, WM_QUIT,
};

use super::aggregator::{Aggregator, AppKey, Event, MonoTime, TimePoint};
use super::event_queue::{
    self, EventReceiver, ForegroundEvent, HookEvent, TitleEvent, WindowId, TIMELINE_CAPACITY,
};
use super::hook::{self, MessageWindow, WinEventThread};
use super::resolver;
use crate::config::Config;
use crate::paths::{AppPaths, InstallScope};
use crate::platform::windows as platform;
use crate::storage::writer::{self, WriterConfig, WriterMsg};

const WRITER_QUEUE_CAPACITY: usize = 256;

pub fn run(scope: InstallScope) -> std::io::Result<()> {
    let _mutex = single_instance_guard()?;

    let paths = AppPaths::for_scope(scope)?;
    paths.ensure_dirs()?;
    let cfg = Config::load(&paths.config_file)?;

    let main_thread_id = unsafe { GetCurrentThreadId() };
    initialize_thread_message_queue();
    let _ctrl_handler = CtrlHandlerGuard::install(main_thread_id)?;
    let stop_event = Arc::new(create_stop_event()?);

    // Bound segment payloads (which can include titles) and propagate storage
    // backpressure through the aggregator to the already-bounded event queue.
    let (writer_tx, writer_rx) = mpsc::sync_channel::<WriterMsg>(WRITER_QUEUE_CAPACITY);
    let writer_cfg = WriterConfig {
        paths: paths.clone(),
        scope,
        flush_block_records: cfg.flush_block_records.max(1),
        flush_interval_secs: cfg.flush_interval_secs.max(1),
    };
    let writer_handle = thread::Builder::new()
        .name("rtn-writer".into())
        .spawn(move || {
            let _quit_on_exit = QuitOnThreadExit(main_thread_id);
            let result = writer::run(writer_cfg, writer_rx);
            if let Err(error) = &result {
                eprintln!("[rtn] writer error: {error}");
            }
            result
        })?;

    // `writer::run` opens/repairs the key, dictionaries, and storage before it
    // receives this barrier. An ACK therefore proves initialization succeeded.
    if let Err(barrier_error) = flush_writer_and_wait(&writer_tx) {
        let _ = writer_tx.send(WriterMsg::Shutdown);
        return match join_worker(writer_handle, "writer") {
            Err(writer_error) => Err(writer_error),
            Ok(()) => Err(barrier_error),
        };
    }

    let (hook_tx, hook_rx) = event_queue::channel(TIMELINE_CAPACITY);
    if let Err(error) = hook::set_sender(hook_tx) {
        let _ = writer_tx.send(WriterMsg::Shutdown);
        let writer_result = join_worker(writer_handle, "writer");
        report_cleanup_error("writer", &writer_result);
        return Err(error);
    }

    let win_event_thread = match WinEventThread::start(cfg.capture_titles, main_thread_id) {
        Ok(thread) => thread,
        Err(error) => {
            drop(hook_rx);
            let _ = writer_tx.send(WriterMsg::Shutdown);
            let writer_result = join_worker(writer_handle, "writer");
            report_cleanup_error("writer", &writer_result);
            return Err(error);
        }
    };

    let message_window = match MessageWindow::create(cfg.idle_tick_secs) {
        Ok(window) => window,
        Err(error) => {
            // No aggregator owns the receiver yet. Disconnect it before
            // joining so a WinEvent callback blocked on bounded capacity wakes.
            drop(hook_rx);
            let hook_result = win_event_thread.shutdown();
            report_cleanup_error("WinEvent thread", &hook_result);
            let _ = writer_tx.send(WriterMsg::Shutdown);
            let writer_result = join_worker(writer_handle, "writer");
            report_cleanup_error("writer", &writer_result);
            return Err(error);
        }
    };

    let aggregator_spawn = {
        let aggregator_cfg = cfg.clone();
        let aggregator_writer = writer_tx.clone();
        thread::Builder::new()
            .name("rtn-aggr".into())
            .spawn(move || {
                let _quit_on_exit = QuitOnThreadExit(main_thread_id);
                let result = aggregator_loop(aggregator_cfg, hook_rx, aggregator_writer);
                if let Err(error) = &result {
                    eprintln!("[rtn] aggregator error: {error}");
                }
                result
            })
    };
    let aggregator_handle = match aggregator_spawn {
        Ok(handle) => handle,
        Err(error) => {
            drop(message_window);
            // The failed spawn drops its closure and therefore the receiver,
            // unblocking any callback before this join.
            let hook_result = win_event_thread.shutdown();
            report_cleanup_error("WinEvent thread", &hook_result);
            let _ = writer_tx.send(WriterMsg::Shutdown);
            let writer_result = join_worker(writer_handle, "writer");
            report_cleanup_error("writer", &writer_result);
            return Err(error);
        }
    };

    let tray_handle = std::env::current_exe()
        .ok()
        .and_then(|exe| super::tray::spawn(exe, paths.root.clone()));

    let waiter_event = Arc::clone(&stop_event);
    let stop_waiter =
        match thread::Builder::new()
            .name("rtn-stop-wait".into())
            .spawn(move || unsafe {
                WaitForSingleObject(waiter_event.0, INFINITE);
                PostThreadMessageW(main_thread_id, WM_QUIT, 0, 0);
            }) {
            Ok(handle) => handle,
            Err(error) => {
                drop(message_window);
                if let Some(tray) = tray_handle {
                    tray.shutdown();
                }
                let hook_result = win_event_thread.shutdown();
                report_cleanup_error("WinEvent thread", &hook_result);
                hook::send_shutdown();
                let aggregator_result = join_worker(aggregator_handle, "aggregator");
                report_cleanup_error("aggregator", &aggregator_result);
                let _ = writer_tx.send(WriterMsg::Shutdown);
                let writer_result = join_worker(writer_handle, "writer");
                report_cleanup_error("writer", &writer_result);
                return Err(error);
            }
        };

    let message_loop_result = run_message_loop();

    drop(message_window);
    if let Some(tray) = tray_handle {
        tray.shutdown();
    }
    // Keep the aggregator alive while joining: a WinEvent callback may be
    // waiting for space in its bounded queue.
    let hook_result = win_event_thread.shutdown();
    hook::send_shutdown();
    let aggregator_result = join_worker(aggregator_handle, "aggregator");
    let _ = writer_tx.send(WriterMsg::Shutdown);
    let writer_result = join_worker(writer_handle, "writer");

    // Release the waiter even when shutdown originated from Ctrl+C, a writer
    // failure, or GetMessageW rather than the named stop event.
    unsafe {
        SetEvent(stop_event.0);
    }
    let waiter_result = stop_waiter
        .join()
        .map_err(|_| std::io::Error::other("stop waiter thread panicked"));

    writer_result?;
    aggregator_result?;
    hook_result?;
    message_loop_result?;
    waiter_result
}

fn initialize_thread_message_queue() {
    unsafe {
        let mut message: MSG = std::mem::zeroed();
        PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
    }
}

fn run_message_loop() -> std::io::Result<()> {
    unsafe {
        let mut message: MSG = std::mem::zeroed();
        loop {
            let result = GetMessageW(&mut message, std::ptr::null_mut(), 0, 0);
            if result == 0 {
                return Ok(());
            }
            if result == -1 {
                return Err(std::io::Error::last_os_error());
            }
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

fn post_thread_quit(thread_id: u32) {
    unsafe {
        PostThreadMessageW(thread_id, WM_QUIT, 0, 0);
    }
}

/// Make worker failures, including unwinding panics in debug builds, wake the
/// main message loop instead of leaving the daemon hung in `GetMessageW`.
struct QuitOnThreadExit(u32);

impl Drop for QuitOnThreadExit {
    fn drop(&mut self) {
        post_thread_quit(self.0);
    }
}

fn join_worker(handle: thread::JoinHandle<std::io::Result<()>>, name: &str) -> std::io::Result<()> {
    handle
        .join()
        .map_err(|_| std::io::Error::other(format!("{name} thread panicked")))?
}

fn report_cleanup_error(component: &str, result: &std::io::Result<()>) {
    if let Err(error) = result {
        eprintln!("[rtn] {component} cleanup error: {error}");
    }
}

fn aggregator_loop(
    cfg: Config,
    receiver: EventReceiver,
    writer: mpsc::SyncSender<WriterMsg>,
) -> std::io::Result<()> {
    aggregator_loop_with_resolver(cfg, receiver, writer, &SystemWindowResolver)
}

fn aggregator_loop_with_resolver<R: WindowResolver>(
    cfg: Config,
    receiver: EventReceiver,
    writer: mpsc::SyncSender<WriterMsg>,
    resolver: &R,
) -> std::io::Result<()> {
    let mut aggregator = Aggregator::new(cfg.afk_threshold_secs());
    let mut state = RuntimeState::default();
    let clock = RuntimeClock;
    let checkpoint_interval = Duration::from_secs(cfg.flush_interval_secs.max(1) as u64);
    let mut checkpoint_deadline = next_checkpoint_deadline(checkpoint_interval);

    loop {
        let timeout = checkpoint_deadline.saturating_duration_since(Instant::now());
        let hook_event = match receiver.recv_timeout(timeout) {
            Ok(event) => Some(event),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                let (at, last_input, _) = clock.sample_activity();
                Some(HookEvent::Shutdown { at, last_input })
            }
        };

        if let Some(hook_event) = hook_event {
            if handle_hook_event(
                &cfg,
                &clock,
                &mut state,
                &mut aggregator,
                &writer,
                resolver,
                hook_event,
            )? {
                break;
            }
        } else {
            checkpoint_active(&clock, &mut state, &mut aggregator, &writer)?;
            checkpoint_deadline = next_checkpoint_deadline(checkpoint_interval);
            continue;
        }

        if Instant::now() >= checkpoint_deadline {
            checkpoint_active(&clock, &mut state, &mut aggregator, &writer)?;
            checkpoint_deadline = next_checkpoint_deadline(checkpoint_interval);
        }
    }
    Ok(())
}

trait WindowResolver {
    fn resolve_event(
        &self,
        event: &ForegroundEvent,
        capture_title: bool,
        title_max: usize,
    ) -> Option<AppKey>;

    fn resolve_current(
        &self,
        capture_title: bool,
        title_max: usize,
    ) -> Option<(WindowId, u32, AppKey)>;

    fn resolve_title(
        &self,
        window: WindowId,
        expected_pid: u32,
        title_max: usize,
    ) -> Option<String>;
}

struct SystemWindowResolver;

impl WindowResolver for SystemWindowResolver {
    fn resolve_event(
        &self,
        event: &ForegroundEvent,
        capture_title: bool,
        title_max: usize,
    ) -> Option<AppKey> {
        resolver::resolve_window(event.window, event.pid_at_event, capture_title, title_max)
    }

    fn resolve_current(
        &self,
        capture_title: bool,
        title_max: usize,
    ) -> Option<(WindowId, u32, AppKey)> {
        resolver::resolve_foreground(capture_title, title_max)
    }

    fn resolve_title(
        &self,
        window: WindowId,
        expected_pid: u32,
        title_max: usize,
    ) -> Option<String> {
        resolver::resolve_title(window, expected_pid, title_max)
    }
}

#[derive(Debug, Clone)]
struct ForegroundState {
    window: WindowId,
    pid_at_event: u32,
    generation: u64,
    app: AppKey,
}

#[derive(Debug, Clone, Copy)]
struct ForegroundObservation {
    window: WindowId,
    pid_at_event: u32,
    generation: u64,
}

#[derive(Debug, Default)]
struct RuntimeState {
    current: Option<ForegroundState>,
    /// Latest callback-time foreground identity, retained even when resolving
    /// its process image fails. This keeps the event-queue generation aligned
    /// with a later `resolve_current` recovery for the same HWND.
    latest_foreground: Option<ForegroundObservation>,
    global_last_input: Option<MonoTime>,
}

impl RuntimeState {
    fn observe_foreground(&mut self, event: &ForegroundEvent) {
        self.latest_foreground = Some(ForegroundObservation {
            window: event.window,
            pid_at_event: event.pid_at_event,
            generation: event.generation,
        });
    }

    fn observe_global_input(&mut self, last_input: MonoTime) {
        self.global_last_input = Some(
            self.global_last_input
                .map(|known| known.max(last_input))
                .unwrap_or(last_input),
        );
    }

    fn causal_global_input(&self, at: TimePoint) -> Option<MonoTime> {
        self.global_last_input
            .filter(|last_input| *last_input <= at.monotonic)
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_hook_event<R: WindowResolver>(
    cfg: &Config,
    clock: &RuntimeClock,
    state: &mut RuntimeState,
    aggregator: &mut Aggregator,
    writer: &mpsc::SyncSender<WriterMsg>,
    resolver: &R,
    hook_event: HookEvent,
) -> std::io::Result<bool> {
    let output = match hook_event {
        HookEvent::Foreground(event) => handle_foreground(cfg, state, aggregator, resolver, event),
        HookEvent::Title(event) => handle_title(cfg, state, aggregator, resolver, event),
        HookEvent::IdleTick { at, last_input } => {
            handle_idle_tick(cfg, state, aggregator, resolver, at, last_input)
        }
        HookEvent::SessionLock { at, last_input } => {
            if let Some(last_input) = last_input {
                state.observe_global_input(last_input);
            }
            let mut output = aggregator.handle_observed_idle(at, last_input);
            output.extend(aggregator.handle(Event::SessionLock { at }));
            output
        }
        HookEvent::SessionUnlock { at } => aggregator.handle(Event::SessionUnlock { at }),
        HookEvent::Suspend { at, last_input } => {
            if let Some(last_input) = last_input {
                state.observe_global_input(last_input);
            }
            let mut output = aggregator.handle_observed_idle(at, last_input);
            output.extend(aggregator.handle(Event::Suspend { at }));
            output
        }
        HookEvent::Resume { at } => aggregator.handle(Event::Resume { at }),
        HookEvent::Shutdown { at, last_input } => {
            if let Some(last_input) = last_input {
                state.observe_global_input(last_input);
            }
            let mut output = aggregator.handle_observed_idle(at, last_input);
            output.extend(aggregator.handle(Event::Shutdown { at }));
            emit_segments(writer, output)?;
            send_writer(writer, WriterMsg::Flush)?;
            return Ok(true);
        }
    };
    emit_segments(writer, output)?;

    // `clock` is intentionally retained here: shutdown/disconnect and
    // checkpoints use the same source, and root can extend this result-returning
    // loop with writer acknowledgements without changing event semantics.
    let _ = clock;
    Ok(false)
}

fn handle_foreground<R: WindowResolver>(
    cfg: &Config,
    state: &mut RuntimeState,
    aggregator: &mut Aggregator,
    resolver: &R,
    event: ForegroundEvent,
) -> Vec<crate::storage::Segment> {
    state.observe_foreground(&event);
    let resolved = resolver
        .resolve_event(&event, cfg.capture_titles, cfg.effective_title_max_chars())
        .map(|app| strip_blacklisted_title(cfg, app));

    let Some(app) = resolved else {
        if let Some(last_input) = event.last_input {
            state.observe_global_input(last_input);
        }
        state.current = None;
        let mut output = aggregator.handle_observed_idle(event.at, event.last_input);
        output.extend(aggregator.close_for_gap(event.at));
        return output;
    };

    if aggregator.is_active()
        && state
            .current
            .as_ref()
            .is_some_and(|current| current.app == app)
    {
        // A duplicate transition may identify a new HWND for the same AppKey,
        // but is not a segment boundary. A causal LASTINPUTINFO observation is
        // still real input and may advance the AFK clock.
        if let Some(last_input) = event.last_input {
            state.observe_global_input(last_input);
        }
        state.current = Some(ForegroundState {
            window: event.window,
            pid_at_event: event.pid_at_event,
            generation: event.generation,
            app,
        });
        return aggregator.handle_observed_idle(event.at, event.last_input);
    }

    let effective_last_input = event
        .last_input
        .or_else(|| state.causal_global_input(event.at));
    if let Some(last_input) = event.last_input {
        state.observe_global_input(last_input);
    }
    state.current = Some(ForegroundState {
        window: event.window,
        pid_at_event: event.pid_at_event,
        generation: event.generation,
        app: app.clone(),
    });
    aggregator.handle_observed_foreground(app, event.at, effective_last_input)
}

fn handle_title<R: WindowResolver>(
    cfg: &Config,
    state: &mut RuntimeState,
    aggregator: &mut Aggregator,
    resolver: &R,
    event: TitleEvent,
) -> Vec<crate::storage::Segment> {
    if !cfg.capture_titles {
        return Vec::new();
    }
    let Some(current) = state.current.as_ref() else {
        return Vec::new();
    };
    if current.window != event.window || current.generation != event.generation {
        return Vec::new();
    }

    // This is the title-only resolver path: it never opens a process.
    let title = if cfg.title_blacklisted(&current.app.basename) {
        None
    } else {
        resolver.resolve_title(
            event.window,
            current.pid_at_event,
            cfg.effective_title_max_chars(),
        )
    };
    let mut app = current.app.clone();
    app.title = title;
    if app == current.app {
        return Vec::new();
    }
    state.current = Some(ForegroundState {
        window: event.window,
        pid_at_event: current.pid_at_event,
        generation: event.generation,
        app: app.clone(),
    });
    // No LASTINPUTINFO sample is attached to a title event. The aggregator
    // preserves the previous real input timestamp.
    aggregator.handle_observed_foreground(app, event.at, None)
}

fn handle_idle_tick<R: WindowResolver>(
    cfg: &Config,
    state: &mut RuntimeState,
    aggregator: &mut Aggregator,
    resolver: &R,
    at: TimePoint,
    last_input: Option<MonoTime>,
) -> Vec<crate::storage::Segment> {
    if let Some(last_input) = last_input {
        state.observe_global_input(last_input);
    }
    let mut output = aggregator.handle_observed_idle(at, last_input);
    let afk_millis = cfg.afk_threshold_secs().max(1).saturating_mul(1_000);
    if last_input
        .is_some_and(|last_input| at.monotonic.elapsed_millis_since(last_input) < afk_millis)
        && !aggregator.is_active()
        && !aggregator.is_suppressed()
    {
        if let Some((window, pid, app)) =
            resolver.resolve_current(cfg.capture_titles, cfg.effective_title_max_chars())
        {
            let last_input = last_input.expect("checked above");
            let app = strip_blacklisted_title(cfg, app);
            let generation = state
                .latest_foreground
                .as_ref()
                .filter(|observed| observed.window == window && observed.pid_at_event == pid)
                .map(|observed| observed.generation)
                .unwrap_or(0);
            state.current = Some(ForegroundState {
                window,
                pid_at_event: pid,
                generation,
                app: app.clone(),
            });
            output.extend(aggregator.handle_observed_foreground(
                app,
                project_time_point(at, last_input),
                Some(last_input),
            ));
        }
    }
    output
}

fn checkpoint_active(
    clock: &RuntimeClock,
    state: &mut RuntimeState,
    aggregator: &mut Aggregator,
    writer: &mpsc::SyncSender<WriterMsg>,
) -> std::io::Result<()> {
    let (at, last_input, _) = clock.sample_activity();
    if let Some(last_input) = last_input {
        state.observe_global_input(last_input);
    }
    emit_segments(
        writer,
        aggregator.handle_observed_checkpoint(at, last_input),
    )?;
    flush_writer_and_wait(writer)
}

fn emit_segments(
    writer: &mpsc::SyncSender<WriterMsg>,
    segments: Vec<crate::storage::Segment>,
) -> std::io::Result<()> {
    for segment in segments {
        send_writer(writer, WriterMsg::Segment(segment))?;
    }
    Ok(())
}

fn send_writer(writer: &mpsc::SyncSender<WriterMsg>, message: WriterMsg) -> std::io::Result<()> {
    writer.send(message).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::BrokenPipe, "writer thread disconnected")
    })
}

fn flush_writer_and_wait(writer: &mpsc::SyncSender<WriterMsg>) -> std::io::Result<()> {
    let (acknowledge, result) = mpsc::channel();
    send_writer(writer, WriterMsg::FlushAndAck(acknowledge))?;
    match result.recv() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(message)) => Err(std::io::Error::other(format!(
            "writer flush failed: {message}"
        ))),
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "writer exited before acknowledging durable flush",
        )),
    }
}

fn next_checkpoint_deadline(interval: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(interval).unwrap_or(now)
}

#[derive(Debug, Clone, Copy)]
struct RuntimeClock;

impl RuntimeClock {
    fn sample(&self) -> TimePoint {
        let monotonic = platform::monotonic_millis();
        let wall_unix_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        TimePoint {
            monotonic: MonoTime::from_millis(monotonic),
            wall_unix_millis,
        }
    }

    fn sample_activity(&self) -> (TimePoint, Option<MonoTime>, Option<u64>) {
        let at = self.sample();
        let last_input = platform::last_input_monotonic_millis(at.monotonic.0)
            .map(MonoTime::from_millis)
            .filter(|last_input| *last_input <= at.monotonic);
        let idle_millis =
            last_input.map(|last_input| at.monotonic.elapsed_millis_since(last_input));
        (at, last_input, idle_millis)
    }
}

fn project_time_point(at: TimePoint, monotonic: MonoTime) -> TimePoint {
    let wall_unix_millis = if monotonic <= at.monotonic {
        at.wall_unix_millis
            .saturating_sub(at.monotonic.elapsed_millis_since(monotonic))
    } else {
        at.wall_unix_millis
            .saturating_add(monotonic.elapsed_millis_since(at.monotonic))
    };
    TimePoint {
        monotonic,
        wall_unix_millis,
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

fn create_stop_event() -> std::io::Result<StopEvent> {
    let name: Vec<u16> = crate::stop_event_name()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    Ok(StopEvent(handle))
}

pub fn signal_stop() -> std::io::Result<bool> {
    use windows_sys::Win32::System::Threading::{OpenEventW, SetEvent, EVENT_MODIFY_STATE};

    let name: Vec<u16> = crate::stop_event_name()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let handle = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
        if handle.is_null() {
            return Ok(false);
        }
        let signaled = SetEvent(handle);
        windows_sys::Win32::Foundation::CloseHandle(handle);
        Ok(signaled != 0)
    }
}

static MAIN_THREAD_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

struct CtrlHandlerGuard;

impl CtrlHandlerGuard {
    fn install(main_thread_id: u32) -> std::io::Result<Self> {
        MAIN_THREAD_ID.store(main_thread_id, std::sync::atomic::Ordering::SeqCst);
        if unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), 1) } == 0 {
            MAIN_THREAD_ID.store(0, std::sync::atomic::Ordering::SeqCst);
            return Err(std::io::Error::last_os_error());
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

unsafe extern "system" fn ctrl_handler(ctrl_type: u32) -> BOOL {
    match ctrl_type {
        CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT
        | CTRL_SHUTDOWN_EVENT => {
            let thread_id = MAIN_THREAD_ID.load(std::sync::atomic::Ordering::SeqCst);
            if thread_id != 0 {
                unsafe {
                    PostThreadMessageW(thread_id, WM_QUIT, 0, 0);
                }
            } else {
                unsafe {
                    PostQuitMessage(0);
                }
            }
            1
        }
        _ => 0,
    }
}

fn strip_blacklisted_title(cfg: &Config, mut app: AppKey) -> AppKey {
    if cfg.title_blacklisted(&app.basename) {
        app.title = None;
    }
    app
}

struct MutexGuard {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

impl Drop for MutexGuard {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

fn single_instance_guard() -> std::io::Result<MutexGuard> {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    let name: Vec<u16> = crate::daemon_mutex_name()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(handle);
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "another tracker daemon is running",
        ));
    }
    Ok(MutexGuard { handle })
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    use super::*;

    const WALL: u64 = 1_000_000;

    struct FakeResolver {
        apps: HashMap<WindowId, AppKey>,
        current: RefCell<Option<(WindowId, u32, AppKey)>>,
        seen_foreground: RefCell<Vec<WindowId>>,
        foreground_calls: Cell<usize>,
        title_calls: Cell<usize>,
        title: RefCell<Option<String>>,
    }

    impl FakeResolver {
        fn new(entries: &[(isize, &str)]) -> Self {
            let apps = entries
                .iter()
                .map(|(window, name)| (WindowId(*window), app(name)))
                .collect();
            Self {
                apps,
                current: RefCell::new(None),
                seen_foreground: RefCell::new(Vec::new()),
                foreground_calls: Cell::new(0),
                title_calls: Cell::new(0),
                title: RefCell::new(None),
            }
        }
    }

    impl WindowResolver for FakeResolver {
        fn resolve_event(
            &self,
            event: &ForegroundEvent,
            _capture_title: bool,
            _title_max: usize,
        ) -> Option<AppKey> {
            self.foreground_calls
                .set(self.foreground_calls.get().saturating_add(1));
            self.seen_foreground.borrow_mut().push(event.window);
            self.apps.get(&event.window).cloned()
        }

        fn resolve_current(
            &self,
            _capture_title: bool,
            _title_max: usize,
        ) -> Option<(WindowId, u32, AppKey)> {
            self.current.borrow().clone()
        }

        fn resolve_title(
            &self,
            _window: WindowId,
            _expected_pid: u32,
            _title_max: usize,
        ) -> Option<String> {
            self.title_calls
                .set(self.title_calls.get().saturating_add(1));
            self.title.borrow().clone()
        }
    }

    fn app(name: &str) -> AppKey {
        AppKey {
            path: format!("C:/{name}.exe"),
            basename: format!("{name}.exe"),
            title: None,
        }
    }

    fn foreground(
        window: isize,
        at_millis: u64,
        last_input: Option<u64>,
        generation: u64,
    ) -> ForegroundEvent {
        ForegroundEvent {
            window: WindowId(window),
            pid_at_event: window as u32 + 100,
            raw_event_millis: at_millis as u32,
            at: TimePoint::new(at_millis, WALL + at_millis),
            last_input: last_input.map(MonoTime::from_millis),
            generation,
        }
    }

    fn config(capture_titles: bool) -> Config {
        Config {
            capture_titles,
            ..Config::default()
        }
    }

    fn process<R: WindowResolver>(
        cfg: &Config,
        state: &mut RuntimeState,
        aggregator: &mut Aggregator,
        writer: &mpsc::SyncSender<WriterMsg>,
        resolver: &R,
        event: HookEvent,
    ) {
        assert!(!handle_hook_event(
            cfg,
            &RuntimeClock,
            state,
            aggregator,
            writer,
            resolver,
            event,
        )
        .unwrap());
    }

    #[test]
    fn delayed_a_b_c_resolves_each_event_window_in_fifo_order() {
        let cfg = config(false);
        let resolver = FakeResolver::new(&[(1, "a"), (2, "b"), (3, "c")]);
        let (sender, receiver) = event_queue::channel(4);
        sender
            .send_foreground(foreground(1, 0, Some(0), 0))
            .unwrap();
        sender
            .send_foreground(foreground(2, 2_000, Some(2_000), 0))
            .unwrap();
        sender
            .send_foreground(foreground(3, 4_000, Some(4_000), 0))
            .unwrap();

        let (writer, written) = mpsc::sync_channel(64);
        let mut state = RuntimeState::default();
        let mut aggregator = Aggregator::new(cfg.afk_threshold_secs());
        for _ in 0..3 {
            let event = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
            process(&cfg, &mut state, &mut aggregator, &writer, &resolver, event);
        }

        assert_eq!(
            *resolver.seen_foreground.borrow(),
            vec![WindowId(1), WindowId(2), WindowId(3)]
        );
        let basenames: Vec<_> = written
            .try_iter()
            .filter_map(|message| match message {
                WriterMsg::Segment(segment) => Some(segment.app_basename),
                _ => None,
            })
            .collect();
        assert_eq!(basenames, vec!["a.exe", "b.exe"]);
    }

    #[test]
    fn unresolvable_foreground_closes_active_and_leaves_a_gap() {
        let cfg = config(false);
        let resolver = FakeResolver::new(&[(1, "a"), (3, "c")]);
        let (writer, written) = mpsc::sync_channel(64);
        let mut state = RuntimeState::default();
        let mut aggregator = Aggregator::new(cfg.afk_threshold_secs());

        for event in [
            foreground(1, 0, Some(0), 1),
            foreground(2, 2_000, Some(2_000), 2),
            foreground(3, 4_000, Some(4_000), 3),
        ] {
            process(
                &cfg,
                &mut state,
                &mut aggregator,
                &writer,
                &resolver,
                HookEvent::Foreground(event),
            );
        }

        let segments: Vec<_> = written
            .try_iter()
            .filter_map(|message| match message {
                WriterMsg::Segment(segment) => Some(segment),
                _ => None,
            })
            .collect();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].app_basename, "a.exe");
        assert_eq!(segments[0].end_unix, (WALL + 2_000) / 1_000);
        assert_eq!(
            state
                .current
                .as_ref()
                .map(|current| current.app.basename.as_str()),
            Some("c.exe")
        );
    }

    #[test]
    fn duplicate_appkey_neither_cuts_segment_nor_refreshes_last_input() {
        let cfg = config(false);
        let resolver = FakeResolver::new(&[(1, "same"), (2, "same")]);
        let (writer, written) = mpsc::sync_channel(64);
        let mut state = RuntimeState::default();
        let mut aggregator = Aggregator::new(cfg.afk_threshold_secs());

        process(
            &cfg,
            &mut state,
            &mut aggregator,
            &writer,
            &resolver,
            HookEvent::Foreground(foreground(1, 0, Some(0), 1)),
        );
        process(
            &cfg,
            &mut state,
            &mut aggregator,
            &writer,
            &resolver,
            HookEvent::Foreground(foreground(2, 1_000, Some(0), 2)),
        );
        assert!(written.try_recv().is_err());

        process(
            &cfg,
            &mut state,
            &mut aggregator,
            &writer,
            &resolver,
            HookEvent::IdleTick {
                at: TimePoint::new(301_000, WALL + 301_000),
                last_input: Some(MonoTime::from_millis(0)),
            },
        );
        match written.try_recv().unwrap() {
            WriterMsg::Segment(segment) => {
                assert_eq!(segment.app_basename, "same.exe");
                assert_eq!(segment.end_unix, (WALL + 300_000) / 1_000);
            }
            _ => panic!("expected a segment"),
        }
    }

    #[test]
    fn duplicate_appkey_accepts_a_real_last_input_sample_without_cutting() {
        let cfg = config(false);
        let resolver = FakeResolver::new(&[(1, "same"), (2, "same")]);
        let (writer, written) = mpsc::sync_channel(64);
        let mut state = RuntimeState::default();
        let mut aggregator = Aggregator::new(cfg.afk_threshold_secs());

        for event in [
            foreground(1, 0, Some(0), 1),
            foreground(2, 200_000, Some(200_000), 2),
        ] {
            process(
                &cfg,
                &mut state,
                &mut aggregator,
                &writer,
                &resolver,
                HookEvent::Foreground(event),
            );
        }
        assert!(written.try_recv().is_err());
        process(
            &cfg,
            &mut state,
            &mut aggregator,
            &writer,
            &resolver,
            HookEvent::IdleTick {
                at: TimePoint::new(501_000, WALL + 501_000),
                last_input: Some(MonoTime::from_millis(200_000)),
            },
        );

        match written.try_recv().unwrap() {
            WriterMsg::Segment(segment) => {
                assert_eq!(segment.app_basename, "same.exe");
                assert_eq!(segment.end_unix, (WALL + 500_000) / 1_000);
            }
            _ => panic!("expected a segment"),
        }
    }

    #[test]
    fn delayed_foreground_does_not_use_input_that_happened_after_the_event() {
        let cfg = config(false);
        let resolver = FakeResolver::new(&[(1, "a"), (2, "b")]);
        let (writer, written) = mpsc::sync_channel(64);
        let mut state = RuntimeState::default();
        let mut aggregator = Aggregator::new(cfg.afk_threshold_secs());

        process(
            &cfg,
            &mut state,
            &mut aggregator,
            &writer,
            &resolver,
            HookEvent::Foreground(foreground(1, 0, Some(0), 1)),
        );
        // The callback for the 100-second transition ran after newer input;
        // hook represents that observation as None.
        process(
            &cfg,
            &mut state,
            &mut aggregator,
            &writer,
            &resolver,
            HookEvent::Foreground(foreground(2, 100_000, None, 2)),
        );
        process(
            &cfg,
            &mut state,
            &mut aggregator,
            &writer,
            &resolver,
            HookEvent::IdleTick {
                at: TimePoint::new(310_000, WALL + 310_000),
                last_input: Some(MonoTime::from_millis(0)),
            },
        );

        let segments: Vec<_> = written
            .try_iter()
            .filter_map(|message| match message {
                WriterMsg::Segment(segment) => Some(segment),
                _ => None,
            })
            .collect();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[1].app_basename, "b.exe");
        assert_eq!(segments[1].end_unix, (WALL + 300_000) / 1_000);
    }

    #[test]
    fn title_event_uses_title_only_resolver_path() {
        let cfg = config(true);
        let resolver = FakeResolver::new(&[(1, "editor")]);
        *resolver.title.borrow_mut() = Some("new title".to_string());
        let (writer, _written) = mpsc::sync_channel(64);
        let mut state = RuntimeState::default();
        let mut aggregator = Aggregator::new(cfg.afk_threshold_secs());

        process(
            &cfg,
            &mut state,
            &mut aggregator,
            &writer,
            &resolver,
            HookEvent::Foreground(foreground(1, 0, Some(0), 7)),
        );
        process(
            &cfg,
            &mut state,
            &mut aggregator,
            &writer,
            &resolver,
            HookEvent::Title(TitleEvent {
                window: WindowId(1),
                raw_event_millis: 1_000,
                at: TimePoint::new(1_000, WALL + 1_000),
                generation: 7,
            }),
        );

        assert_eq!(resolver.foreground_calls.get(), 1);
        assert_eq!(resolver.title_calls.get(), 1);
        assert_eq!(
            state
                .current
                .as_ref()
                .and_then(|current| current.app.title.as_deref()),
            Some("new title")
        );
    }

    #[test]
    fn idle_recovery_reuses_generation_after_foreground_resolution_failure() {
        let cfg = config(true);
        let resolver = FakeResolver::new(&[]);
        let (writer, _written) = mpsc::sync_channel(64);
        let mut state = RuntimeState::default();
        let mut aggregator = Aggregator::new(cfg.afk_threshold_secs());

        process(
            &cfg,
            &mut state,
            &mut aggregator,
            &writer,
            &resolver,
            HookEvent::Foreground(foreground(1, 0, Some(0), 7)),
        );
        assert!(state.current.is_none());
        assert_eq!(
            state
                .latest_foreground
                .as_ref()
                .map(|observed| observed.generation),
            Some(7)
        );

        *resolver.current.borrow_mut() = Some((WindowId(1), 101, app("editor")));
        process(
            &cfg,
            &mut state,
            &mut aggregator,
            &writer,
            &resolver,
            HookEvent::IdleTick {
                at: TimePoint::new(1_000, WALL + 1_000),
                last_input: Some(MonoTime::from_millis(1_000)),
            },
        );
        assert_eq!(
            state.current.as_ref().map(|current| current.generation),
            Some(7)
        );

        *resolver.title.borrow_mut() = Some("recovered title".to_string());
        process(
            &cfg,
            &mut state,
            &mut aggregator,
            &writer,
            &resolver,
            HookEvent::Title(TitleEvent {
                window: WindowId(1),
                raw_event_millis: 2_000,
                at: TimePoint::new(2_000, WALL + 2_000),
                generation: 7,
            }),
        );
        assert_eq!(resolver.title_calls.get(), 1);
        assert_eq!(
            state
                .current
                .as_ref()
                .and_then(|current| current.app.title.as_deref()),
            Some("recovered title")
        );
    }

    #[test]
    fn checkpoint_input_failure_preserves_prior_input_and_afk_deadline() {
        let mut aggregator = Aggregator::new(300);
        aggregator.handle_observed_foreground(
            app("editor"),
            TimePoint::new(0, WALL),
            Some(MonoTime::from_millis(0)),
        );

        let mut segments =
            aggregator.handle_observed_checkpoint(TimePoint::new(250_000, WALL + 250_000), None);
        segments.extend(
            aggregator.handle_observed_checkpoint(TimePoint::new(310_000, WALL + 310_000), None),
        );

        assert_eq!(segments.len(), 2);
        assert_eq!(
            segments
                .iter()
                .map(|segment| segment.duration())
                .sum::<u64>(),
            300
        );
        assert_eq!(
            segments.last().map(|segment| segment.end_unix),
            Some((WALL + 300_000) / 1_000)
        );
    }
}
