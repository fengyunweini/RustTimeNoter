//! 把当前前台窗口解析为 [`AppKey`]（处理 UWP `ApplicationFrameHost` 等边界情况）。

#![cfg(windows)]

use crate::daemon::aggregator::AppKey;
use crate::daemon::hook::WindowSnapshot;
use crate::platform::windows as plat;

const UWP_HOST: &str = "ApplicationFrameHost.exe";

/// One live process path is retained for repeated samples/title events. Window
/// ownership and title are still read for every observation; UWP children are
/// resolved afresh because the host can change its child process.
#[derive(Default)]
pub struct Resolver {
    cached: Option<CachedImage>,
}

struct CachedImage {
    pid: u32,
    handle: plat::ProcessImageHandle,
    path: String,
    basename: String,
}

struct ChildImage {
    window: plat::ChildWindow,
    // Keep this identity alive until the final HWND/PID/parent check, including
    // the title read. No child process is cached across observations.
    _handle: plat::ProcessImageHandle,
    path: String,
    basename: String,
}

impl ChildImage {
    fn open(host: windows_sys::Win32::Foundation::HWND, window: plat::ChildWindow) -> Option<Self> {
        if !window.is_current_child_of(host) {
            return None;
        }
        let (handle, path) = plat::ProcessImageHandle::open_query(window.pid)?;
        if !window.is_current_child_of(host) {
            return None;
        }
        let basename = plat::basename(&path);
        Some(Self {
            window,
            _handle: handle,
            path: path.to_string_lossy().into_owned(),
            basename,
        })
    }
}

fn observation_is_current(window: WindowSnapshot, child: Option<&ChildImage>) -> bool {
    let hwnd = window.hwnd as windows_sys::Win32::Foundation::HWND;
    plat::window_pid(hwnd) == Some(window.pid)
        && child.is_none_or(|child| child.window.is_current_child_of(hwnd))
}

impl Resolver {
    fn image(&mut self, pid: u32) -> Option<&CachedImage> {
        let reusable = self
            .cached
            .as_ref()
            .is_some_and(|cached| cached.pid == pid && cached.handle.is_running());
        if !reusable {
            self.cached = None;
            let (handle, path) = plat::ProcessImageHandle::open(pid)?;
            let basename = plat::basename(&path);
            self.cached = Some(CachedImage {
                pid,
                handle,
                path: path.to_string_lossy().into_owned(),
                basename,
            });
        }
        // Query-only permission fallback is retained through this observation
        // too; is_running() rejects it for reuse on the next observation.
        self.cached.as_ref()
    }

    pub fn resolve_window(
        &mut self,
        window: WindowSnapshot,
        capture_title: bool,
        title_max: usize,
    ) -> Option<AppKey> {
        let hwnd = window.hwnd as windows_sys::Win32::Foundation::HWND;
        let pid = window.pid;
        if pid == 0 || plat::window_pid(hwnd) != Some(pid) {
            return None;
        }
        let host = self.image(pid)?;
        let child = if host.basename.eq_ignore_ascii_case(UWP_HOST) {
            match plat::first_child_window_distinct(hwnd, pid) {
                Some(window) => Some(ChildImage::open(hwnd, window)?),
                None => None,
            }
        } else {
            None
        };
        let path = child
            .as_ref()
            .map(|child| &child.path)
            .unwrap_or(&host.path);
        if path.is_empty() {
            return None;
        }
        let title = if capture_title {
            let title = plat::window_title(hwnd, title_max);
            (!title.is_empty()).then_some(title)
        } else {
            None
        };
        if !observation_is_current(window, child.as_ref()) {
            return None;
        }
        let (path, basename) = match child {
            Some(child) => (child.path, child.basename),
            None => (host.path.clone(), host.basename.clone()),
        };
        Some(AppKey {
            path,
            basename,
            title,
        })
    }
}

pub fn resolve_foreground(capture_title: bool, title_max: usize) -> Option<AppKey> {
    let hwnd = plat::foreground_window()?;
    let pid = plat::window_pid(hwnd)?;
    resolve_window(
        WindowSnapshot {
            hwnd: hwnd as isize,
            pid,
        },
        capture_title,
        title_max,
    )
}

