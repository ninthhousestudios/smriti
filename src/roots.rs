use std::path::{Path, PathBuf};

use crate::config::{Config, expand_tilde};
use crate::error::Result;

fn roots_conf_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".smriti").join("roots.conf")
}

pub fn load_roots(config: &Config) -> Result<Vec<PathBuf>> {
    if !config.roots.is_empty() {
        return Ok(config.roots.clone());
    }
    list_roots()
}

pub fn list_roots() -> Result<Vec<PathBuf>> {
    let conf = roots_conf_path();
    if !conf.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&conf)?;
    let roots: Vec<PathBuf> = content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(expand_tilde)
        .collect();
    Ok(roots)
}

pub fn add_root(path: &Path) -> Result<()> {
    let conf = roots_conf_path();
    if let Some(parent) = conf.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = list_roots()?;
    let canonical = path.to_string_lossy().to_string();
    if existing.iter().any(|r| r == path) {
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
            let expanded = expand_tilde(trimmed);
            expanded != path && trimmed != path_str.as_ref()
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

pub fn is_under_root(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}
