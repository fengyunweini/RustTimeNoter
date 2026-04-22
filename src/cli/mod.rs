//! 命令行子命令解析与分发。

use clap::{Parser, Subcommand};

pub mod report;
pub mod export;
pub mod config_cmd;

#[cfg(windows)]
pub mod install;

#[derive(Debug, Parser)]
#[command(name = "tracker", version, about = "Ultra-light Windows app usage tracker")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// 显式启动 daemon（默认无参也是这个行为）。
    Run,
    /// 生成报表。
    Report(report::ReportArgs),
    /// 导出 CSV / JSON。
    Export(export::ExportArgs),
    /// 读写配置项。
    Config(config_cmd::ConfigArgs),
    /// 安装/卸载自启 / 服务。
    #[cfg(windows)]
    Install(install::InstallArgs),
    /// 卸载。
    #[cfg(windows)]
    Uninstall(install::UninstallArgs),
    /// 由 Windows SCM 调用，不要手工执行。
    #[cfg(windows)]
    #[command(hide = true)]
    ServiceMain,
}
