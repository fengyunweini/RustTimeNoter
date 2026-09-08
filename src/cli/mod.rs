//! 命令行解析（手写，零 clap，用 `lexopt` 做底层 token 流）。

pub mod config_cmd;
pub mod export;
pub mod report;
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
        Self::parse_from(lexopt::Parser::from_env(), |help| {
            print!("{help}");
            std::process::exit(0);
        })
    }

    fn parse_from(
        mut p: lexopt::Parser,
        mut show_help: impl FnMut(&str) -> Result<Self, lexopt::Error>,
    ) -> Result<Self, lexopt::Error> {
        if let Some(arg) = p.next()? {
            match arg {
                Short('h') | Long("help") => {
                    parse_no_arguments(&mut p)?;
                    return show_help(HELP);
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
                            parse_no_arguments(&mut p)?;
                            return show_help(HELP);
                        }
                        other => {
                            return Err(lexopt::Error::UnexpectedArgument(other.into()));
                        }
                    };
                    // Dedicated parsers already consume their options. For
                    // bare commands, validate every remaining token before
                    // either showing help or allowing command dispatch.
                    if parse_no_arguments(&mut p)? {
                        return show_help(HELP);
                    }
                    return Ok(Cli { command: Some(cmd) });
                }
                _ => return Err(arg.unexpected()),
            }
        }
        Ok(Cli { command: None })
    }
}

fn parse_no_arguments(p: &mut lexopt::Parser) -> Result<bool, lexopt::Error> {
    let mut help = false;
    while let Some(arg) = p.next()? {
        match arg {
            Short('h') | Long("help") => help = true,
            _ => return Err(arg.unexpected()),
        }
    }
    // Reading through the end also rejects an attached value such as
    // --help=bad; an early help exit would bypass lexopt's validation.
    Ok(help)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_ARGUMENT_COMMANDS: &[&str] = &[
        "run",
        "status",
        #[cfg(windows)]
        "stop",
        #[cfg(windows)]
        "setup",
        #[cfg(windows)]
        "service-main",
    ];

    #[test]
    fn no_argument_commands_accept_only_the_command_itself() {
        for command in NO_ARGUMENT_COMMANDS {
            let parsed = Cli::parse_from(lexopt::Parser::from_args([*command]), |_| {
                panic!("unexpected help for {command}")
            })
            .unwrap();
            assert!(parsed.command.is_some(), "{command}");
        }
    }

    #[test]
    fn no_argument_help_never_returns_a_dispatchable_command() {
        for command in NO_ARGUMENT_COMMANDS {
            for flag in ["-h", "--help"] {
                let mut help_shown = false;
                let parsed = Cli::parse_from(lexopt::Parser::from_args([*command, flag]), |text| {
                    assert!(text.contains("tracker"));
                    help_shown = true;
                    // A harmless sentinel replaces the real help callback's
                    // process exit; this test never dispatches a command.
                    Ok(Cli { command: None })
                })
                .unwrap();
                assert!(help_shown, "{command} {flag} did not show help");
                assert!(parsed.command.is_none(), "{command} {flag} can dispatch");
            }
        }
    }

    #[test]
    fn no_argument_commands_reject_unknown_flags_and_extra_positionals() {
        for command in NO_ARGUMENT_COMMANDS {
            for extra in [
                &["--not-an-option"][..],
                &["unexpected"][..],
                &["--help=bad"][..],
                &["--help", "unexpected"][..],
                &["-hx"][..],
                &["--", "--help"][..],
            ] {
                let parsed = Cli::parse_from(
                    lexopt::Parser::from_args(
                        std::iter::once(*command).chain(extra.iter().copied()),
                    ),
                    |_| Ok(Cli { command: None }),
                );
                assert!(parsed.is_err(), "accepted {command} {extra:?}");
            }
        }
    }

    #[test]
    fn malformed_top_level_help_does_not_bypass_argument_validation() {
        let parsed = Cli::parse_from(lexopt::Parser::from_args(["--help=bad"]), |_| {
            Ok(Cli { command: None })
        });
        assert!(parsed.is_err());
    }
}
