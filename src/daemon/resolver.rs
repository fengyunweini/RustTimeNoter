//! 把当前前台窗口解析为 [`AppKey`]（处理 UWP `ApplicationFrameHost` 等边界情况）。

#![cfg(windows)]

use crate::daemon::aggregator::AppKey;
use crate::platform::windows as plat;

const UWP_HOST: &str = "ApplicationFrameHost.exe";

pub fn resolve_foreground(capture_title: bool, title_max: usize) -> Option<AppKey> {
    let hwnd = plat::foreground_window()?;
    let pid = plat::window_pid(hwnd)?;
    let mut path = plat::process_image_path(pid);
    let mut basename = path.as_ref().map(|p| plat::basename(p)).unwrap_or_default();

    // UWP: 找真实 child 进程
    if basename.eq_ignore_ascii_case(UWP_HOST) {
        if let Some(child_pid) = plat::first_child_pid_distinct(hwnd, pid) {
            if let Some(child_path) = plat::process_image_path(child_pid) {
                basename = plat::basename(&child_path);
                path = Some(child_path);
            }
        }
    }

    let path_str = path
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        // 退化：拿不到完整路径时只用 basename
        .unwrap_or_else(|| basename.clone());

    if path_str.is_empty() {
        return None;
    }

    let title = if capture_title {
        let t = plat::window_title(hwnd, title_max);
        if t.is_empty() { None } else { Some(t) }
    } else {
        None
    };

    Some(AppKey { path: path_str, basename, title })
}
