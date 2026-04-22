//! `tracker config show|init|path|set|get` 子命令。

use lexopt::prelude::*;

use crate::config::Config;
use crate::paths::AppPaths;

pub struct ConfigArgs {
    pub cmd: ConfigCmd,
}

pub enum ConfigCmd {
    Show,
    Init,
    Path,
    Set { key: String, value: String },
    Get { key: String },
}

const CFG_HELP: &str = "\
tracker config — 读写配置项

USAGE:
    tracker config show
    tracker config init
    tracker config path
    tracker config set <KEY> <VALUE>
    tracker config get <KEY>

KEYS:
    afk_minutes, capture_titles, flush_interval_secs,
    flush_block_records, idle_tick_secs, title_max_chars,
    title_blacklist (逗号分隔的 exe basename)
";

pub fn parse(p: &mut lexopt::Parser) -> Result<ConfigArgs, lexopt::Error> {
    // 第一个非 flag 是 sub-sub command
    let mut sub: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    while let Some(arg) = p.next()? {
        match arg {
            Short('h') | Long("help") => { print!("{CFG_HELP}"); std::process::exit(0); }
            Value(v) => {
                if sub.is_none() {
                    sub = Some(v.to_string_lossy().into_owned());
                } else {
                    positional.push(v.to_string_lossy().into_owned());
                }
            }
            _ => return Err(arg.unexpected()),
        }
    }
    let sub = sub.ok_or(lexopt::Error::MissingValue { option: Some("config <SUBCOMMAND>".into()) })?;
    let cmd = match sub.as_str() {
        "show" => ConfigCmd::Show,
        "init" => ConfigCmd::Init,
        "path" => ConfigCmd::Path,
        "set" => {
            let key = positional.first().cloned()
                .ok_or(lexopt::Error::MissingValue { option: Some("KEY".into()) })?;
            let value = positional.get(1).cloned()
                .ok_or(lexopt::Error::MissingValue { option: Some("VALUE".into()) })?;
            ConfigCmd::Set { key, value }
        }
        "get" => {
            let key = positional.first().cloned()
                .ok_or(lexopt::Error::MissingValue { option: Some("KEY".into()) })?;
            ConfigCmd::Get { key }
        }
        other => return Err(lexopt::Error::UnexpectedArgument(format!("unknown config subcommand: {other}").into())),
    };
    Ok(ConfigArgs { cmd })
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
