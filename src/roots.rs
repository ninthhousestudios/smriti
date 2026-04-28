use std::path::{Path, PathBuf};

use crate::config::{Config, expand_tilde};
use crate::error::Result;

pub struct RootEntry {
    pub path: PathBuf,
    pub enabled: bool,
}

fn roots_conf_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".smriti").join("roots.conf")
}

pub fn load_roots(config: &Config) -> Result<Vec<PathBuf>> {
    if !config.roots.is_empty() {
        return Ok(config.roots.clone());
    }
    Ok(list_all_roots()?.into_iter().filter(|e| e.enabled).map(|e| e.path).collect())
}

pub fn list_all_roots() -> Result<Vec<RootEntry>> {
    let conf = roots_conf_path();
    if !conf.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&conf)?;
    let entries = content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let (enabled, raw) = if let Some(rest) = l.strip_prefix('!') {
                (false, rest)
            } else {
                (true, l)
            };
            RootEntry { path: expand_tilde(raw), enabled }
        })
        .collect();
    Ok(entries)
}

pub fn add_root(path: &Path) -> Result<()> {
    let conf = roots_conf_path();
    if let Some(parent) = conf.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let all = list_all_roots()?;
    let canonical = path.to_string_lossy().to_string();
    if all.iter().any(|e| e.path == path) {
        return Ok(());
    }
    let mut content = if conf.exists() {
        std::fs::read_to_string(&conf)?
    } else {
        String::new()
    };
    if !content.ends_with('\n') && !content.is_empty() {
        content.push('\n');
    }
    content.push_str(&canonical);
    content.push('\n');
    std::fs::write(&conf, &content)?;
    Ok(())
}

pub fn remove_root(path: &Path) -> Result<()> {
    let conf = roots_conf_path();
    if !conf.exists() {
        return Err(crate::error::SmritiError::Config {
            var: "roots.conf".to_string(),
            message: format!("root not found: {}", path.display()),
        });
    }
    let content = std::fs::read_to_string(&conf)?;
    let path_str = path.to_string_lossy();
    let original_count = content.lines().count();
    let filtered: String = content
        .lines()
        .filter(|l| {
            let trimmed = l.trim();
            let raw = trimmed.strip_prefix('!').unwrap_or(trimmed);
            let expanded = expand_tilde(raw);
            expanded != path && raw != path_str.as_ref()
        })
        .map(|l| format!("{l}\n"))
        .collect();
    if filtered.lines().count() == original_count {
        return Err(crate::error::SmritiError::Config {
            var: "roots.conf".to_string(),
            message: format!("root not found: {}", path.display()),
        });
    }
    std::fs::write(&conf, &filtered)?;
    Ok(())
}

pub fn enable_root(path: &Path) -> Result<()> {
    set_root_enabled(path, true)
}

pub fn disable_root(path: &Path) -> Result<()> {
    set_root_enabled(path, false)
}

fn set_root_enabled(path: &Path, enable: bool) -> Result<()> {
    let conf = roots_conf_path();
    if !conf.exists() {
        return Err(crate::error::SmritiError::Config {
            var: "roots.conf".to_string(),
            message: format!("root not found: {}", path.display()),
        });
    }
    let path_str = path.to_string_lossy();
    let content = std::fs::read_to_string(&conf)?;
    let mut found = false;
    let updated: String = content
        .lines()
        .map(|l| {
            let trimmed = l.trim();
            let raw = trimmed.strip_prefix('!').unwrap_or(trimmed);
            let expanded = expand_tilde(raw);
            if expanded == path || raw == path_str.as_ref() {
                found = true;
                if enable {
                    format!("{raw}\n")
                } else {
                    format!("!{raw}\n")
                }
            } else {
                format!("{l}\n")
            }
        })
        .collect();
    if !found {
        return Err(crate::error::SmritiError::Config {
            var: "roots.conf".to_string(),
            message: format!("root not found: {}", path.display()),
        });
    }
    std::fs::write(&conf, &updated)?;
    Ok(())
}

pub fn is_under_root(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}
