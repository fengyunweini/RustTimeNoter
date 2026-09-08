//! windows-sys 薄封装：UTF-16 转换、HWND→进程信息、AttachConsole 等。

#![cfg(windows)]

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_INSUFFICIENT_BUFFER, HANDLE, HWND, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Console::{AttachConsole, FreeConsole, ATTACH_PARENT_PROCESS};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId, IsChild,
};

/// 转 OsStr → 以 NUL 结尾的 UTF-16。
pub fn to_wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 把 wide 缓冲（不含 trailing NUL）转 String。
pub fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

pub fn foreground_window() -> Option<HWND> {
    let h = unsafe { GetForegroundWindow() };
    if h.is_null() {
        None
    } else {
        Some(h)
    }
}

pub(crate) fn window_pid(hwnd: HWND) -> Option<u32> {
    let mut pid: u32 = 0;
    let tid = unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if tid == 0 || pid == 0 {
        None
    } else {
        Some(pid)
    }
}

pub(crate) fn window_title(hwnd: HWND, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    // Read the current title once. A preliminary length query adds a Win32
    // call and can truncate a title that grows between the two reads.
    let cap = max_chars.saturating_add(1).min(i32::MAX as usize);
    let mut short = [0u16; 257];
    let mut long;
    let buf = if cap <= short.len() {
        &mut short[..cap]
    } else {
        long = vec![0u16; cap];
        &mut long[..]
    };
    let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), cap as i32) };
    if n <= 0 {
        return String::new();
    }
    from_wide(&buf[..n as usize])
}

/// 取进程完整路径；权限不足时返回 None。
pub fn process_image_path(pid: u32) -> Option<PathBuf> {
    ProcessImageHandle::open_query(pid).map(|(_handle, path)| path)
}

/// Keeps one process identity alive while its immutable image path is cached.
/// A recycled PID can never reuse this handle's identity.
pub(crate) struct ProcessImageHandle {
    raw: HANDLE,
    can_wait: bool,
}

impl ProcessImageHandle {
    pub(crate) fn open(pid: u32) -> Option<(Self, PathBuf)> {
        Self::open_with_access(pid, true).or_else(|| Self::open_query(pid))
    }

    /// A query-only handle still pins the process identity for the duration of
    /// one observation. It is never reused as a live-process cache entry.
    pub(crate) fn open_query(pid: u32) -> Option<(Self, PathBuf)> {
        Self::open_with_access(pid, false)
    }

    fn open_with_access(pid: u32, can_wait: bool) -> Option<(Self, PathBuf)> {
        let access =
            PROCESS_QUERY_LIMITED_INFORMATION | if can_wait { PROCESS_SYNCHRONIZE } else { 0 };
        let raw = unsafe { OpenProcess(access, 0, pid) };
        if raw.is_null() {
            return None;
        }
        let handle = Self { raw, can_wait };
        let path = unsafe { read_image_path(raw) }?;
        Some((handle, path))
    }

    pub(crate) fn is_running(&self) -> bool {
        self.can_wait && unsafe { WaitForSingleObject(self.raw, 0) == WAIT_TIMEOUT }
    }
}

impl Drop for ProcessImageHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.raw);
        }
    }
}

unsafe fn read_image_path(h: windows_sys::Win32::Foundation::HANDLE) -> Option<PathBuf> {
    // Most executable paths fit on the stack. Query the executable directly,
    // without inspecting modules or requesting process-memory read access.
    let mut buf = [0u16; 1024];
    let mut n = buf.len() as u32;
    if QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut n) != 0 {
        return Some(PathBuf::from(OsString::from_wide(&buf[..n as usize])));
    }
    if GetLastError() != ERROR_INSUFFICIENT_BUFFER {
        return None;
    }
    // The uncommon long-path case remains supported, without truncation.
    let mut long = vec![0u16; 32_768];
    n = long.len() as u32;
    if QueryFullProcessImageNameW(h, 0, long.as_mut_ptr(), &mut n) == 0 {
        return None;
    }
    Some(PathBuf::from(OsString::from_wide(&long[..n as usize])))
}

/// Milliseconds since boot, including time spent suspended.
pub fn monotonic_millis() -> u64 {
    unsafe { GetTickCount64() }
}

