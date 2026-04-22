//! 系统托盘图标。
//!
//! 用户右键菜单项：
//!   - Open report     → 后台 spawn `tracker view`
//!   - Open data folder → ShellExecute "open" 数据根目录
//!   - Status (popup)  → 一行总结日志
//!   - Stop tracking   → 触发命名 Stop 事件，daemon 优雅退出
//!
//! 实现细节：
//!   - 独立线程，独立消息泵；窗口是不可见 message-only window 不行——托盘回调必须有真正
//!     的 HWND（不能是 HWND_MESSAGE），但窗口本身不必显示。
//!   - 用 `LoadIconW(NULL, IDI_APPLICATION)` 取系统自带图标，零资源嵌入。
//!   - 第二份 daemon 进程不会到这里（被 single_instance_guard 拦截在前），所以不存在
//!     重复注册同一 GUID 的问题。
//!
//! 不抛错：托盘失败不影响 daemon 主流程，只 eprintln! 一条。

#![cfg(windows)]

use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, AtomicPtr, Ordering};
use std::thread::JoinHandle;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW, PostQuitMessage, RegisterClassExW,
    SetForegroundWindow, TrackPopupMenu, TranslateMessage, IDI_APPLICATION, MF_SEPARATOR, MF_STRING,
    MSG, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RIGHTBUTTON, WM_APP, WM_COMMAND, WM_DESTROY,
    WM_RBUTTONUP, WM_USER, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
};

const TRAY_CALLBACK_MSG: u32 = WM_USER + 1;
const TRAY_ICON_ID: u32 = 1;
const WM_TRAY_QUIT: u32 = WM_APP + 1;

const ID_OPEN_REPORT: u16 = 1001;
const ID_OPEN_DATA: u16 = 1002;
const ID_STOP: u16 = 1003;

// 主线程发的"快关掉托盘"消息要送到这里
static TRAY_HWND: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
// 托盘菜单要打开 view 时需要知道 tracker.exe 的路径
static TRACKER_EXE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
// 数据根目录（用于 "Open data folder"）
static DATA_ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
// 累计点击次数（仅诊断用）
static MENU_CLICKS: AtomicI32 = AtomicI32::new(0);

pub struct TrayHandle {
    join: Option<JoinHandle<()>>,
}

