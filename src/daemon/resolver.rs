//! Resolve the process identity and optional title for a specific window.

#![cfg(windows)]

use crate::daemon::aggregator::AppKey;
use crate::daemon::event_queue::WindowId;
use crate::platform::windows as platform;

const UWP_HOST: &str = "ApplicationFrameHost.exe";

/// Resolve exactly the HWND/PID pair captured by the foreground callback.
///
/// Process identity trusts the callback-time PID, so a short-lived/destroyed
/// window does not erase an intermediate transition. HWND ownership is
/// re-checked only before reading title text or enumerating UWP children.
pub fn resolve_window(
    window: WindowId,
    pid_at_event: u32,
    capture_title: bool,
    title_max: usize,
) -> Option<AppKey> {
    if pid_at_event == 0 {
        return None;
    }
    resolve_window_with_pid(window, pid_at_event, capture_title, title_max)
}

/// Resolve the current foreground for startup and post-AFK recovery.
pub fn resolve_foreground(
    capture_title: bool,
    title_max: usize,
) -> Option<(WindowId, u32, AppKey)> {
    let hwnd = platform::foreground_window()?;
    let pid = platform::window_pid(hwnd)?;
    let window = WindowId::from_hwnd(hwnd);
    let app = resolve_window_with_pid(window, pid, capture_title, title_max)?;
    Some((window, pid, app))
}

fn resolve_window_with_pid(
    window: WindowId,
    pid: u32,
    capture_title: bool,
    title_max: usize,
) -> Option<AppKey> {
    let hwnd = window.as_hwnd();
    let mut path = platform::process_image_path(pid);
    let mut basename = path
        .as_ref()
        .map(|path| platform::basename(path))
        .unwrap_or_default();

    let window_still_owned = platform::window_pid(hwnd) == Some(pid);
    if basename.eq_ignore_ascii_case(UWP_HOST) && window_still_owned {
        if let Some(child_pid) = platform::first_child_pid_distinct(hwnd, pid) {
            if let Some(child_path) = platform::process_image_path(child_pid) {
                basename = platform::basename(&child_path);
                path = Some(child_path);
            }
        }
    }

    let path = path
        .map(|path| path.to_string_lossy().into_owned())
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| basename.clone());
    if path.is_empty() {
        return None;
    }

    let title = capture_title
        .then(|| resolve_title(window, pid, title_max))
        .flatten();
    Some(AppKey {
        path,
        basename,
        title,
    })
}

/// Resolve only title text. This path intentionally performs no PID lookup,
/// `OpenProcess`, executable-path resolution, or UWP child enumeration.
pub fn resolve_title(window: WindowId, expected_pid: u32, title_max: usize) -> Option<String> {
    if expected_pid == 0 || platform::window_pid(window.as_hwnd()) != Some(expected_pid) {
        return None;
    }
    let title = platform::window_title(window.as_hwnd(), title_max);
    (!title.is_empty()).then_some(title)
}
