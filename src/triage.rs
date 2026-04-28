use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::config::expand_tilde;
use crate::error::{Result, SmritiError};
use crate::ignore::{PathClassification, SectionRules};

fn escape_glob(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '*' | '?' | '[' | ']' | '{' | '}' | '\\' => {
                out.push('[');
                out.push(c);
                out.push(']');
            }
            _ => out.push(c),
        }
    }
    out
}

pub struct Recommendation {
    pub path: PathBuf,
    pub suggested_action: Action,
    pub reason: String,
    pub size_bytes: u64,
    pub file_count: Option<u64>,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Action {
    Catalog,
    Ignore,
    Keep,
}

impl Action {
    fn as_str(&self) -> &'static str {
        match self {
            Action::Catalog => "catalog",
            Action::Ignore => "ignore",
            Action::Keep => "keep",
        }
    }
}

impl std::str::FromStr for Action {
    type Err = SmritiError;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "catalog" => Ok(Action::Catalog),
            "ignore" => Ok(Action::Ignore),
            "keep" => Ok(Action::Keep),
            other => Err(SmritiError::Other(format!("unknown action: {other}"))),
        }
    }
}

pub struct DuplicateGroup {
    pub content_hash: String,
    pub size_bytes: u64,
    pub paths: Vec<PathBuf>,
}

pub struct TriageReport {
    pub recommendations: Vec<Recommendation>,
    pub duplicates: Vec<DuplicateGroup>,
    pub total_files: u64,
    pub total_bytes: u64,
}

pub struct ApplyResult {
    pub applied: usize,
    pub messages: Vec<String>,
}

const REGENERABLE_DIRS: &[&str] = &[
    "target",
    "node_modules",
    ".cache",
    "__pycache__",
    "build",
    "dist",
    ".gradle",
    ".m2",
    "venv",
    ".venv",
    "vendor",
];

const BUILD_MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "build.gradle",
    "go.mod",
    "pyproject.toml",
    "Makefile",
];

const AUDIO_EXTS: &[&str] = &["mp3", "flac", "ogg", "m4a", "aac", "wav", "opus", "wma", "aiff"];
const VIDEO_EXTS: &[&str] = &["mp4", "mkv", "avi", "mov", "wmv", "webm", "flv", "m4v", "ts"];
const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "gif", "bmp", "tiff", "webp", "heic", "raw", "cr2", "nef"];

fn canonical_score(rules: &SectionRules, path: &Path) -> i32 {
    let mut score: i32 = 100;

    match rules.classify(path, false) {
        PathClassification::Cataloged => score -= 50,
        PathClassification::Ignored => score -= 80,
        _ => {}
    }

    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        let lower = name.to_ascii_lowercase();
        for dir in path.ancestors().skip(1) {
            if let Some(d) = dir.file_name().and_then(|n| n.to_str()) {
                let dl = d.to_ascii_lowercase();
                if REGENERABLE_DIRS.contains(&dl.as_str()) {
                    score -= 40;
                    break;
                }
                if dl == "downloads" || dl == "tmp" || dl == "temp" {
                    score -= 30;
                    break;
                }
            }
        }
        if lower.contains("backup") || lower.contains("copy") || lower.contains("old") {
            score -= 20;
        }
    }

    let depth = path.components().count() as i32;
    score -= depth;

    score
}

fn media_family(ext: &str) -> Option<&'static str> {
    let lower = ext.to_ascii_lowercase();
    if AUDIO_EXTS.contains(&lower.as_str()) {
        return Some("audio");
    }
    if VIDEO_EXTS.contains(&lower.as_str()) {
        return Some("video");
    }
    if IMAGE_EXTS.contains(&lower.as_str()) {
        return Some("image");
    }
    None
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