impl TrayHandle {
    /// 通知托盘线程结束。返回后托盘图标已经移除。
    pub fn shutdown(mut self) {
        let hwnd = TRAY_HWND.swap(std::ptr::null_mut(), Ordering::SeqCst) as HWND;
        if !hwnd.is_null() {
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
                    hwnd, WM_TRAY_QUIT, 0, 0,
                );
            }
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// 在后台线程启动托盘。失败时记一条 stderr，但不返回 Err（托盘失败不应让 daemon 退出）。
pub fn spawn(tracker_exe: PathBuf, data_root: PathBuf) -> Option<TrayHandle> {
    let _ = TRACKER_EXE.set(tracker_exe);
    let _ = DATA_ROOT.set(data_root);
    let join = std::thread::Builder::new()
        .name("rtn-tray".into())
        .spawn(tray_thread)
        .ok()?;
    Some(TrayHandle { join: Some(join) })
}

fn tray_thread() {
    unsafe {
        let class_name: Vec<u16> = "RustTimeNoterTray\0".encode_utf16().collect();
        let hinst = GetModuleHandleW(std::ptr::null());
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(tray_wnd_proc),
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
        let _ = RegisterClassExW(&wc);

        // 真正的（不可见的）顶层窗口；托盘回调 HWND 不能是 HWND_MESSAGE。
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            0, 0, 0, 0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            eprintln!("[rtn-tray] CreateWindowExW failed: {}",
                std::io::Error::last_os_error());
            return;
        }
        TRAY_HWND.store(hwnd as *mut _, Ordering::SeqCst);

        // 注册托盘图标
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_ICON_ID;
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = TRAY_CALLBACK_MSG;
        nid.hIcon = LoadIconW(std::ptr::null_mut(), IDI_APPLICATION);
        // tooltip "RustTimeNoter (active)"
        let tip: Vec<u16> = "RustTimeNoter\0".encode_utf16().collect();
        let copy_len = tip.len().min(nid.szTip.len());
        nid.szTip[..copy_len].copy_from_slice(&tip[..copy_len]);

        if Shell_NotifyIconW(NIM_ADD, &nid) == 0 {
            eprintln!("[rtn-tray] Shell_NotifyIconW(NIM_ADD) failed: {}",
                std::io::Error::last_os_error());
            DestroyWindow(hwnd);
            TRAY_HWND.store(std::ptr::null_mut(), Ordering::SeqCst);
            return;
        }

        // 消息泵
        let mut msg: MSG = std::mem::zeroed();
        loop {
            let r = GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
            if r == 0 || r == -1 {
                break;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // 收尾
        Shell_NotifyIconW(NIM_DELETE, &nid);
        DestroyWindow(hwnd);
        TRAY_HWND.store(std::ptr::null_mut(), Ordering::SeqCst);
    }
}

unsafe extern "system" fn tray_wnd_proc(
    hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TRAY_QUIT => {
            unsafe { PostQuitMessage(0) };
            0
        }
        TRAY_CALLBACK_MSG => {
            let event = (lparam as u32) & 0xFFFF;
            if event == WM_RBUTTONUP || event == 0x0205 /* WM_RBUTTONUP fallback */ {
                show_menu(hwnd);
            } else if event == 0x0203 /* WM_LBUTTONDBLCLK */ {
                open_report();
            }
            0
        }
        WM_COMMAND => {
            let id = (wparam as u32) & 0xFFFF;
            MENU_CLICKS.fetch_add(1, Ordering::Relaxed);
            match id as u16 {
                ID_OPEN_REPORT => open_report(),
                ID_OPEN_DATA => open_data_folder(),
                ID_STOP => {
                    // 触发命名 Stop 事件 → daemon 主线程退出 → daemon::run 收尾流程会
                    // 调用 TrayHandle::shutdown 把托盘也带走。
                    let _ = crate::daemon::runtime::signal_stop();
                }
                _ => {}
            }
            0
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let menu = CreatePopupMenu();
    if menu.is_null() { return; }

    let item_open: Vec<u16> = "Open report\0".encode_utf16().collect();
    let item_data: Vec<u16> = "Open data folder\0".encode_utf16().collect();
    let item_stop: Vec<u16> = "Stop tracking\0".encode_utf16().collect();

    AppendMenuW(menu, MF_STRING, ID_OPEN_REPORT as usize, item_open.as_ptr());
    AppendMenuW(menu, MF_STRING, ID_OPEN_DATA as usize, item_data.as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
    AppendMenuW(menu, MF_STRING, ID_STOP as usize, item_stop.as_ptr());

    let mut pt: POINT = std::mem::zeroed();
    GetCursorPos(&mut pt);
    // SetForegroundWindow 是必须的，否则点菜单外部不会自动消失（MSDN 已知坑）。
    SetForegroundWindow(hwnd);
    TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_LEFTALIGN | TPM_BOTTOMALIGN,
        pt.x, pt.y, 0, hwnd, std::ptr::null(),
    );
    DestroyMenu(menu);
}

fn open_report() {
    let Some(exe) = TRACKER_EXE.get() else { return };
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let _ = std::process::Command::new(exe)
        .arg("view")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn open_data_folder() {
    let Some(root) = DATA_ROOT.get() else { return };
    unsafe {
        let verb: Vec<u16> = "open\0".encode_utf16().collect();
        let path: Vec<u16> = root.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        windows_sys::Win32::UI::Shell::ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL as i32,
        );
    }
}
