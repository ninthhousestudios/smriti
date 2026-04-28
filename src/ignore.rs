//! .smritiignore parser with gitignore semantics.
//!
//! A .smritiignore file has three sections:
//! - Default section (before any header): patterns for files to ignore entirely.
//! - `[catalog]` section: patterns for paths to track as catalog (tier 2) only.
//! - `[no-embed]` section: patterns for files to index but not embed.
//!
//! Each section uses full gitignore semantics via the `ignore` crate's
//! `Gitignore` / `GitignoreBuilder`.

use std::fs;
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::Match;

use crate::error::{Result, SmritiError};

/// Hardened defaults embedded in the binary.
const DEFAULTS: &str = include_str!("ignore_defaults.txt");

/// Classification result for a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathClassification {
    /// Skip entirely — not tracked at all.
    Ignored,
    /// Tier-2 catalog: track existence + size only, no content indexing.
    Cataloged,
    /// Tier-1 indexed: hashed, metadata extracted, FTS indexed.
    Indexed,
    /// Tier-1 indexed but embedding suppressed.
    IndexedNoEmbed,
}

/// Compiled rules for one .smritiignore file (or the hardened defaults).
pub struct SectionRules {
    pub ignored: Gitignore,
    pub cataloged: Gitignore,
    pub no_embed: Gitignore,
}

impl SectionRules {
    /// Returns `true` if all three matchers are empty (match nothing).
    pub fn is_empty(&self) -> bool {
        self.ignored.is_empty() && self.cataloged.is_empty() && self.no_embed.is_empty()
    }

    /// Build a `SectionRules` with no patterns (matches nothing).
    pub fn empty() -> Self {
        let g = |d: &Path| GitignoreBuilder::new(d).build().unwrap();
        let root = Path::new("/");
        Self { ignored: g(root), cataloged: g(root), no_embed: g(root) }
    }

    pub fn classify(&self, path: &Path, is_dir: bool) -> PathClassification {
        classify_against(self, path, is_dir).unwrap_or(PathClassification::Indexed)
    }
}

/// Parse a .smritiignore file's content into compiled `SectionRules`.
///
/// Lines before the first section header go into the ignored section.
/// `[catalog]` and `[no-embed]` headers switch the active section.
/// Lines beginning with `#` or blank lines are ignored (gitignore semantics
/// handle this inside `add_line`).
pub fn parse_smritiignore(content: &str, base_dir: &Path) -> Result<SectionRules> {
    let mut ignored_builder = GitignoreBuilder::new(base_dir);
    let mut cataloged_builder = GitignoreBuilder::new(base_dir);
    let mut no_embed_builder = GitignoreBuilder::new(base_dir);

    enum Section {
        Ignored,
        Cataloged,
        NoEmbed,
    }
    let mut current = Section::Ignored;

    for line in content.lines() {
        let trimmed = line.trim();
        match trimmed {
            "[catalog]" => {
                current = Section::Cataloged;
                continue;
            }
            "[no-embed]" => {
                current = Section::NoEmbed;
                continue;
            }
            _ => {}
        }

        let builder = match current {
            Section::Ignored => &mut ignored_builder,
            Section::Cataloged => &mut cataloged_builder,
            Section::NoEmbed => &mut no_embed_builder,
        };

        builder
            .add_line(None, trimmed)
            .map_err(|e| SmritiError::Other(format!("ignore pattern error: {e}")))?;
    }

    let ignored = ignored_builder
        .build()
        .map_err(|e| SmritiError::Other(format!("failed to build ignore matcher: {e}")))?;
    let cataloged = cataloged_builder
        .build()
        .map_err(|e| SmritiError::Other(format!("failed to build catalog matcher: {e}")))?;
    let no_embed = no_embed_builder
        .build()
        .map_err(|e| SmritiError::Other(format!("failed to build no-embed matcher: {e}")))?;

    Ok(SectionRules { ignored, cataloged, no_embed })
}

