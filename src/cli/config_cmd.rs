//! `tracker config get|set|path` 子命令。

use clap::{Args, Subcommand};

use crate::config::Config;
use crate::paths::AppPaths;

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub cmd: ConfigCmd,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCmd {
    /// 显示完整配置（即便文件不存在也展示默认值）。
    Show,
    /// 写出当前配置到磁盘（创建 config.toml 模板）。
    Init,
    /// 显示 config.toml 路径。
    Path,
    /// 修改单个字段。
    Set {
        key: String,
        value: String,
    },
    /// 读取单个字段。
    Get { key: String },
}

pub fn run(args: ConfigArgs, paths: &AppPaths) -> std::io::Result<()> {
    let mut cfg = Config::load(&paths.config_file)?;
    match args.cmd {
        ConfigCmd::Show => {
            let s = toml::to_string_pretty(&cfg).unwrap();
            print!("{s}");
        }
        ConfigCmd::Init => {
            cfg.save(&paths.config_file)?;
            println!("wrote {}", paths.config_file.display());
        }
        ConfigCmd::Path => println!("{}", paths.config_file.display()),
        ConfigCmd::Set { key, value } => {
            apply_set(&mut cfg, &key, &value)?;
            cfg.save(&paths.config_file)?;
            println!("ok: {key} = {value}");
        }
        ConfigCmd::Get { key } => {
            println!("{}", read_key(&cfg, &key)?);
        }
    }
    Ok(())
}

fn apply_set(cfg: &mut Config, key: &str, value: &str) -> std::io::Result<()> {
    fn pe(e: impl std::fmt::Display) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string())
    }
    match key {
        "afk_minutes" => cfg.afk_minutes = value.parse().map_err(pe)?,
        "capture_titles" => cfg.capture_titles = value.parse().map_err(pe)?,
        "flush_interval_secs" => cfg.flush_interval_secs = value.parse().map_err(pe)?,
        "flush_block_records" => cfg.flush_block_records = value.parse().map_err(pe)?,
        "idle_tick_secs" => cfg.idle_tick_secs = value.parse().map_err(pe)?,
        "title_max_chars" => cfg.title_max_chars = value.parse().map_err(pe)?,
        "title_blacklist" => {
            cfg.title_blacklist =
                value.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        }
        other => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("unknown config key: {other}"),
            ));
        }
    }
    Ok(())
}

fn read_key(cfg: &Config, key: &str) -> std::io::Result<String> {
    match key {
        "afk_minutes" => Ok(cfg.afk_minutes.to_string()),
        "capture_titles" => Ok(cfg.capture_titles.to_string()),
        "flush_interval_secs" => Ok(cfg.flush_interval_secs.to_string()),
        "flush_block_records" => Ok(cfg.flush_block_records.to_string()),
        "idle_tick_secs" => Ok(cfg.idle_tick_secs.to_string()),
        "title_max_chars" => Ok(cfg.title_max_chars.to_string()),
        "title_blacklist" => Ok(cfg.title_blacklist.join(",")),
        other => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unknown config key: {other}"),
        )),
    }
}