pub fn analyze(conn: &Connection, global_rules: &SectionRules) -> Result<TriageReport> {
    let (total_files, total_bytes) = {
        let mut stmt = conn.prepare(
            "SELECT COUNT(*), COALESCE(SUM(d.byte_size), 0) \
             FROM paths p \
             JOIN documents d ON d.content_hash = p.content_hash \
             WHERE p.disappeared IS NULL",
        )?;
        stmt.query_row([], |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)))?
    };

    let mut dir_stats: HashMap<PathBuf, (u64, u64, Vec<String>)> = HashMap::new();

    {
        let mut stmt = conn.prepare(
            "SELECT p.path, d.byte_size \
             FROM paths p \
             JOIN documents d ON d.content_hash = p.content_hash \
             WHERE p.disappeared IS NULL",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let path_str: String = row.get(0)?;
            let size: u64 = row.get(1)?;
            let path = PathBuf::from(&path_str);

            if let Some(parent) = path.parent() {
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let entry = dir_stats.entry(parent.to_path_buf()).or_insert((0, 0, Vec::new()));
                entry.0 += size;
                entry.1 += 1;
                if !ext.is_empty() {
                    entry.2.push(ext);
                }
            }
        }
    }

    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let home_path = PathBuf::from(&home);
    let xdg_cache = home_path.join(".cache");
    let trash = home_path.join(".local/share/Trash");

    let mut recommendations: Vec<Recommendation> = Vec::new();
    let mut seen_dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    for (dir, (total_size, file_count, exts)) in &dir_stats {
        if seen_dirs.contains(dir) {
            continue;
        }

        let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("").to_ascii_lowercase();

        if dir.starts_with(&xdg_cache) || dir.starts_with(&trash) {
            seen_dirs.insert(dir.clone());
            recommendations.push(Recommendation {
                path: dir.clone(),
                suggested_action: Action::Catalog,
                reason: if dir.starts_with(&xdg_cache) {
                    "XDG cache directory".to_string()
                } else {
                    "trash directory".to_string()
                },
                size_bytes: *total_size,
                file_count: Some(*file_count),
            });
            continue;
        }

        if REGENERABLE_DIRS.contains(&dir_name.as_str()) {
            let manifest = dir.parent().and_then(|parent| {
                BUILD_MANIFESTS.iter().find(|m| parent.join(m).exists())
            });

            let reason = match (dir_name.as_str(), manifest) {
                ("target", Some(m)) => Some(format!("cargo build output ({m} in parent)")),
                ("node_modules", Some(m)) => Some(format!("npm dependency cache ({m} in parent)")),
                (".gradle", Some(m)) => Some(format!("gradle cache ({m} in parent)")),
                (".m2", Some(m)) => Some(format!("maven cache ({m} in parent)")),
                ("vendor", Some(m)) => Some(format!("vendored dependencies ({m} in parent)")),
                ("venv" | ".venv", Some(m)) => Some(format!("python venv ({m} in parent)")),
                ("venv" | ".venv", None) => Some("python venv".to_string()),
                ("__pycache__", _) => Some("python bytecode cache".to_string()),
                (_, Some(m)) => Some(format!("build output ({m} in parent)")),
                _ => None,
            };

            if let Some(reason) = reason {
                seen_dirs.insert(dir.clone());
                recommendations.push(Recommendation {
                    path: dir.clone(),
                    suggested_action: Action::Catalog,
                    reason,
                    size_bytes: *total_size,
                    file_count: Some(*file_count),
                });
                continue;
            }
        }

        if *total_size >= 1_073_741_824 && *file_count >= 10 {
            let total = exts.len();
            if total > 0 {
                let mut family_counts: HashMap<&'static str, usize> = HashMap::new();
                let mut ext_counts: HashMap<String, usize> = HashMap::new();
                for ext in exts {
                    ext_counts.entry(ext.clone()).and_modify(|c| *c += 1).or_insert(1);
                    if let Some(family) = media_family(ext) {
                        *family_counts.entry(family).or_insert(0) += 1;
                    }
                }
                for (family, count) in &family_counts {
                    let pct = *count as f64 / total as f64;
                    if pct > 0.90 {
                        let mut top_exts: Vec<(String, usize)> = ext_counts
                            .iter()
                            .filter(|(e, _)| media_family(e).map_or(false, |f| f == *family))
                            .map(|(e, c)| (e.clone(), *c))
                            .collect();
                        top_exts.sort_by(|a, b| b.1.cmp(&a.1));
                        let ext_list: Vec<String> = top_exts
                            .iter()
                            .take(3)
                            .map(|(e, _)| format!(".{e}"))
                            .collect();
                        let reason = format!(
                            "{} ({} files, {:.0}% {})",
                            format_bytes(*total_size),
                            file_count,
                            pct * 100.0,
                            ext_list.join("/"),
                        );
                        seen_dirs.insert(dir.clone());
                        recommendations.push(Recommendation {
                            path: dir.clone(),
                            suggested_action: Action::Keep,
                            reason,
                            size_bytes: *total_size,
                            file_count: Some(*file_count),
                        });
                        break;
                    }
                }
            }
        }
    }

    recommendations.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));

    let mut duplicates = query_duplicates(conn)?;

    for group in &mut duplicates {
        group.paths.sort_by(|a, b| {
            canonical_score(global_rules, b).cmp(&canonical_score(global_rules, a))
        });
    }

    Ok(TriageReport {
        recommendations,
        duplicates,
        total_files,
        total_bytes,
    })
}

