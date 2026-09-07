// V1：保持 console 子系统，CLI 输出可见；daemon 模式下主动 FreeConsole 隐藏窗口。
// 后续若要彻底无闪烁，可拆出 `trackerd.exe` 走 windows 子系统。

use tracker::cli::{Cli, Cmd};
use tracker::paths::{AppPaths, InstallScope};

fn main() {
    let cli = match Cli::parse() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = dispatch(cli) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn dispatch(cli: Cli) -> std::io::Result<()> {
    let scope = InstallScope::detect();
    let paths = AppPaths::for_scope(scope)?;
    let machine_scope = scope == InstallScope::Machine;

    match cli.command {
        None => run_daemon(scope, &paths, true),
        Some(Cmd::Run) => run_daemon(scope, &paths, false),
        #[cfg(windows)]
        Some(Cmd::Stop) => {
            let signaled = tracker::daemon::runtime::signal_stop()?;
            if signaled {
                println!("Stop signal sent.");
            } else {
                println!("No running daemon found.");
            }
            Ok(())
        }
        Some(Cmd::Report(args)) => tracker::cli::report::run(args, &paths, machine_scope),
        Some(Cmd::Status) => tracker::cli::status::run(&paths, machine_scope),
        Some(Cmd::Tail(args)) => tracker::cli::tail::run(args, &paths, machine_scope),
        Some(Cmd::View(args)) => tracker::cli::view::run(args, &paths, machine_scope),
        Some(Cmd::Export(args)) => tracker::cli::export::run(args, &paths, machine_scope),
        Some(Cmd::Config(args)) => tracker::cli::config_cmd::run(args, &paths),
        #[cfg(windows)]
        Some(Cmd::Install(args)) => tracker::cli::install::install(args),
        #[cfg(windows)]
        Some(Cmd::Uninstall(args)) => tracker::cli::install::uninstall(args),
        #[cfg(windows)]
        Some(Cmd::ServiceMain) => tracker::cli::install::run_service_dispatcher(),
        #[cfg(windows)]
        Some(Cmd::Setup) => tracker::cli::setup::run(),
    }
}

#[cfg(windows)]
fn run_daemon(scope: InstallScope, paths: &AppPaths, background: bool) -> std::io::Result<()> {
    // No-argument startup is the autostart path and remains windowless.
    // Explicit `tracker run` keeps its console so registration/storage errors
    // are immediately visible to the operator.
    if background {
        tracker::platform::windows::free_console();
    }
    let result = tracker::daemon::run(scope);
    if background {
        if let Err(error) = &result {
            append_daemon_error(paths, error);
        }
    }
    result
}

#[cfg(windows)]
fn append_daemon_error(paths: &AppPaths, error: &std::io::Error) {
    use std::io::Write;

    let _ = std::fs::create_dir_all(&paths.root);
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.crash_log)
    else {
        return;
    };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let _ = writeln!(
        file,
        "[{timestamp}] pid={} daemon stopped with error: {error}",
        std::process::id()
    );
    let _ = file.sync_data();
}

#[cfg(not(windows))]
fn run_daemon(_scope: InstallScope, _paths: &AppPaths, _background: bool) -> std::io::Result<()> {
    Err(std::io::Error::other("daemon mode is Windows-only"))
}

#[cfg(all(test, windows))]
mod tests {
    use super::append_daemon_error;
    use tracker::paths::AppPaths;

    #[test]
    fn background_daemon_errors_are_persisted() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_root(directory.path());
        append_daemon_error(&paths, &std::io::Error::other("registration failed"));

        let text = std::fs::read_to_string(&paths.crash_log).unwrap();
        assert!(text.contains("daemon stopped with error: registration failed"));
    }
}
