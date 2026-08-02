//! WinEvent hooks plus the hidden window used for timer, WTS, and power events.

#![cfg(windows)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, OnceLock};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

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
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetForegroundWindow,
    GetMessageW, KillTimer, PeekMessageW, PostQuitMessage, PostThreadMessageW, RegisterClassExW,
    SetTimer, TranslateMessage, DEVICE_NOTIFY_WINDOW_HANDLE, EVENT_OBJECT_NAMECHANGE,
    EVENT_SYSTEM_FOREGROUND, HWND_MESSAGE, MSG, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMECRITICAL,
    PBT_APMRESUMESUSPEND, PBT_APMSUSPEND, PM_NOREMOVE, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_DESTROY, WM_POWERBROADCAST, WM_QUIT, WM_TIMER,
    WM_WTSSESSION_CHANGE, WNDCLASSEXW, WS_OVERLAPPED, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
};

use super::aggregator::{MonoTime, TimePoint};
use super::event_queue::{EventSender, ForegroundEvent, HookEvent, TimelineSendError, WindowId};
use crate::platform::windows as platform;

static SENDER: OnceLock<EventSender> = OnceLock::new();
static WIN_EVENT_STOPPING: AtomicBool = AtomicBool::new(false);
static CONTROL_QUEUE_OVERFLOWED: AtomicBool = AtomicBool::new(false);

pub fn set_sender(sender: EventSender) -> std::io::Result<()> {
    SENDER.set(sender).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "hook event sender is already registered",
        )
    })
}

fn send_timeline(event: HookEvent) {
    if let Some(sender) = SENDER.get() {
        if sender.send_timeline(event) == Err(TimelineSendError::Full) {
            CONTROL_QUEUE_OVERFLOWED.store(true, Ordering::Release);
            // Never wait in WndProc. End the main loop so normal teardown can
            // enqueue Shutdown through its dedicated reserve and surface the
            // overflow through crash.log in background mode.
            unsafe {
                PostQuitMessage(1);
            }
        }
    }
}

pub fn control_queue_overflowed() -> bool {
    CONTROL_QUEUE_OVERFLOWED.load(Ordering::Acquire)
}

/// Explicitly stop the aggregator after hooks and the message window are gone.
pub fn send_shutdown() {
    let (at, last_input) = sample_activity();
    send_timeline(HookEvent::Shutdown { at, last_input });
}

/// Seed the queue after installing hooks. This also establishes the foreground
/// generation against which subsequent title callbacks are checked.
fn send_current_foreground() {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return;
    }
    let now = platform::monotonic_millis();
    send_foreground(hwnd, now as u32);
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _id_event_thread: u32,
    dwms_event_time: u32,
) {
    if WIN_EVENT_STOPPING.load(Ordering::Acquire) {
        return;
    }

    if event == EVENT_SYSTEM_FOREGROUND && !hwnd.is_null() {
        send_foreground(hwnd, dwms_event_time);
    } else if event == EVENT_OBJECT_NAMECHANGE
        && id_object == 0
        && id_child == 0
        && !hwnd.is_null()
        && unsafe { GetForegroundWindow() } == hwnd
    {
        // The title lane performs a second generation/window check under its
        // mutex, covering a foreground switch between this check and enqueue.
        if let Some(sender) = SENDER.get() {
            let _ = sender.send_title(
                WindowId::from_hwnd(hwnd),
                dwms_event_time,
                event_time_point(dwms_event_time),
            );
        }
    }
}

fn send_foreground(hwnd: HWND, raw_event_millis: u32) {
    let Some(sender) = SENDER.get() else {
        return;
    };
    let at = event_time_point(raw_event_millis);
    let last_input = causal_last_input(
        platform::last_input_monotonic_millis(platform::monotonic_millis()),
        at.monotonic,
    );
    let _ = sender.send_foreground(ForegroundEvent {
        window: WindowId::from_hwnd(hwnd),
        pid_at_event: platform::window_pid(hwnd).unwrap_or(0),
        raw_event_millis,
        at,
        last_input,
        generation: 0,
    });
}

fn event_time_point(raw_event_millis: u32) -> TimePoint {
    let observed = sample_time_point();
    let event_monotonic = MonoTime::from_millis(platform::extend_tick_count_32(
        raw_event_millis,
        observed.monotonic.0,
    ));
    project_time_point(observed, event_monotonic)
}