fn query_duplicates(conn: &Connection) -> Result<Vec<DuplicateGroup>> {
    let mut stmt = conn.prepare(
        "SELECT d.content_hash, d.byte_size, GROUP_CONCAT(p.path, '|') \
         FROM documents d \
         JOIN paths p ON p.content_hash = d.content_hash \
         WHERE p.disappeared IS NULL \
         GROUP BY d.content_hash \
         HAVING COUNT(p.path) > 1 \
         ORDER BY d.byte_size DESC",
    )?;

    let mut groups = Vec::new();
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let content_hash: String = row.get(0)?;
        let size_bytes: u64 = row.get(1)?;
        let paths_concat: String = row.get(2)?;
        let paths: Vec<PathBuf> = paths_concat
            .split('|')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
        if paths.len() > 1 {
            groups.push(DuplicateGroup { content_hash, size_bytes, paths });
        }
    }
    Ok(groups)
}

pub fn format_triage_file(report: &TriageReport) -> String {
    let date = chrono::Local::now().format("%Y-%m-%d");
    let mut out = String::new();

    let _ = writeln!(out, "# smriti triage — {date}");
    let _ = writeln!(out, "# Edit the ACTION column. Save and close to apply.");
    let _ = writeln!(out, "#");
    let _ = writeln!(out, "# Actions:  catalog = tier 2 (size only)  |  ignore = stop tracking  |  keep = no change");
    let _ = writeln!(out, "#");
    let _ = writeln!(out, "# {:<10}  {:<48}  {:<10}  {}", "ACTION", "PATH", "SIZE", "REASON");

    if !report.recommendations.is_empty() {
        let _ = writeln!(out);
        for rec in &report.recommendations {
            let path_str = path_display(&rec.path);
            let size_str = format_bytes(rec.size_bytes);
            let _ = writeln!(
                out,
                "{:<10}  {:<48}  {:<10}  {}",
                rec.suggested_action.as_str(),
                path_str,
                size_str,
                rec.reason,
            );
        }
    }

    if !report.duplicates.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "# DUPLICATES — same content at multiple paths");
        let _ = writeln!(out, "# {:<10}  {:<48}  {:<10}  {}", "ACTION", "PATH", "SIZE", "DUPLICATE OF");

        let mut dir_pairs: HashMap<(PathBuf, PathBuf), Vec<&DuplicateGroup>> = HashMap::new();
        let mut individual: Vec<&DuplicateGroup> = Vec::new();

        for group in &report.duplicates {
            if group.paths.len() == 2 {
                if let (Some(a), Some(b)) = (group.paths[0].parent(), group.paths[1].parent()) {
                    let key = if a <= b {
                        (a.to_path_buf(), b.to_path_buf())
                    } else {
                        (b.to_path_buf(), a.to_path_buf())
                    };
                    dir_pairs.entry(key).or_default().push(group);
                    continue;
                }
            }
            individual.push(group);
        }

        let mut collapsed: Vec<(PathBuf, PathBuf, u64, usize)> = Vec::new();
        for ((dir_a, dir_b), groups) in &dir_pairs {
            if groups.len() >= 3 {
                let total: u64 = groups.iter().map(|g| g.size_bytes).sum();
                collapsed.push((dir_a.clone(), dir_b.clone(), total, groups.len()));
            } else {
                individual.extend(groups.iter());
            }
        }
        collapsed.sort_by(|a, b| b.2.cmp(&a.2));

        if !collapsed.is_empty() {
            let _ = writeln!(out);
            let _ = writeln!(out, "# Directory duplicates (files with same content in both dirs)");
            for (canonical_dir, dup_dir, total, count) in &collapsed {
                let _ = writeln!(
                    out,
                    "{:<10}  {:<48}  {:<10}  duplicates {} ({} files)",
                    "catalog",
                    path_display(dup_dir),
                    format_bytes(*total),
                    path_display(canonical_dir),
                    count,
                );
            }
        }

        if !individual.is_empty() {
            let _ = writeln!(out);
            for group in &individual {
                let size_str = format_bytes(group.size_bytes);
                let canonical = path_display(&group.paths[0]);
                for path in &group.paths {
                    let path_str = path_display(path);
                    let is_canonical = path == &group.paths[0];
                    let action = if is_canonical { "keep" } else { "catalog" };
                    let dup_of = if is_canonical {
                        "(canonical)".to_string()
                    } else {
                        canonical.clone()
                    };
                    let _ = writeln!(
                        out,
                        "{:<10}  {:<48}  {:<10}  {}",
                        action,
                        path_str,
                        size_str,
                        dup_of,
                    );
                }
            }
        }
    }

    out
}

