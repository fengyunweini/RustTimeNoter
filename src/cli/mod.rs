//! 命令行解析（手写，零 clap，用 `lexopt` 做底层 token 流）。

pub mod report;
pub mod export;
pub mod config_cmd;
pub mod status;
pub mod tail;
pub mod view;

#[cfg(windows)]
pub mod install;
#[cfg(windows)]
pub mod setup;

use lexopt::prelude::*;

pub struct Cli {
    pub command: Option<Cmd>,
}

pub enum Cmd {
    Run,
    #[cfg(windows)]
    Stop,
    Status,
    Tail(tail::TailArgs),
    Report(report::ReportArgs),
    View(view::ViewArgs),
    Export(export::ExportArgs),
    Config(config_cmd::ConfigArgs),
    #[cfg(windows)]
    Install(install::InstallArgs),
    #[cfg(windows)]
    Uninstall(install::UninstallArgs),
    #[cfg(windows)]
    ServiceMain,
    #[cfg(windows)]
    Setup,
}

const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "\
Ultra-light Windows app usage tracker

USAGE:
    tracker [SUBCOMMAND]

SUBCOMMANDS:
    setup                 一键安装：autostart + 启动 daemon + 打开 HTML 报表
    run                   显式启动 daemon（默认无参也是这个行为）
    stop                  优雅停止正在运行的 daemon（命名事件）
    status                显示 daemon 运行状态、当日累计、数据目录大小
    view [OPTS]           生成最近 N 天的 HTML 报表并用浏览器打开
    tail [OPTS]           实时跟随当日日志输出
    report [OPTS]         生成报表
    export [OPTS]         导出 CSV / JSON
    config <SUB> [ARGS]   读写配置项
    install <MODE>        安装：autostart 或 service
    uninstall <MODE>      卸载：autostart 或 service
    help                  打印帮助
    --version             打印版本

任何子命令加 -h / --help 看子命令的细节。
";

impl Cli {
    pub fn parse() -> Result<Self, lexopt::Error> {
        let mut p = lexopt::Parser::from_env();
        while let Some(arg) = p.next()? {
            match arg {
                Short('h') | Long("help") => {
                    print!("{HELP}");
                    std::process::exit(0);
                }
                Short('V') | Long("version") => {
                    println!("tracker {VERSION}");
                    std::process::exit(0);
                }
                Value(v) => {
                    let sub = v.to_string_lossy().into_owned();
                    let cmd = match sub.as_str() {
                        "run" => Cmd::Run,
                        #[cfg(windows)]
                        "stop" => Cmd::Stop,
                        "status" => Cmd::Status,
                        "tail" => Cmd::Tail(tail::parse(&mut p)?),
                        "report" => Cmd::Report(report::parse(&mut p)?),
                        "view" => Cmd::View(view::parse(&mut p)?),
                        "export" => Cmd::Export(export::parse(&mut p)?),
                        "config" => Cmd::Config(config_cmd::parse(&mut p)?),
                        #[cfg(windows)]
                        "install" => Cmd::Install(install::parse_install(&mut p)?),
                        #[cfg(windows)]
                        "uninstall" => Cmd::Uninstall(install::parse_uninstall(&mut p)?),
                        #[cfg(windows)]
                        "service-main" => Cmd::ServiceMain,
                        #[cfg(windows)]
                        "setup" => Cmd::Setup,
                        "help" => {
                            print!("{HELP}");
                            std::process::exit(0);
                        }
                        other => {
                            return Err(lexopt::Error::UnexpectedArgument(other.into()));
                        }
                    };
                    return Ok(Cli { command: Some(cmd) });
                }
                _ => return Err(arg.unexpected()),
            }
        }
        Ok(Cli { command: None })
    }
}