/// Resolve only a window that still belongs to the observed process. A stale
/// HWND/PID pair becomes an explicit gap, never a lookup of an unrelated
/// process that has reused a historical PID.
pub fn resolve_window(
    window: WindowSnapshot,
    capture_title: bool,
    title_max: usize,
) -> Option<AppKey> {
    Resolver::default().resolve_window(window, capture_title, title_max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, SetParent, SetWindowTextW, HWND_MESSAGE, WS_CHILD,
    };

    struct TestWindow(windows_sys::Win32::Foundation::HWND);

    impl TestWindow {
        fn new(parent: windows_sys::Win32::Foundation::HWND, title: &str) -> Self {
            let class_name = plat::to_wide("STATIC");
            let title = plat::to_wide(title);
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    class_name.as_ptr(),
                    title.as_ptr(),
                    if parent == HWND_MESSAGE { 0 } else { WS_CHILD },
                    0,
                    0,
                    0,
                    0,
                    parent,
                    std::ptr::null_mut(),
                    GetModuleHandleW(std::ptr::null()),
                    std::ptr::null(),
                )
            };
            assert!(!hwnd.is_null());
            Self(hwnd)
        }

        fn snapshot(&self) -> WindowSnapshot {
            WindowSnapshot {
                hwnd: self.0 as isize,
                pid: plat::window_pid(self.0).unwrap(),
            }
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            if !self.0.is_null() {
                assert_ne!(unsafe { DestroyWindow(self.0) }, 0);
            }
        }
    }

    #[test]
    fn destroyed_uwp_child_invalidates_observation_even_when_its_process_and_host_live() {
        let host = TestWindow::new(HWND_MESSAGE, "host");
        let mut child = TestWindow::new(host.0, "child");
        // The ownership checks use real Win32 HWNDs. A distinct host PID is
        // only the selection filter here; no system UWP app is manipulated.
        let selected = plat::first_child_window_distinct(host.0, u32::MAX).unwrap();
        assert_eq!(selected.hwnd, child.0);
        let image = ChildImage::open(host.0, selected).unwrap();
        assert!(observation_is_current(host.snapshot(), Some(&image)));
        assert_ne!(unsafe { DestroyWindow(child.0) }, 0);
        child.0 = std::ptr::null_mut();
        assert!(plat::process_image_path(selected.pid).is_some());
        assert!(!observation_is_current(host.snapshot(), Some(&image)));
        assert!(ChildImage::open(host.0, selected).is_none());
    }

    #[test]
    fn reparented_or_wrong_owner_uwp_child_is_rejected() {
        let host = TestWindow::new(HWND_MESSAGE, "host");
        let other_host = TestWindow::new(HWND_MESSAGE, "other host");
        let child = TestWindow::new(host.0, "child");
        let selected = plat::first_child_window_distinct(host.0, u32::MAX).unwrap();
        let image = ChildImage::open(host.0, selected).unwrap();
        assert!(ChildImage::open(
            host.0,
            plat::ChildWindow {
                pid: selected.pid.wrapping_add(1),
                ..selected
            },
        )
        .is_none());
        assert_eq!(unsafe { SetParent(child.0, other_host.0) }, host.0);
        assert_eq!(plat::window_pid(child.0), Some(selected.pid));
        assert!(!observation_is_current(host.snapshot(), Some(&image)));
        assert!(ChildImage::open(host.0, selected).is_none());
        assert!(ChildImage::open(other_host.0, selected).is_some());
    }

    #[test]
    fn cached_process_still_reads_changed_and_long_unicode_titles() {
        let window = TestWindow::new(HWND_MESSAGE, "initial");
        let mut resolver = Resolver::default();
        let first = resolver
            .resolve_window(window.snapshot(), true, 256)
            .unwrap();
        assert_eq!(first.title.as_deref(), Some("initial"));
        let title = format!("{}🦀 final", "中文 document ".repeat(50));
        assert_ne!(
            unsafe { SetWindowTextW(window.0, plat::to_wide(&title).as_ptr()) },
            0
        );
        for limit in [0, 1, 2, 255, 256, 257, 4_096] {
            let units: Vec<_> = title.encode_utf16().take(limit).collect();
            let expected = String::from_utf16_lossy(&units);
            let app = resolver
                .resolve_window(window.snapshot(), true, limit)
                .unwrap();
            assert_eq!(app.path, first.path);
            assert_eq!(app.title, (!expected.is_empty()).then_some(expected));
        }
        assert!(resolver
            .resolve_window(window.snapshot(), false, 256)
            .unwrap()
            .title
            .is_none());
    }

    #[test]
    fn destroyed_window_does_not_resolve_its_still_live_process() {
        // A private message-only STATIC window needs no message pump and never
        // changes the user's foreground window or starts the real daemon.
        let class_name: Vec<u16> = "STATIC\0".encode_utf16().collect();
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                std::ptr::null(),
            )
        };
        assert!(!hwnd.is_null());
        let snapshot = WindowSnapshot {
            hwnd: hwnd as isize,
            pid: plat::window_pid(hwnd).unwrap(),
        };
        let mut resolver = Resolver::default();
        assert!(resolver.resolve_window(snapshot, false, 0).is_some());
        let wrong_owner = WindowSnapshot {
            pid: snapshot.pid.wrapping_add(1),
            ..snapshot
        };
        assert!(resolver.resolve_window(wrong_owner, false, 0).is_none());
        assert_ne!(unsafe { DestroyWindow(hwnd) }, 0);
        // Its owning test process still exists and could be opened by PID.
        // The destroyed HWND must nevertheless reject historical attribution.
        assert!(plat::process_image_path(snapshot.pid).is_some());
        assert!(resolve_window(snapshot, false, 0).is_none());
        assert!(resolver.resolve_window(snapshot, false, 0).is_none());
    }
}