fn path_display(path: &Path) -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| String::new());
    let s = path.to_string_lossy();
    if !home.is_empty() && s.starts_with(&home) {
        format!("~{}", &s[home.len()..])
    } else {
        s.into_owned()
    }
}

pub fn parse_triage_file(content: &str) -> Result<Vec<(Action, PathBuf)>> {
    let mut decisions = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut tokens = trimmed.split_whitespace();
        let action_str = match tokens.next() {
            Some(s) => s,
            None => continue,
        };
        let action: Action = match action_str.parse() {
            Ok(a) => a,
            Err(_) => continue,
        };
        let path_str = match tokens.next() {
            Some(s) => s,
            None => continue,
        };
        if path_str.is_empty() {
            continue;
        }
        let path = expand_tilde(path_str);
        decisions.push((action, path));
    }
    Ok(decisions)
}

pub fn apply_triage(decisions: &[(Action, PathBuf)]) -> Result<ApplyResult> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let ignore_path = PathBuf::from(&home).join(".smritiignore");

    let existing = if ignore_path.exists() {
        std::fs::read_to_string(&ignore_path)?
    } else {
        String::new()
    };

    let mut ignore_entries: Vec<String> = Vec::new();
    let mut catalog_entries: Vec<String> = Vec::new();

    for (action, path) in decisions {
        match action {
            Action::Keep => {}
            Action::Ignore => {
                let s = escape_glob(&path_display(path));
                ignore_entries.push(s);
            }
            Action::Catalog => {
                let s = escape_glob(&path_display(path));
                catalog_entries.push(s);
            }
        }
    }

    let applied = ignore_entries.len() + catalog_entries.len();
    if applied == 0 {
        return Ok(ApplyResult { applied: 0, messages: Vec::new() });
    }

    let mut new_content = existing.clone();
    if !new_content.is_empty() && !new_content.ends_with('\n') {
        new_content.push('\n');
    }

    let mut messages = Vec::new();

    if !ignore_entries.is_empty() {
        new_content.push('\n');
        for entry in &ignore_entries {
            new_content.push_str(entry);
            new_content.push('\n');
        }
        messages.push(format!("Added {} ignore pattern(s) to ~/.smritiignore", ignore_entries.len()));
    }

    if !catalog_entries.is_empty() {
        let mut lines: Vec<&str> = new_content.lines().collect();
        let catalog_pos = lines.iter().position(|l| l.trim() == "[catalog]");

        let insert_idx = if let Some(cat_idx) = catalog_pos {
            // Find the next section header after [catalog], insert before it.
            lines
                .iter()
                .enumerate()
                .skip(cat_idx + 1)
                .find(|(_, l)| {
                    let t = l.trim();
                    t.starts_with('[') && t.ends_with(']') && !t.starts_with('#')
                })
                .map(|(i, _)| i)
                .unwrap_or(lines.len())
        } else {
            lines.push("");
            lines.push("[catalog]");
            lines.len()
        };

        let new_lines: Vec<&str> = catalog_entries.iter().map(|s| s.as_str()).collect();
        for (i, entry) in new_lines.iter().enumerate() {
            lines.insert(insert_idx + i, entry);
        }
        new_content = lines.join("\n");
        new_content.push('\n');
        messages.push(format!("Added {} catalog pattern(s) to ~/.smritiignore", catalog_entries.len()));
    }

    std::fs::write(&ignore_path, &new_content)?;

    Ok(ApplyResult { applied, messages })
}
