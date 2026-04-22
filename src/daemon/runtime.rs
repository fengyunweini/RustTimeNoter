//! Daemon 主循环：把 Win32 消息转 [`Event`]，喂给 [`Aggregator`]，再丢给 writer 线程。

#![cfg(windows)]

use std::sync::mpsc::{self};
use std::thread;

use windows_sys::Win32::Foundation::{BOOL, HANDLE, HWND};
use windows_sys::Win32::System::Console::{SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT};
use windows_sys::Win32::System::Threading::{CreateEventW, GetCurrentThreadId, WaitForSingleObject, INFINITE};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PostQuitMessage, PostThreadMessageW, TranslateMessage, MSG, WM_QUIT,
};

use super::aggregator::{Aggregator, Event};
use super::hook::{self, HookEvent, MessageWindow, WinHook};
use super::resolver;
use crate::config::Config;
use crate::paths::{AppPaths, InstallScope};
use crate::storage::writer::{self, WriterConfig, WriterMsg};
use crate::STOP_EVENT_NAME;

pub fn run(scope: InstallScope) -> std::io::Result<()> {
    // 单实例锁
    let _mutex = single_instance_guard()?;

    let paths = AppPaths::for_scope(scope)?;
    paths.ensure_dirs()?;
    let cfg = Config::load(&paths.config_file).unwrap_or_default();

    // 启动 writer 线程
    let (wtx, wrx) = mpsc::channel::<WriterMsg>();
    let writer_cfg = WriterConfig {
        paths: paths.clone(),
        scope,
        flush_block_records: cfg.flush_block_records.max(1),
        flush_interval_secs: cfg.flush_interval_secs.max(1),
    };
    let writer_handle = thread::Builder::new()
        .name("rtn-writer".into())
        .spawn(move || {
            if let Err(e) = writer::run(writer_cfg, wrx) {
                eprintln!("[rtn] writer error: {e}");
            }
        })?;

    // 启动 hook 事件 channel + Win32 消息处理线程
    let (htx, hrx) = mpsc::channel::<HookEvent>();
    hook::set_sender(htx);

    let aggregator_handle = {
        let cfg2 = cfg.clone();
        let wtx2 = wtx.clone();
        thread::Builder::new()
            .name("rtn-aggr".into())
            .spawn(move || aggregator_loop(cfg2, hrx, wtx2))?
    };

    // 装 hook + 创建消息窗口
    let _hook = WinHook::install();
    let _msg_win = MessageWindow::create(cfg.idle_tick_secs)?;

    // 注册 Ctrl handler（控制台模式下 Ctrl+C / close 触发）
    unsafe {
        MAIN_THREAD_ID.store(GetCurrentThreadId(), std::sync::atomic::Ordering::SeqCst);
        SetConsoleCtrlHandler(Some(ctrl_handler), 1);
    }
    let main_tid = unsafe { GetCurrentThreadId() };

    // 创建命名 Stop 事件并起一个 waiter 线程：被 signal 后 PostThreadMessage(WM_QUIT) 给主线程
    let stop_event = create_stop_event()?;
    let _waiter = thread::Builder::new()
        .name("rtn-stop-wait".into())
        .spawn(move || {
            let ev = stop_event;
            unsafe {
                WaitForSingleObject(ev.0, INFINITE);
                PostThreadMessageW(main_tid, WM_QUIT, 0, 0);
            }
            drop(ev);
        })?;

    // GetMessage 主循环
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        loop {
            let r = GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
            if r == 0 || r == -1 {
                break;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // 收尾：先关 hook & 消息窗口（不再产生事件），再发 Shutdown 给 aggregator → writer flush → join。
    drop(_msg_win);
    drop(_hook);
    hook::send_shutdown();
    let _ = aggregator_handle.join();
    let _ = wtx.send(WriterMsg::Shutdown);
    let _ = writer_handle.join();
    Ok(())
}

fn aggregator_loop(
    cfg: Config,
    hrx: mpsc::Receiver<HookEvent>,
    wtx: mpsc::Sender<WriterMsg>,
) {
    let mut agg = Aggregator::new(cfg.afk_threshold_secs());

    // 立即解析一次当前前台窗口
    if let Some(app) = resolver::resolve_foreground(cfg.capture_titles, cfg.title_max_chars as usize) {
        let key = strip_blacklisted_title(&cfg, app);
        let segs = agg.handle(Event::Foreground { app: key, t: now() });
        for s in segs { let _ = wtx.send(WriterMsg::Segment(s)); }
    }

    loop {
        let ev = match hrx.recv() {
            Ok(e) => e,
            Err(_) => HookEvent::Shutdown, // sender dropped (主循环退出) → 优雅收尾
        };
        let now_t = now();
        let outs = match ev {
            HookEvent::ForegroundChanged | HookEvent::TitleChanged => {
                if let Some(app) = resolver::resolve_foreground(cfg.capture_titles, cfg.title_max_chars as usize) {
                    let app = strip_blacklisted_title(&cfg, app);
                    agg.handle(Event::Foreground { app, t: now_t })
                } else {
                    Vec::new()
                }
            }
            HookEvent::IdleTick => {
                let idle_secs = crate::platform::windows::seconds_since_last_input();
                let last_input = now_t.saturating_sub(idle_secs);
                agg.handle(Event::IdleTick { now: now_t, last_input })
            }
            HookEvent::SessionLock => agg.handle(Event::SessionLock { t: now_t }),
            HookEvent::SessionUnlock => agg.handle(Event::SessionUnlock { t: now_t }),
            HookEvent::Suspend => agg.handle(Event::Suspend { t: now_t }),
            HookEvent::Resume => agg.handle(Event::Resume { t: now_t }),
            HookEvent::Shutdown => {
                let segs = agg.handle(Event::Shutdown { t: now_t });
                for s in segs { let _ = wtx.send(WriterMsg::Segment(s)); }
                let _ = wtx.send(WriterMsg::Flush);
                break;
            }
        };
        for s in outs {
            let _ = wtx.send(WriterMsg::Segment(s));
        }
    }
}

// ── Stop event (named) ─────────────────────────────────────────────────

struct StopEvent(HANDLE);
unsafe impl Send for StopEvent {}
impl Drop for StopEvent {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0); }
    }
}

