//! Daemon 主循环：把 Win32 消息转 [`Event`]，喂给 [`Aggregator`]，再丢给 writer 线程。

#![cfg(windows)]

use std::sync::mpsc::{self};
use std::thread;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PostQuitMessage, TranslateMessage, MSG,
};

use super::aggregator::{Aggregator, Event};
use super::hook::{self, HookEvent, MessageWindow, WinHook};
use super::resolver;
use crate::config::Config;
use crate::paths::{AppPaths, InstallScope};
use crate::storage::writer::{self, WriterConfig, WriterMsg};

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
        .stack_size(64 * 1024)
        .spawn(move || {
            if let Err(e) = writer::run(writer_cfg, wrx) {
                eprintln!("[rtn] writer error: {e}");
            }
        })?;

    // 启动 hook 事件 channel + Win32 消息处理线程
    let (htx, hrx) = mpsc::channel::<HookEvent>();
    hook::set_sender(htx);

    // 把"业务循环"放在工作线程：消息线程只跑 GetMessage。
    // 但 SetWinEventHook OUTOFCONTEXT 回调依赖该线程的消息循环；
    // 所以让"主线程"跑 GetMessage（这里是 daemon 入口线程），
    // 业务循环放在另一个线程里 `recv()` 处理 HookEvent。
    let aggregator_handle = {
        let cfg2 = cfg.clone();
        let wtx2 = wtx.clone();
        thread::Builder::new()
            .name("rtn-aggr".into())
            .stack_size(128 * 1024)
            .spawn(move || aggregator_loop(cfg2, hrx, wtx2))?
    };

    // 主线程：装 hook + 创建消息窗口 + 跑消息循环
    let _hook = WinHook::install();
    let _msg_win = MessageWindow::create(cfg.idle_tick_secs)?;

    // 启动初始：触发一次 ForegroundChanged 让 aggregator 拿到当前窗口
    if let Some(tx) = std::sync::OnceLock::new().get_or_init(|| ()).into() {
        let _ = tx;
    }
    // 简化：直接通过 hook channel 不可用（已 move），改成 PostMessage 不需要——
    // 让 aggregator 在收到第一次 IdleTick 时主动 resolve。

    // GetMessage 循环
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

    // 收尾
    drop(_msg_win);
    drop(_hook);
    let _ = wtx.send(WriterMsg::Shutdown);
    let _ = aggregator_handle.join();
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

    while let Ok(ev) = hrx.recv() {
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

#[allow(dead_code)]
fn quit() {
    unsafe { PostQuitMessage(0); }
}

// 让 HWND 类型出现，避免 unused import
#[allow(dead_code)]
fn _typing(_h: HWND) {}
