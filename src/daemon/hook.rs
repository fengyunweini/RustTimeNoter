//! `SetWinEventHook` + 隐藏消息窗口（用于 WTS 锁屏 / 电源 / Timer 通知）。
//!
//! 全部回调通过全局 `Sender<HookEvent>` 把消息扔给 runtime 主循环消费。

#![cfg(windows)]

use std::sync::mpsc::Sender;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Power::{UnregisterPowerSettingNotification, HPOWERNOTIFY};
use windows_sys::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, KillTimer, RegisterClassExW, SetTimer,
    EVENT_OBJECT_NAMECHANGE, EVENT_SYSTEM_FOREGROUND, HWND_MESSAGE,
    PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_DESTROY, WM_POWERBROADCAST, WM_TIMER, WM_WTSSESSION_CHANGE,
    WNDCLASSEXW, WS_OVERLAPPED, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
};

#[derive(Debug, Clone)]
pub enum HookEvent {
    ForegroundChanged,
    TitleChanged, // 当前前台窗口标题改变
    IdleTick,
    SessionLock,
    SessionUnlock,
    Suspend,
    Resume,
    Shutdown,
}

static SENDER: OnceLock<Sender<HookEvent>> = OnceLock::new();

fn send(ev: HookEvent) {
    if let Some(tx) = SENDER.get() {
        let _ = tx.send(ev);
    }
}

pub fn set_sender(tx: Sender<HookEvent>) {
    let _ = SENDER.set(tx);
}

// ── WinEventProc ────────────────────────────────────────────────────────

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    _hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _dwms: u32,
) {
    if event == EVENT_SYSTEM_FOREGROUND {
        send(HookEvent::ForegroundChanged);
    } else if event == EVENT_OBJECT_NAMECHANGE && id_object == 0 {
        // OBJID_WINDOW = 0; 标题变化（仅当前线程窗口；不一定是前台）。
        // runtime 端会重新查 foreground window 决定是否更新。
        send(HookEvent::TitleChanged);
    }
}

pub struct WinHook {
    h: HWINEVENTHOOK,
    h2: HWINEVENTHOOK,
}

impl WinHook {
    pub fn install() -> Self {
        let h = unsafe {
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
        let h2 = unsafe {
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
        WinHook { h, h2 }
    }
}

impl Drop for WinHook {
    fn drop(&mut self) {
        unsafe {
            if !self.h.is_null() {
                UnhookWinEvent(self.h);
            }
            if !self.h2.is_null() {
                UnhookWinEvent(self.h2);
            }
        }
    }
}

// ── Message-only window for WM_TIMER / WM_WTSSESSION_CHANGE / WM_POWERBROADCAST ──

const TIMER_ID: usize = 1;

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER if wparam == TIMER_ID => {
            send(HookEvent::IdleTick);
            0
        }
        WM_WTSSESSION_CHANGE => {
            match wparam as u32 {
                WTS_SESSION_LOCK => send(HookEvent::SessionLock),
                WTS_SESSION_UNLOCK => send(HookEvent::SessionUnlock),
                _ => {}
            }
            0
        }
        WM_POWERBROADCAST => {
            match wparam as u32 {
                PBT_APMSUSPEND => send(HookEvent::Suspend),
                PBT_APMRESUMEAUTOMATIC => send(HookEvent::Resume),
                _ => {}
            }
            // Return TRUE for power messages.
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
        let _atom = unsafe { RegisterClassExW(&wc) };
        // 即便 atom 失败（已注册）也尝试创建。

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
            return Err(std::io::Error::last_os_error());
        }

        let timer_set = unsafe {
            SetTimer(hwnd, TIMER_ID, idle_tick_secs.saturating_mul(1000).max(1000), None) != 0
        };

        let wts_registered = unsafe {
            WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) != 0
        };

        // GUID_MONITOR_POWER_ON / SESSION 等可选；这里只靠 WM_POWERBROADCAST 的 PBT_APMSUSPEND
        // 不需要 RegisterPowerSettingNotification。
        let power_handle: HPOWERNOTIFY = 0;

        Ok(Self { hwnd, timer_set, wts_registered, power_handle })
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
                UnregisterPowerSettingNotification(self.power_handle);
            }
            if !self.hwnd.is_null() {
                DestroyWindow(self.hwnd);
            }
        }
    }
}

// 占位避免 unused 警告（暂未启用 RegisterPowerSettingNotification 路径）
#[allow(dead_code)]
const _: () = ();