fn sample_time_point() -> TimePoint {
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

fn sample_activity() -> (TimePoint, Option<MonoTime>) {
    let at = sample_time_point();
    let last_input = causal_last_input(
        platform::last_input_monotonic_millis(at.monotonic.0),
        at.monotonic,
    );
    (at, last_input)
}

fn causal_last_input(observed_millis: Option<u64>, event_at: MonoTime) -> Option<MonoTime> {
    observed_millis
        .map(MonoTime::from_millis)
        .filter(|last_input| *last_input <= event_at)
}

fn project_time_point(observed: TimePoint, monotonic: MonoTime) -> TimePoint {
    let wall_unix_millis = if monotonic <= observed.monotonic {
        observed
            .wall_unix_millis
            .saturating_sub(observed.monotonic.elapsed_millis_since(monotonic))
    } else {
        observed
            .wall_unix_millis
            .saturating_add(monotonic.elapsed_millis_since(observed.monotonic))
    };
    TimePoint {
        monotonic,
        wall_unix_millis,
    }
}

struct WinHook {
    foreground: HWINEVENTHOOK,
    title: HWINEVENTHOOK,
}

impl WinHook {
    fn install(capture_titles: bool) -> std::io::Result<Self> {
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
            return Err(last_registration_error("SetWinEventHook(foreground)"));
        }

        let title = if should_install_title_hook(capture_titles) {
            unsafe {
                SetWinEventHook(
                    EVENT_OBJECT_NAMECHANGE,
                    EVENT_OBJECT_NAMECHANGE,
                    std::ptr::null_mut(),
                    Some(win_event_proc),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                )
            }
        } else {
            std::ptr::null_mut()
        };
        if capture_titles && title.is_null() {
            let error = last_registration_error("SetWinEventHook(title)");
            unsafe {
                UnhookWinEvent(foreground);
            }
            return Err(error);
        }

        Ok(Self { foreground, title })
    }
}

fn should_install_title_hook(capture_titles: bool) -> bool {
    capture_titles
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

/// Owns the out-of-context WinEvent hooks and the message pump that delivers
/// their callbacks. Keeping this pump off the main thread means foreground
/// queue backpressure cannot prevent the control window from receiving
/// suspend, session, or quit messages.
pub struct WinEventThread {
    thread_id: u32,
    join: Option<thread::JoinHandle<std::io::Result<()>>>,
}

impl WinEventThread {
    pub fn start(capture_titles: bool, main_thread_id: u32) -> std::io::Result<Self> {
        WIN_EVENT_STOPPING.store(false, Ordering::Release);
        let (ready_tx, ready_rx) = mpsc::sync_channel::<std::io::Result<u32>>(1);
        let join = thread::Builder::new()
            .name("rtn-winevent".into())
            .spawn(move || {
                initialize_thread_message_queue();
                let thread_id = unsafe { GetCurrentThreadId() };
                let hooks = match WinHook::install(capture_titles) {
                    Ok(hooks) => hooks,
                    Err(error) => {
                        let reported = std::io::Error::new(error.kind(), error.to_string());
                        let _ = ready_tx.send(Err(reported));
                        return Err(error);
                    }
                };

                let _wake_main = WakeMainOnExit(main_thread_id);
                if ready_tx.send(Ok(thread_id)).is_err() {
                    return Ok(());
                }

                // Install first, then snapshot. Any later focus switch is
                // delivered by this same thread's WinEvent message pump.
                send_current_foreground();
                let result = run_win_event_message_loop();
                drop(hooks);
                result
            })?;

        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok(Self {
                thread_id,
                join: Some(join),
            }),
            Ok(Err(error)) => {
                let _ = join_win_event_thread(join);
                Err(error)
            }
            Err(_) => match join_win_event_thread(join) {
                Err(error) => Err(error),
                Ok(()) => Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "WinEvent thread exited before reporting startup",
                )),
            },
        }
    }

    /// Stop the message pump and wait for hook teardown on its owning thread.
    ///
    /// Runtime must call this while the aggregator receiver is still alive so
    /// a callback waiting on bounded-queue space can finish.
    pub fn shutdown(mut self) -> std::io::Result<()> {
        let Some(join) = self.join.take() else {
            return Ok(());
        };

        // Once a callback already waiting on queue space returns, suppress
        // later WinEvents so the pump can reach its queued WM_QUIT promptly.
        WIN_EVENT_STOPPING.store(true, Ordering::Release);
        let post_error = if join.is_finished() {
            Some(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "WinEvent thread exited before shutdown",
            ))
        } else if unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) } == 0 {
            Some(last_registration_error(
                "PostThreadMessageW(WinEvent WM_QUIT)",
            ))
        } else {
            None
        };

        if let Some(post_error) = post_error {
            if join.is_finished() {
                return join_win_event_thread(join).and(Err(post_error));
            }
            // A live thread that cannot be signalled must not make cleanup
            // block forever. Detach it and propagate the explicit failure.
            drop(join);
            return Err(post_error);
        }

        join_win_event_thread(join)
    }
}

impl Drop for WinEventThread {
    fn drop(&mut self) {
        if self.join.is_some() {
            WIN_EVENT_STOPPING.store(true, Ordering::Release);
            unsafe {
                PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
            }
        }
    }
}

fn initialize_thread_message_queue() {
    unsafe {
        let mut message: MSG = std::mem::zeroed();
        PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
    }
}

