//! Blake3 content hashing, frontmatter splitting, and minor-change detection.

use std::fs::File;
use std::path::Path;

use crate::error::Result;
use crate::error::SmritiError;

/// Hash arbitrary bytes with blake3, returning a lowercase hex string.
pub fn hash_content(content: &[u8]) -> String {
    blake3::hash(content).to_hex().to_string()
}

/// Hash a file via streaming blake3 — safe for GB-scale files.
pub fn hash_file(path: &Path) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    let file = File::open(path).map_err(SmritiError::Io)?;
    hasher
        .update_reader(file)
        .map_err(SmritiError::Io)?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// Split YAML (`---\n`) or TOML (`+++\n`) frontmatter from content.
///
/// Returns `(Some(frontmatter_bytes), body_bytes)` when a valid frontmatter
/// block is found, otherwise `(None, content)`.  The returned frontmatter
/// slice does **not** include the opening or closing delimiter lines.  The
/// body starts immediately after the newline that terminates the closing
/// delimiter line.
pub fn split_frontmatter(content: &[u8]) -> (Option<&[u8]>, &[u8]) {
    // Detect opening delimiter: "---\n" or "+++\n"
    let (delimiter, close_tag): (&[u8], &[u8]) = if content.starts_with(b"---\n") {
        (b"---\n", b"---")
    } else if content.starts_with(b"+++\n") {
        (b"+++\n", b"+++")
    } else {
        return (None, content);
    };

    // Frontmatter content starts right after the opening delimiter line.
    let after_open = &content[delimiter.len()..];

    // Search for the matching closing delimiter on its own line.
    // We look for "\n---\n" / "\n+++\n" (preceded by a newline) so the
    // delimiter must be on its own line.
    let mut search_pos = 0;
    loop {
        // Find the next newline from search_pos.
        match memchr(b'\n', &after_open[search_pos..]) {
            None => return (None, content), // no closing delimiter found
            Some(rel) => {
                let line_start = search_pos + rel + 1; // byte after the '\n'
                if after_open.len() < line_start + close_tag.len() {
                    return (None, content);
                }
                if &after_open[line_start..line_start + close_tag.len()] == close_tag {
                    // Check that this is a complete line (ends with '\n' or is end-of-content).
                    let after_close = line_start + close_tag.len();
                    let line_ends =
                        after_open.len() == after_close || after_open[after_close] == b'\n';
                    if line_ends {
                        let frontmatter = &after_open[..search_pos + rel]; // exclude the '\n' before close tag
                        // Body starts after the closing delimiter line (including its '\n').
                        let body_start = delimiter.len()
                            + after_close
                            + if after_open.len() > after_close { 1 } else { 0 };
                        let body = if body_start <= content.len() {
                            &content[body_start..]
                        } else {
                            b""
                        };
                        return (Some(frontmatter), body);
                    }
                }
                search_pos = line_start;
            }
        }
    }
}

/// Find the first occurrence of `needle` in `haystack`, returning its index.
fn memchr(needle: u8, haystack: &[u8]) -> Option<usize> {
    haystack.iter().position(|&b| b == needle)
}

/// Hash only the body portion of content (after any frontmatter).
pub fn hash_body(content: &[u8]) -> String {
    let (_, body) = split_frontmatter(content);
    hash_content(body)
}

/// Returns `true` when a frontmatter-only edit occurred:
/// content hashes differ, but body hashes are identical.
pub fn detect_minor_change(
    old_content_hash: &str,
    new_content_hash: &str,
    old_body_hash: &str,
    new_body_hash: &str,
) -> bool {
    old_content_hash != new_content_hash && old_body_hash == new_body_hash
}

// ---------------------------------------------------------------------------
// Inline tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blake3_consistency() {
        let content = b"hello, smriti";
        assert_eq!(hash_content(content), hash_content(content));
    }

    #[test]
    fn test_blake3_different_content() {
        let a = hash_content(b"alpha");
        let b = hash_content(b"beta");
        assert_ne!(a, b);
    }

    #[test]
    fn test_frontmatter_yaml_detection() {
        let doc = b"---\ntitle: Hello\ndate: 2024-01-01\n---\n\nBody text here.\n";
        let (fm, body) = split_frontmatter(doc);
        assert!(fm.is_some(), "expected frontmatter to be detected");
        let fm = fm.unwrap();
        assert!(fm.contains(&b't'), "frontmatter should contain content");
        assert!(std::str::from_utf8(fm).unwrap().contains("title: Hello"));
        assert!(std::str::from_utf8(body).unwrap().contains("Body text here."));
        // Delimiter lines must not appear in either slice.
        assert!(!std::str::from_utf8(fm).unwrap().starts_with("---"));
        assert!(!std::str::from_utf8(body).unwrap().starts_with("---"));
    }

    #[test]
    fn test_frontmatter_toml_detection() {
        let doc = b"+++\ntitle = \"Hello\"\n+++\n\nBody text here.\n";
        let (fm, body) = split_frontmatter(doc);
        assert!(fm.is_some(), "expected TOML frontmatter to be detected");
        let fm_str = std::str::from_utf8(fm.unwrap()).unwrap();
        assert!(fm_str.contains("title = \"Hello\""));
        let body_str = std::str::from_utf8(body).unwrap();
        assert!(body_str.contains("Body text here."));
        assert!(!fm_str.starts_with("+++"));
        assert!(!body_str.starts_with("+++"));
    }

    #[test]
    fn test_body_hash_stable_across_frontmatter_edits() {
        let body_text = b"\n# My Document\n\nSome content.\n";
        let v1 = {
            let mut doc = b"---\ntitle: Old Title\n---\n".to_vec();
            doc.extend_from_slice(body_text);
            doc
        };
        let v2 = {
            let mut doc = b"---\ntitle: New Title\ndate: 2025-01-01\n---\n".to_vec();
            doc.extend_from_slice(body_text);
            doc
        };

        // Content hashes should differ (frontmatter changed).
        assert_ne!(hash_content(&v1), hash_content(&v2));
        // Body hashes must be equal.
        assert_eq!(hash_body(&v1), hash_body(&v2));

        // detect_minor_change should return true.
        let old_ch = hash_content(&v1);
        let new_ch = hash_content(&v2);
        let old_bh = hash_body(&v1);
        let new_bh = hash_body(&v2);
        assert!(detect_minor_change(&old_ch, &new_ch, &old_bh, &new_bh));
    }

    #[test]
    fn test_no_frontmatter() {
        let doc = b"# Just a heading\n\nSome text.\n";
        let (fm, body) = split_frontmatter(doc);
        assert!(fm.is_none(), "expected no frontmatter");
        assert_eq!(body, doc, "body should equal full content when no frontmatter");
    }
}
