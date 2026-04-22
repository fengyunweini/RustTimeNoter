//! 查询时分类器：基于正则匹配 exe 路径 / 窗口标题，标注分类。
//!
//! 规则文件 `rules.toml`：
//! ```toml
//! [[rule]]
//! pattern_exe   = "(?i)chrome\\.exe$"
//! pattern_title = "GitHub|Stack Overflow"
//! category      = "工作"
//!
//! [[rule]]
//! pattern_exe = "(?i)\\\\Code\\.exe$"
//! category    = "工作"
//! ```

use std::path::Path;

use regex::Regex;
use serde::Deserialize;

#[derive(Debug, Deserialize, Default)]
struct RawRules {
    #[serde(default)]
    rule: Vec<RawRule>,
}

#[derive(Debug, Deserialize)]
struct RawRule {
    #[serde(default)]
    pattern_exe: Option<String>,
    #[serde(default)]
    pattern_title: Option<String>,
    category: String,
}

pub struct Rule {
    exe: Option<Regex>,
    title: Option<Regex>,
    category: String,
}

#[derive(Default)]
pub struct Classifier {
    rules: Vec<Rule>,
}

impl Classifier {
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let s = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        let raw: RawRules = toml::from_str(&s).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
        })?;
        let mut rules = Vec::with_capacity(raw.rule.len());
        for r in raw.rule {
            let exe = r
                .pattern_exe
                .as_deref()
                .map(Regex::new)
                .transpose()
                .map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
                })?;
            let title = r
                .pattern_title
                .as_deref()
                .map(Regex::new)
                .transpose()
                .map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
                })?;
            rules.push(Rule { exe, title, category: r.category });
        }
        Ok(Self { rules })
    }

    /// 第一个匹配规则胜出；都不匹配返回 `None`。
    pub fn classify(&self, exe_path: &str, title: Option<&str>) -> Option<&str> {
        for r in &self.rules {
            let exe_ok = r.exe.as_ref().map(|re| re.is_match(exe_path)).unwrap_or(true);
            let title_ok = r
                .title
                .as_ref()
                .map(|re| title.map(|t| re.is_match(t)).unwrap_or(false))
                .unwrap_or(true);
            if exe_ok && title_ok && (r.exe.is_some() || r.title.is_some()) {
                return Some(&r.category);
            }
        }
        None
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_and_matches() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("rules.toml");
        std::fs::write(
            &p,
            r#"
[[rule]]
pattern_exe = "(?i)chrome\\.exe$"
pattern_title = "GitHub"
category = "工作"

[[rule]]
pattern_exe = "(?i)chrome\\.exe$"
category = "浏览"
"#,
        )
        .unwrap();
        let c = Classifier::load(&p).unwrap();
        assert_eq!(c.classify("C:/x/chrome.exe", Some("GitHub - foo")), Some("工作"));
        assert_eq!(c.classify("C:/x/chrome.exe", Some("YouTube")), Some("浏览"));
        assert_eq!(c.classify("C:/x/notepad.exe", None), None);
    }

    #[test]
    fn missing_file_ok() {
        let c = Classifier::load(Path::new("__no_such_rules.toml__")).unwrap();
        assert!(c.is_empty());
    }
}
