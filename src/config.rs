//! User configuration: which items to hide, loaded from a TOML file.
//!
//! The file is re-read automatically when it changes (and on SIGHUP), so you
//! can hide/unhide icons without restarting.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Patterns matched (case-insensitively, `*` and `?` wildcards allowed)
    /// against each item's Id, Title and D-Bus name. A match hides the icon.
    pub hide: Vec<String>,
    /// Hide items that report Status "Passive" (the SNI hint for "nothing
    /// interesting right now").
    pub hide_passive: bool,
}

impl Config {
    pub fn is_hidden(&self, id: &str, title: &str, bus_name: &str, status: &str) -> bool {
        if self.hide_passive && status.eq_ignore_ascii_case("passive") {
            return true;
        }
        self.hide.iter().any(|pat| {
            [id, title, bus_name]
                .iter()
                .any(|s| !s.is_empty() && glob_match(&pat.to_lowercase(), &s.to_lowercase()))
        })
    }
}

pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    base.join("trayproxy").join("config.toml")
}

/// Tracks the config file and reloads it when its mtime changes.
pub struct ConfigFile {
    pub path: PathBuf,
    mtime: Option<SystemTime>,
}

impl ConfigFile {
    pub fn new(path: PathBuf) -> Self {
        Self { path, mtime: None }
    }

    /// Load the file. A missing file is an empty config; a broken file keeps
    /// whatever was loaded before (returns None) so a typo doesn't unhide
    /// everything.
    pub fn load(&mut self) -> Option<Config> {
        self.mtime = mtime(&self.path);
        match std::fs::read_to_string(&self.path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(c) => {
                    eprintln!("config: loaded {} ({} hide patterns)", self.path.display(), c.hide.len());
                    Some(c)
                }
                Err(e) => {
                    eprintln!("config: error in {}: {e}", self.path.display());
                    None
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(Config::default()),
            Err(e) => {
                eprintln!("config: cannot read {}: {e}", self.path.display());
                None
            }
        }
    }

    pub fn changed(&self) -> bool {
        mtime(&self.path) != self.mtime
    }
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Minimal glob: `*` matches any run of characters, `?` matches one.
pub fn glob_match(pat: &str, s: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let s: Vec<char> = s.chars().collect();
    let (mut pi, mut si) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_match("discord", "discord"));
        assert!(glob_match("chrome_status_icon_*", "chrome_status_icon_1"));
        assert!(glob_match("*steam*", "org.kde.steam"));
        assert!(glob_match("n?-applet", "nm-applet"));
        assert!(!glob_match("discord", "discord-canary"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn hiding() {
        let c: Config = toml::from_str(r#"hide = ["Discord", "nm-*"]"#).unwrap();
        assert!(c.is_hidden("discord", "", ":1.5", "Active"));
        assert!(c.is_hidden("", "NM-Applet", ":1.5", "Active"));
        assert!(!c.is_hidden("steam", "Steam", ":1.5", "Passive"));
        let c: Config = toml::from_str("hide_passive = true").unwrap();
        assert!(c.is_hidden("steam", "Steam", ":1.5", "Passive"));
    }
}
