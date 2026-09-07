//! Real Win32 window-resolution microbenchmark. Creates only a message-only
//! window; never changes foreground focus or sends synthetic input/events.
//! cargo run --release --example resolver_perf -- 20000
//! Optional arguments: iterations, title limit (0..4096), title repetitions.
//! cargo run --release --example resolver_perf -- 20000 4096 40
#[cfg(windows)]
fn main() {
    use std::hint::black_box;
    use std::time::Instant;
    use tracker::daemon::hook::WindowSnapshot;
    use tracker::daemon::resolver::Resolver;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GetWindowThreadProcessId, HWND_MESSAGE,
    };
    let mut args = std::env::args().skip(1);
    let iterations: usize = args.next().map(|v| v.parse().unwrap()).unwrap_or(20_000);
    let title_max: usize = args.next().map(|v| v.parse().unwrap()).unwrap_or(256);
    let title_repetitions: usize = args.next().map(|v| v.parse().unwrap()).unwrap_or(1);
    assert!(iterations > 0 && title_max <= 4096 && (1..=1024).contains(&title_repetitions));
    let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
    let title_text =
        "A representative document title — performance replay".repeat(title_repetitions);
    let title: Vec<u16> = title_text
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
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
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
    }
    let snapshot = WindowSnapshot {
        hwnd: hwnd as isize,
        pid,
    };
    for capture_titles in [false, true] {
        let mut resolver = Resolver::default();
        let expected_title = if capture_titles && title_max > 0 {
            Some(String::from_utf16_lossy(
                &title[..(title.len() - 1).min(title_max)],
            ))
        } else {
            None
        };
        assert_eq!(
            resolver
                .resolve_window(snapshot, capture_titles, title_max)
                .unwrap()
                .title,
            expected_title
        );
        for _ in 0..1_000 {
            black_box(
                resolver
                    .resolve_window(snapshot, capture_titles, title_max)
                    .unwrap(),
            );
        }
        for repetition in 0..5 {
            let start = Instant::now();
            let mut resolved = 0;
            for _ in 0..iterations {
                let app = black_box(resolver.resolve_window(
                    black_box(snapshot),
                    capture_titles,
                    title_max,
                ))
                .unwrap();
                assert!(!app.path.is_empty());
                assert_eq!(app.title.is_some(), expected_title.is_some());
                resolved += 1;
            }
            println!(
                "{}",
                serde_json::json!({"scenario":"window_resolution", "capture_titles":capture_titles,
                "title_max":title_max, "title_utf16_units":title.len() - 1,
                "repetition":repetition, "iterations":iterations, "resolved":resolved,
                "elapsed_ns":start.elapsed().as_nanos()})
            );
        }
    }
    assert_ne!(unsafe { DestroyWindow(hwnd) }, 0);
}
#[cfg(not(windows))]
fn main() {
    eprintln!("Windows only");
}