/// Extend a past WinEvent timestamp into the current 64-bit boot epoch.
pub fn extend_tick_count_32(tick: u32, now: u64) -> u64 {
    let age = (now as u32).wrapping_sub(tick) as u64;
    now.saturating_sub(age)
}

fn extend_last_input_tick_count_32(tick: u32, now: u64) -> Option<u64> {
    let age = (now as u32).wrapping_sub(tick);
    // LASTINPUTINFO may contain a timestamp ahead of the sampled clock. Do not
    // mistake that for input almost 49.7 days ago. Very old ambiguous samples
    // likewise cannot safely establish a new activity interval.
    if age > i32::MAX as u32 {
        return None;
    }
    now.checked_sub(age as u64)
}

/// Last real input only when it does not postdate the caller's observation.
/// A delayed callback must not apply newer input retroactively to its event.
pub fn last_input_monotonic_millis(causal_now: u64) -> Option<u64> {
    unsafe {
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if GetLastInputInfo(&mut info) == 0 {
            return None;
        }
        // Read the clock after LASTINPUTINFO; input arriving between the two
        // reads cannot wrap a one-tick future value into the previous epoch.
        let observed_now = GetTickCount64();
        extend_last_input_tick_count_32(info.dwTime, observed_now)
            .filter(|last_input| *last_input <= causal_now)
    }
}

/// System idle duration for display callers. Accounting uses the causal sample.
pub fn seconds_since_last_input() -> u64 {
    let now = monotonic_millis();
    last_input_monotonic_millis(now)
        .map(|last| now.saturating_sub(last) / 1_000)
        .unwrap_or(0)
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ChildWindow {
    pub(crate) hwnd: HWND,
    pub(crate) pid: u32,
}

impl ChildWindow {
    pub(crate) fn is_current_child_of(self, host: HWND) -> bool {
        window_pid(self.hwnd) == Some(self.pid) && unsafe { IsChild(host, self.hwnd) != 0 }
    }
}

/// Keep the child window as well as its PID so callers can reject destruction,
/// reparenting, or owner changes while resolving an ApplicationFrameHost child.
pub(crate) fn first_child_window_distinct(host_hwnd: HWND, host_pid: u32) -> Option<ChildWindow> {
    struct Ctx {
        host_pid: u32,
        result: Option<ChildWindow>,
    }
    let mut ctx = Ctx {
        host_pid,
        result: None,
    };

    unsafe extern "system" fn cb(
        hwnd: HWND,
        lparam: windows_sys::Win32::Foundation::LPARAM,
    ) -> i32 {
        let ctx = &mut *(lparam as *mut Ctx);
        if let Some(pid) = window_pid(hwnd) {
            if pid != ctx.host_pid {
                ctx.result = Some(ChildWindow { hwnd, pid });
                return 0; // 停止枚举
            }
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

#[cfg(test)]
mod tests {
    use super::{
        extend_last_input_tick_count_32, extend_tick_count_32, from_wide, ProcessImageHandle,
    };

    #[test]
    fn query_only_handle_resolves_but_cannot_be_reused_as_live_cache() {
        let pid = std::process::id();
        let (live, live_path) = ProcessImageHandle::open(pid).unwrap();
        assert!(live.is_running());
        let (query_only, query_path) = ProcessImageHandle::open_query(pid).unwrap();
        assert_eq!(query_path, live_path);
        assert!(!query_only.is_running());
        assert!(ProcessImageHandle::open_query(0).is_none());
    }

    #[test]
    fn wide_text_preserves_unicode_and_replaces_only_invalid_surrogates() {
        assert_eq!(from_wide(&[0x4e2d, 0xd83e, 0xdd80, 0, 0x61]), "中🦀");
        assert_eq!(from_wide(&[0xd83e, 0x61, 0xdc00]), "�a�");
    }

    #[test]
    fn extends_recent_event_across_u32_rollover() {
        let now = u32::MAX as u64 + 17;
        assert_eq!(extend_tick_count_32(u32::MAX - 15, now), now - 32);
        assert_eq!(
            extend_last_input_tick_count_32(u32::MAX - 15, now),
            Some(now - 32)
        );
    }

    #[test]
    fn future_input_is_not_mistaken_for_a_previous_epoch() {
        assert_eq!(extend_last_input_tick_count_32(123_457, 123_456), None);
        assert_eq!(
            extend_last_input_tick_count_32(123_400, 123_456),
            Some(123_400)
        );
    }
}