fn create_stop_event() -> std::io::Result<StopEvent> {
    let name: Vec<u16> = STOP_EVENT_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    let h = unsafe {
        // manual reset, initially non-signaled
        CreateEventW(std::ptr::null(), 1, 0, name.as_ptr())
    };
    if h.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    Ok(StopEvent(h))
}

/// 由外部 CLI (`tracker stop`) 调用：打开命名事件并 set，触发 daemon PostQuitMessage。
pub fn signal_stop() -> std::io::Result<bool> {
    use windows_sys::Win32::System::Threading::{OpenEventW, SetEvent, EVENT_MODIFY_STATE};
    let name: Vec<u16> = STOP_EVENT_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let h = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
        if h.is_null() {
            return Ok(false); // daemon 未运行
        }
        let ok = SetEvent(h);
        windows_sys::Win32::Foundation::CloseHandle(h);
        Ok(ok != 0)
    }
}

// ── Console Ctrl handler ───────────────────────────────────────────────

static MAIN_THREAD_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

unsafe extern "system" fn ctrl_handler(ctrl_type: u32) -> BOOL {
    match ctrl_type {
        CTRL_C_EVENT
        | CTRL_BREAK_EVENT
        | CTRL_CLOSE_EVENT
        | CTRL_LOGOFF_EVENT
        | CTRL_SHUTDOWN_EVENT => {
            let tid = MAIN_THREAD_ID.load(std::sync::atomic::Ordering::SeqCst);
            if tid != 0 {
                unsafe { PostThreadMessageW(tid, WM_QUIT, 0, 0); }
            } else {
                unsafe { PostQuitMessage(0); }
            }
            1 // handled
        }
        _ => 0,
    }
}

fn strip_blacklisted_title(cfg: &Config, mut app: super::aggregator::AppKey) -> super::aggregator::AppKey {
    if cfg.title_blacklisted(&app.basename) {
        app.title = None;
    }
    app
}

fn now() -> u64 {
    crate::storage::writer::now_unix()
}

// ── single instance ─────────────────────────────────────────────────────

struct MutexGuard {
    h: windows_sys::Win32::Foundation::HANDLE,
}
impl Drop for MutexGuard {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.h); }
    }
}

fn single_instance_guard() -> std::io::Result<MutexGuard> {
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let name: Vec<u16> = crate::MUTEX_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    let h = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
    if h.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let err = unsafe { GetLastError() };
    if err == ERROR_ALREADY_EXISTS {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(h); }
        return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "another tracker daemon is running"));
    }
    Ok(MutexGuard { h })
}

// 让 HWND 类型出现，避免 unused import
#[allow(dead_code)]
fn _typing(_h: HWND) {}