/// Compile the hardened defaults embedded in the binary.
///
/// `base_dir` is used as the root for pattern anchoring. For a global
/// matcher this should be `/` (or the user's home dir); for tests use a
/// temp dir.
pub fn hardened_defaults(base_dir: &Path) -> SectionRules {
    // Errors in the embedded defaults are programming errors — panic fast.
    parse_smritiignore(DEFAULTS, base_dir)
        .expect("hardened defaults must always parse successfully")
}

/// Incremental stack of SectionRules layers, one per directory level that
/// contains a `.smritiignore` file, plus a global base layer.
///
/// The scanner pushes a layer when it enters a directory that has a
/// `.smritiignore`, and pops it when it leaves.  Classification checks layers
/// from most-specific (top of stack) to the global base; the first match wins.
pub struct IgnoreStack {
    global: SectionRules,
    layers: Vec<(PathBuf, SectionRules)>,
}

impl IgnoreStack {
    pub fn new(global: SectionRules) -> Self {
        Self { global, layers: Vec::new() }
    }

    /// Check whether `dir` contains a `.smritiignore` file. If so, parse it
    /// and push the resulting rules onto the stack.
    ///
    /// Returns `Ok(true)` if a layer was pushed, `Ok(false)` if not.
    pub fn push_dir(&mut self, dir: &Path) -> Result<bool> {
        let smritiignore = dir.join(".smritiignore");
        if !smritiignore.is_file() {
            return Ok(false);
        }
        let content = fs::read_to_string(&smritiignore)?;
        let rules = parse_smritiignore(&content, dir)?;
        self.layers.push((dir.to_path_buf(), rules));
        Ok(true)
    }

    /// Pop the most-recently-pushed layer. No-op if the stack is empty.
    pub fn pop(&mut self) {
        self.layers.pop();
    }

    /// Classify a path by checking all layers from most-specific to global.
    ///
    /// Priority per layer (highest to lowest):
    ///   1. ignored   — if the ignored matcher fires → `Ignored`
    ///   2. no_embed  — if the no_embed matcher fires → `IndexedNoEmbed`
    ///   3. cataloged — if the cataloged matcher fires → `Cataloged`
    ///
    /// If no layer matches, returns `Indexed`.
    pub fn classify(&self, path: &Path, is_dir: bool) -> PathClassification {
        // Check layers from most-specific (top) to least-specific (bottom),
        // then fall through to the global base.
        for (_dir, rules) in self.layers.iter().rev() {
            if let Some(c) = classify_against(rules, path, is_dir) {
                return c;
            }
        }
        if let Some(c) = classify_against(&self.global, path, is_dir) {
            return c;
        }
        PathClassification::Indexed
    }
}

/// Check a single `SectionRules` layer against `path`.
///
/// Returns `None` if none of the matchers fire (so the caller can continue
/// to a less-specific layer).
fn classify_against(
    rules: &SectionRules,
    path: &Path,
    is_dir: bool,
) -> Option<PathClassification> {
    // `ignored` has highest priority within a layer.
    match match_path(&rules.ignored, path, is_dir) {
        Match::Ignore(_) => return Some(PathClassification::Ignored),
        Match::Whitelist(_) => {
            // Explicit negation in the ignored section — path is un-ignored;
            // still check the other sections.
        }
        Match::None => {}
    }

    match match_path(&rules.no_embed, path, is_dir) {
        Match::Ignore(_) => return Some(PathClassification::IndexedNoEmbed),
        Match::Whitelist(_) | Match::None => {}
    }

    match match_path(&rules.cataloged, path, is_dir) {
        Match::Ignore(_) => return Some(PathClassification::Cataloged),
        Match::Whitelist(_) | Match::None => {}
    }

    None
}

/// Match a path against a `Gitignore` matcher.
///
/// Uses `Gitignore::matched` which handles prefix-stripping internally and
/// does not panic on absolute paths.
fn match_path<'a>(gi: &'a Gitignore, path: &Path, is_dir: bool) -> Match<&'a ignore::gitignore::Glob> {
    gi.matched(path, is_dir)
}
