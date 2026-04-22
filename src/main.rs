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
        None => run_daemon(scope),
        Some(Cmd::Run) => run_daemon(scope),
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
fn run_daemon(scope: InstallScope) -> std::io::Result<()> {
    // 隐藏从 explorer/Run 启动时的命令窗口
    tracker::platform::windows::free_console();
    tracker::daemon::run(scope)
}

#[cfg(not(windows))]
fn run_daemon(_scope: InstallScope) -> std::io::Result<()> {
    Err(std::io::Error::other("daemon mode is Windows-only"))
}
