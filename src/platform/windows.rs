//! windows-sys 薄封装：UTF-16 转换、HWND→进程信息、AttachConsole 等。

#![cfg(windows)]

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

use windows_sys::Win32::Foundation::{CloseHandle, HWND};
use windows_sys::Win32::System::Console::{
    AttachConsole, FreeConsole, ATTACH_PARENT_PROCESS,
};
use windows_sys::Win32::System::ProcessStatus::GetModuleFileNameExW;
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId,
};

/// 转 OsStr → 以 NUL 结尾的 UTF-16。
pub fn to_wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// 把 wide 缓冲（不含 trailing NUL）转 String。
pub fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    OsString::from_wide(&buf[..len]).to_string_lossy().into_owned()
}

pub fn foreground_window() -> Option<HWND> {
    let h = unsafe { GetForegroundWindow() };
    if h.is_null() { None } else { Some(h) }
}

pub fn window_pid(hwnd: HWND) -> Option<u32> {
    let mut pid: u32 = 0;
    let tid = unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if tid == 0 || pid == 0 { None } else { Some(pid) }
}

pub fn window_title(hwnd: HWND, max_chars: usize) -> String {
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return String::new();
    }
    let cap = (len as usize + 1).min(max_chars + 1);
    let mut buf = vec![0u16; cap];
    let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), cap as i32) };
    if n <= 0 {
        return String::new();
    }
    from_wide(&buf[..n as usize])
}

/// 取进程完整路径；权限不足时返回 None。
pub fn process_image_path(pid: u32) -> Option<PathBuf> {
    unsafe {
        let h = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
            0,
            pid,
        );
        if h.is_null() {
            // 试一下不带 VM_READ
            let h2 = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h2.is_null() {
                return None;
            }
            let p = read_image_path(h2);
            CloseHandle(h2);
            return p;
        }
        let p = read_image_path(h);
        CloseHandle(h);
        p
    }
}

unsafe fn read_image_path(h: windows_sys::Win32::Foundation::HANDLE) -> Option<PathBuf> {
    let mut buf = vec![0u16; 1024];
    let n = GetModuleFileNameExW(h, std::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32);
    if n == 0 {
        return None;
    }
    Some(PathBuf::from(OsString::from_wide(&buf[..n as usize])))
}

/// 系统启动以来上一次输入与现在的差（秒）。失败返回 0。
pub fn seconds_since_last_input() -> u64 {
    unsafe {
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if GetLastInputInfo(&mut info) == 0 {
            return 0;
        }
        let now = windows_sys::Win32::System::SystemInformation::GetTickCount();
        let diff_ms = now.wrapping_sub(info.dwTime);
        (diff_ms as u64) / 1000
    }
}

/// EnumChildWindows 拿首个 child 窗口的 PID（用于 UWP ApplicationFrameHost）。
pub fn first_child_pid_distinct(host_hwnd: HWND, host_pid: u32) -> Option<u32> {
    struct Ctx {
        host_pid: u32,
        result: Option<u32>,
    }
    let mut ctx = Ctx { host_pid, result: None };

    unsafe extern "system" fn cb(hwnd: HWND, lparam: windows_sys::Win32::Foundation::LPARAM) -> i32 {
        let ctx = &mut *(lparam as *mut Ctx);
        let mut pid: u32 = 0;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if pid != 0 && pid != ctx.host_pid {
            ctx.result = Some(pid);
            return 0; // 停止枚举
        }
        1
    }

    unsafe {
        EnumChildWindows(host_hwnd, Some(cb), &mut ctx as *mut _ as isize);
    }
    ctx.result
}

/// 进程 basename（小写不变，按存储原样）。
pub fn basename(p: &std::path::Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// ── CLI 控制台 ────────────────────────────────────────────────────────

/// 把当前进程附着到父终端，使 println! 可见。仅 CLI 命令调用。
pub fn attach_parent_console() {
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

pub fn free_console() {
    unsafe {
        let _ = FreeConsole();
    }
}