fn run_win_event_message_loop() -> std::io::Result<()> {
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

fn join_win_event_thread(join: thread::JoinHandle<std::io::Result<()>>) -> std::io::Result<()> {
    join.join()
        .map_err(|_| std::io::Error::other("WinEvent thread panicked"))?
}

struct WakeMainOnExit(u32);

impl Drop for WakeMainOnExit {
    fn drop(&mut self) {
        unsafe {
            PostThreadMessageW(self.0, WM_QUIT, 0, 0);
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
        WM_TIMER if wparam == TIMER_ID => {
            let (at, last_input) = sample_activity();
            send_timeline(HookEvent::IdleTick { at, last_input });
            0
        }
        WM_WTSSESSION_CHANGE => {
            match wparam as u32 {
                WTS_SESSION_LOCK => {
                    let (at, last_input) = sample_activity();
                    send_timeline(HookEvent::SessionLock { at, last_input });
                }
                WTS_SESSION_UNLOCK => {
                    let at = sample_time_point();
                    send_timeline(HookEvent::SessionUnlock { at });
                }
                _ => {}
            }
            0
        }
        WM_POWERBROADCAST => {
            match wparam as u32 {
                PBT_APMSUSPEND => {
                    let (at, last_input) = sample_activity();
                    send_timeline(HookEvent::Suspend { at, last_input });
                }
                PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMECRITICAL | PBT_APMRESUMESUSPEND => {
                    let at = sample_time_point();
                    send_timeline(HookEvent::Resume { at })
                }
                _ => {}
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
            return Err(last_registration_error("GetModuleHandleW"));
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
            return Err(last_registration_error("RegisterClassExW"));
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
                HWND_MESSAGE,
                std::ptr::null_mut(),
                hinst,
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err(last_registration_error("CreateWindowExW"));
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
            return Err(last_registration_error("SetTimer"));
        }
        window.timer_set = true;

        if unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) } == 0 {
            return Err(last_registration_error("WTSRegisterSessionNotification"));
        }
        window.wts_registered = true;

        // Message-only windows need directed power notifications.
        let power_handle =
            unsafe { RegisterSuspendResumeNotification(hwnd, DEVICE_NOTIFY_WINDOW_HANDLE) };
        if power_handle == 0 {
            return Err(last_registration_error("RegisterSuspendResumeNotification"));
        }
        window.power_handle = power_handle;

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

fn last_registration_error(operation: &str) -> std::io::Error {
    let source = std::io::Error::last_os_error();
    std::io::Error::new(source.kind(), format!("{operation} failed: {source}"))
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::{
        causal_last_input, initialize_thread_message_queue, project_time_point,
        run_win_event_message_loop, should_install_title_hook, WakeMainOnExit,
    };
    use crate::daemon::aggregator::{MonoTime, TimePoint};

    #[test]
    fn title_hook_is_registered_only_when_capture_is_enabled() {
        assert!(!should_install_title_hook(false));
        assert!(should_install_title_hook(true));
    }

    #[test]
    fn event_wall_time_is_projected_from_its_monotonic_tick() {
        let observed = TimePoint::new(10_000, 50_000);
        assert_eq!(
            project_time_point(observed, MonoTime::from_millis(9_750)),
            TimePoint::new(9_750, 49_750)
        );
    }

    #[test]
    fn callback_input_after_event_is_not_attributed_to_event() {
        assert_eq!(
            causal_last_input(Some(10_001), MonoTime::from_millis(10_000)),
            None
        );
        assert_eq!(
            causal_last_input(Some(9_999), MonoTime::from_millis(10_000)),
            Some(MonoTime::from_millis(9_999))
        );
        assert_eq!(causal_last_input(None, MonoTime::from_millis(10_000)), None);
    }

    #[test]
    fn dedicated_message_pump_stops_on_quit_and_wakes_main() {
        initialize_thread_message_queue();
        let main_thread_id = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
        drain_thread_quit();

        let (ready, started) = mpsc::channel();
        let worker = thread::spawn(move || {
            initialize_thread_message_queue();
            let thread_id = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
            let _wake_main = WakeMainOnExit(main_thread_id);
            ready.send(thread_id).unwrap();
            run_win_event_message_loop()
        });

        let worker_thread_id = started.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_ne!(
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                    worker_thread_id,
                    windows_sys::Win32::UI::WindowsAndMessaging::WM_QUIT,
                    0,
                    0,
                )
            },
            0
        );
        assert!(worker.join().unwrap().is_ok());
        assert!(drain_thread_quit());
    }

    fn drain_thread_quit() -> bool {
        let mut received = false;
        unsafe {
            let mut message: windows_sys::Win32::UI::WindowsAndMessaging::MSG = std::mem::zeroed();
            while windows_sys::Win32::UI::WindowsAndMessaging::PeekMessageW(
                &mut message,
                std::ptr::null_mut(),
                windows_sys::Win32::UI::WindowsAndMessaging::WM_QUIT,
                windows_sys::Win32::UI::WindowsAndMessaging::WM_QUIT,
                windows_sys::Win32::UI::WindowsAndMessaging::PM_REMOVE,
            ) != 0
            {
                received = true;
            }
        }
        received
    }
}
