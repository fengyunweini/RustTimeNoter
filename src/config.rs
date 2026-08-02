//! User-editable configuration loaded from `config.toml`.
//! 缺省值在代码中固定，文件不存在时使用 [`Config::default`]。

use std::path::Path;

use serde::{Deserialize, Serialize};

pub const MAX_TITLE_CHARS: usize = 4_096;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// 多少分钟内无键鼠输入就视为离开（AFK）。
    pub afk_minutes: u32,
    /// 是否捕获窗口标题（默认关闭，隐私优先；可在 config.toml 打开）。
    pub capture_titles: bool,
    /// 这些 exe basename（不区分大小写）即便 capture_titles=true 也不记录标题。
    pub title_blacklist: Vec<String>,
    /// 写入器空闲多少秒后强制 flush。
    pub flush_interval_secs: u32,
    /// 单个 block 缓冲多少条记录后 flush。
    pub flush_block_records: u32,
    /// AFK 探测周期（秒）。
    pub idle_tick_secs: u32,
    /// 标题最大长度（按 UTF-16 code units 截断后再转回 UTF-8）。
    pub title_max_chars: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            afk_minutes: 5,
            capture_titles: false,
            title_blacklist: Vec::new(),
            flush_interval_secs: 30,
            flush_block_records: 256,
            idle_tick_secs: 30,
            title_max_chars: 256,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> std::io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let s = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        std::fs::write(path, s)
    }

    pub fn afk_threshold_secs(&self) -> u64 {
        self.afk_minutes as u64 * 60
    }

    /// Bound callback-time allocation even if a config file contains an
    /// accidental or hostile `u32::MAX` title length.
    pub fn effective_title_max_chars(&self) -> usize {
        (self.title_max_chars as usize).min(MAX_TITLE_CHARS)
    }

    pub fn title_blacklisted(&self, exe_basename: &str) -> bool {
        self.title_blacklist
            .iter()
            .any(|b| b.eq_ignore_ascii_case(exe_basename))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_round_trip() {
        let c = Config::default();
        let s = toml::to_string(&c).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.afk_minutes, c.afk_minutes);
        assert_eq!(back.capture_titles, c.capture_titles);
    }

    #[test]
    fn missing_file_yields_default() {
        let p = std::env::temp_dir().join("__rtn_no_such_config.toml");
        let _ = std::fs::remove_file(&p);
        let c = Config::load(&p).unwrap();
        assert_eq!(c.afk_minutes, 5);
    }

    #[test]
    fn title_capture_allocation_is_bounded() {
        let config = Config {
            title_max_chars: u32::MAX,
            ..Config::default()
        };
        assert_eq!(config.effective_title_max_chars(), MAX_TITLE_CHARS);
    }
}
