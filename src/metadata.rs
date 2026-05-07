//! Heuristic metadata extraction: title, summary, topics, document structure,
//! binary detection, MIME type mapping, and file extension helpers.

use std::path::Path;

use crate::hasher::split_frontmatter;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Extracted metadata for a document.
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentMetadata {
    pub title: Option<String>,
    pub summary: Option<String>,
    pub topics: Vec<String>,
    pub structure: Vec<Section>,
    pub is_binary: bool,
}

/// A heading entry in a document's structure.
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub heading: String,
    pub level: u32,
    pub line: u32,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Detect whether content is binary by scanning the first 8 192 bytes for
/// null bytes (0x00).
pub fn is_binary(content: &[u8]) -> bool {
    let sample_len = content.len().min(8192);
    content[..sample_len].contains(&0u8)
}

/// Return the lowercase file extension, or `None` if there is none.
pub fn file_extension(path: &Path) -> Option<String> {
    path.extension()
        .map(|ext| ext.to_string_lossy().to_lowercase())
}

/// Extension-based MIME type detection.  Falls back to
/// `application/octet-stream` for unknown types.
pub fn detect_mime_type(path: &Path) -> String {
    match file_extension(path).as_deref() {
        Some("md") | Some("markdown") => "text/markdown",
        Some("rs") => "text/x-rust",
        Some("py") => "text/x-python",
        Some("txt") => "text/plain",
        Some("json") => "application/json",
        Some("yaml") | Some("yml") => "application/yaml",
        Some("toml") => "application/toml",
        Some("html") | Some("htm") => "text/html",
        Some("css") => "text/css",
        Some("js") => "text/javascript",
        Some("ts") => "text/typescript",
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Extract metadata from file content, dispatching by extension.
///
/// - `.md` / `.markdown` → full markdown extraction
/// - Binary (has null bytes) → `is_binary = true`, title = filename
/// - Other text → title = stem, summary = first non-empty line
pub fn extract_metadata(path: &Path, content: &[u8]) -> DocumentMetadata {
    // Binary files: skip all extraction.
    if is_binary(content) {
        return DocumentMetadata {
            title: path.file_name().map(|n| n.to_string_lossy().into_owned()),
            summary: None,
            topics: vec![],
            structure: vec![],
            is_binary: true,
        };
    }

    let ext = file_extension(path);
    let is_markdown = matches!(ext.as_deref(), Some("md") | Some("markdown"));

    if is_markdown {
        extract_markdown_metadata(path, content)
    } else {
        extract_text_metadata(path, content)
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Extract metadata from a Markdown document.
fn extract_markdown_metadata(path: &Path, content: &[u8]) -> DocumentMetadata {
    // Strip frontmatter before extracting headings / paragraphs.
    let (_, body) = split_frontmatter(content);

    let body_str = match std::str::from_utf8(body) {
        Ok(s) => s,
        Err(_) => {
            // Not valid UTF-8 after stripping frontmatter — treat as binary.
            return DocumentMetadata {
                title: path.file_name().map(|n| n.to_string_lossy().into_owned()),
                summary: None,
                topics: vec![],
                structure: vec![],
                is_binary: true,
            };
        }
    };

    let mut structure: Vec<Section> = vec![];
    let mut title: Option<String> = None;
    let mut summary: Option<String> = None;
    let mut in_paragraph = false;
    let mut paragraph_buf = String::new();

    // 1-indexed line counter; body starts after frontmatter so we number from 1.
    for (line_num, raw_line) in (1_u32..).zip(body_str.lines()) {
        let line = raw_line.trim_end();

        if let Some(heading_text) = parse_heading(line) {
            let level = heading_level(line);
            let text = heading_text.to_string();

            if title.is_none() && level == 1 {
                title = Some(text.clone());
            }

            structure.push(Section {
                heading: text,
                level,
                line: line_num,
            });

            // A heading resets any open paragraph accumulation.
            in_paragraph = false;
            paragraph_buf.clear();
        } else if summary.is_none() {
            // Accumulate the first non-heading, non-empty paragraph for summary.
            if line.is_empty() {
                if in_paragraph && !paragraph_buf.is_empty() {
                    // End of paragraph — capture it as summary.
                    let s = paragraph_buf.trim().to_string();
                    if !s.is_empty() {
                        summary = Some(truncate_to_chars(&s, 200));
                    }
                    in_paragraph = false;
                    paragraph_buf.clear();
                }
            } else {
                in_paragraph = true;
                if !paragraph_buf.is_empty() {
                    paragraph_buf.push(' ');
                }
                paragraph_buf.push_str(line);
            }
        }
    }

    // Capture a trailing paragraph with no following blank line.
    if summary.is_none() && in_paragraph && !paragraph_buf.is_empty() {
        let s = paragraph_buf.trim().to_string();
        if !s.is_empty() {
            summary = Some(truncate_to_chars(&s, 200));
        }
    }

    // Topics: unique, lowercased heading texts.
    let mut topics: Vec<String> = vec![];
    let mut seen = std::collections::HashSet::new();
    for sec in &structure {
        let lower = sec.heading.to_lowercase();
        if seen.insert(lower.clone()) {
            topics.push(lower);
        }
    }

    DocumentMetadata {
        title,
        summary,
        topics,
        structure,
        is_binary: false,
    }
}

/// Extract metadata from a non-markdown text file.
fn extract_text_metadata(path: &Path, content: &[u8]) -> DocumentMetadata {
    let title = path.file_stem().map(|s| s.to_string_lossy().into_owned());

    let summary = std::str::from_utf8(content).ok().and_then(|s| {
        s.lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(|l| truncate_to_chars(l, 200))
    });

    DocumentMetadata {
        title,
        summary,
        topics: vec![],
        structure: vec![],
        is_binary: false,
    }
}

/// Returns the heading text (without leading `#` chars and space) if `line`
/// is an ATX heading, otherwise `None`.
fn parse_heading(line: &str) -> Option<&str> {
    let trimmed = line.trim_start_matches('#');
    let hashes = line.len() - trimmed.len();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    // Must be followed by a space (or be empty — though unusual).
    if trimmed.starts_with(' ') {
        Some(trimmed.trim_start_matches(' '))
    } else if trimmed.is_empty() {
        Some("")
    } else {
        None
    }
}

/// Count the number of leading `#` characters (1–6).
fn heading_level(line: &str) -> u32 {
    line.chars().take_while(|&c| c == '#').count() as u32
}

/// Truncate `s` to at most `max_chars` Unicode scalar values.
fn truncate_to_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        s.chars().take(max_chars).collect()
    }
}

// ---------------------------------------------------------------------------
// Inline tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn md_path() -> PathBuf {
        PathBuf::from("doc.md")
    }

    fn txt_path() -> PathBuf {
        PathBuf::from("notes.txt")
    }

    #[test]
    fn test_title_from_first_heading() {
        let content = b"# My Great Title\n\n## Section One\n\nSome body text.\n";
        let meta = extract_metadata(&md_path(), content);
        assert_eq!(meta.title, Some("My Great Title".to_string()));
    }

    #[test]
    fn test_title_fallback_to_filename() {
        // A text file with no headings should use the file stem.
        let content = b"just some text\nmore text\n";
        let meta = extract_metadata(&txt_path(), content);
        assert_eq!(meta.title, Some("notes".to_string()));
    }

    #[test]
    fn test_topics_from_headings() {
        let content = b"# Intro\n\n## Setup\n\n## Teardown\n\n## Setup\n";
        let meta = extract_metadata(&md_path(), content);
        // "setup" appears twice but must be deduplicated.
        assert!(meta.topics.contains(&"intro".to_string()));
        assert!(meta.topics.contains(&"setup".to_string()));
        assert!(meta.topics.contains(&"teardown".to_string()));
        let setup_count = meta.topics.iter().filter(|t| t.as_str() == "setup").count();
        assert_eq!(setup_count, 1, "setup should only appear once");
    }

    #[test]
    fn test_structure_hierarchy() {
        let content = b"# Top\n\n## Sub\n\n### Sub-sub\n\n## Another\n";
        let meta = extract_metadata(&md_path(), content);
        assert_eq!(meta.structure.len(), 4);
        assert_eq!(meta.structure[0].level, 1);
        assert_eq!(meta.structure[1].level, 2);
        assert_eq!(meta.structure[2].level, 3);
        assert_eq!(meta.structure[3].level, 2);
        // Line numbers must be 1-indexed and monotonically increasing.
        for w in meta.structure.windows(2) {
            assert!(w[1].line > w[0].line);
        }
    }

    #[test]
    fn test_binary_detection() {
        let mut content = b"normal text ".to_vec();
        content.push(0x00); // null byte → binary
        content.extend_from_slice(b" more text");
        assert!(is_binary(&content));

        let text_content = b"completely normal utf-8 text without null bytes";
        assert!(!is_binary(text_content));

        let path = PathBuf::from("file.bin");
        let meta = extract_metadata(&path, &content);
        assert!(meta.is_binary);
        assert_eq!(meta.title, Some("file.bin".to_string()));
    }

    #[test]
    fn test_mime_type_from_extension() {
        assert_eq!(
            detect_mime_type(&PathBuf::from("readme.md")),
            "text/markdown"
        );
        assert_eq!(detect_mime_type(&PathBuf::from("lib.rs")), "text/x-rust");
        assert_eq!(detect_mime_type(&PathBuf::from("app.py")), "text/x-python");
        assert_eq!(
            detect_mime_type(&PathBuf::from("data.json")),
            "application/json"
        );
        assert_eq!(
            detect_mime_type(&PathBuf::from("config.yaml")),
            "application/yaml"
        );
        assert_eq!(
            detect_mime_type(&PathBuf::from("config.yml")),
            "application/yaml"
        );
        assert_eq!(
            detect_mime_type(&PathBuf::from("unknown.xyz")),
            "application/octet-stream"
        );
    }
}
