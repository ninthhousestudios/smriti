//! Integration tests for the ignore parser (Issue 2).

use std::fs;
use std::path::Path;

use tempfile::TempDir;

use smriti::ignore::{hardened_defaults, parse_smritiignore, IgnoreStack, PathClassification};

// ── helpers ──────────────────────────────────────────────────────────────────

/// Write a .smritiignore file into `dir`.
fn write_smritiignore(dir: &Path, content: &str) {
    fs::write(dir.join(".smritiignore"), content).unwrap();
}

/// Classify `rel` (relative to `base`) using an `IgnoreStack` seeded with
/// `content` as the base dir's .smritiignore.  `is_dir` controls the flag.
fn classify_with(base: &Path, content: &str, rel: &str, is_dir: bool) -> PathClassification {
    let rules = parse_smritiignore(content, base).unwrap();
    let stack = IgnoreStack::new(rules);
    let full = base.join(rel);
    stack.classify(&full, is_dir)
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// Empty file produces empty rules; any path classifies as Indexed.
#[test]
fn test_parse_empty_file() {
    let tmp = TempDir::new().unwrap();
    let rules = parse_smritiignore("", tmp.path()).unwrap();
    assert!(rules.is_empty());

    let stack = IgnoreStack::new(rules);
    assert_eq!(
        stack.classify(&tmp.path().join("anything.txt"), false),
        PathClassification::Indexed
    );
}

/// Standard gitignore-style patterns in the default section classify as Ignored.
#[test]
fn test_parse_basic_patterns() {
    let tmp = TempDir::new().unwrap();
    let content = "*.log\n*.tmp\n";

    assert_eq!(
        classify_with(tmp.path(), content, "app.log", false),
        PathClassification::Ignored
    );
    assert_eq!(
        classify_with(tmp.path(), content, "build.tmp", false),
        PathClassification::Ignored
    );
    assert_eq!(
        classify_with(tmp.path(), content, "main.rs", false),
        PathClassification::Indexed
    );
}

/// Patterns under `[catalog]` classify as Cataloged.
#[test]
fn test_parse_catalog_section() {
    let tmp = TempDir::new().unwrap();
    let content = "[catalog]\n**/node_modules/\n**/target/\n";

    assert_eq!(
        classify_with(tmp.path(), content, "node_modules", true),
        PathClassification::Cataloged
    );
    assert_eq!(
        classify_with(tmp.path(), content, "target", true),
        PathClassification::Cataloged
    );
    // A file not in either section stays Indexed.
    assert_eq!(
        classify_with(tmp.path(), content, "src/main.rs", false),
        PathClassification::Indexed
    );
}

/// Patterns under `[no-embed]` classify as IndexedNoEmbed.
#[test]
fn test_parse_no_embed_section() {
    let tmp = TempDir::new().unwrap();
    let content = "[no-embed]\n**/*.lic\n**/license-keys/\n";

    assert_eq!(
        classify_with(tmp.path(), content, "vendor.lic", false),
        PathClassification::IndexedNoEmbed
    );
    assert_eq!(
        classify_with(tmp.path(), content, "src/main.rs", false),
        PathClassification::Indexed
    );
}

/// Hardened defaults must classify all known secret-bearing patterns as Ignored.
#[test]
fn test_hardened_defaults_cover_secrets() {
    let tmp = TempDir::new().unwrap();
    let global = hardened_defaults(tmp.path());
    let stack = IgnoreStack::new(global);

    let secrets = [
        (".env", false),
        (".env.local", false),
        ("server.pem", false),
        ("private.key", false),
        ("id_rsa", false),
        ("id_rsa.pub", false),
        (".ssh", true),
        ("db.kdbx", false),
        ("secrets.json", false),
        ("credentials", false),
        (".aws", true),
        (".gnupg", true),
    ];

    for (rel, is_dir) in &secrets {
        let path = tmp.path().join(rel);
        let got = stack.classify(&path, *is_dir);
        assert_eq!(
            got,
            PathClassification::Ignored,
            "expected {rel} to be Ignored, got {got:?}"
        );
    }
}

/// Subdirectory .smritiignore overrides parent rules via push_dir / pop.
#[test]
fn test_nested_ignore_files() {
    let tmp = TempDir::new().unwrap();

    // Parent: ignore *.bak
    write_smritiignore(tmp.path(), "*.bak\n");

    // Child subdir: ignore *.txt (but not *.bak)
    let child = tmp.path().join("subdir");
    fs::create_dir(&child).unwrap();
    write_smritiignore(&child, "*.txt\n");

    // Build stack with parent rules as global.
    let parent_rules = parse_smritiignore("*.bak\n", tmp.path()).unwrap();
    let mut stack = IgnoreStack::new(parent_rules);

    // Before push: *.txt not ignored
    assert_eq!(
        stack.classify(&child.join("notes.txt"), false),
        PathClassification::Indexed
    );

    // After push child dir: *.txt now ignored, *.bak still ignored (global)
    let pushed = stack.push_dir(&child).unwrap();
    assert!(pushed, "expected child layer to be pushed");

    assert_eq!(
        stack.classify(&child.join("notes.txt"), false),
        PathClassification::Ignored,
        "*.txt should be Ignored after push"
    );
    assert_eq!(
        stack.classify(&child.join("backup.bak"), false),
        PathClassification::Ignored,
        "*.bak should still be Ignored via global rules"
    );

    // After pop: *.txt no longer ignored
    stack.pop();
    assert_eq!(
        stack.classify(&child.join("notes.txt"), false),
        PathClassification::Indexed,
        "*.txt should be Indexed after pop"
    );
}

/// Full pipeline: build a stack from multi-section content and classify several paths.
#[test]
fn test_classify_path_pipeline() {
    let tmp = TempDir::new().unwrap();
    let content = "*.swp\n*.tmp\n[catalog]\n**/node_modules/\n[no-embed]\n**/*.lic\n";
    let rules = parse_smritiignore(content, tmp.path()).unwrap();
    let stack = IgnoreStack::new(rules);

    let cases: &[(&str, bool, PathClassification)] = &[
        ("editor.swp", false, PathClassification::Ignored),
        ("cache.tmp", false, PathClassification::Ignored),
        ("node_modules", true, PathClassification::Cataloged),
        ("vendor.lic", false, PathClassification::IndexedNoEmbed),
        ("src/main.rs", false, PathClassification::Indexed),
    ];

    for (rel, is_dir, expected) in cases {
        let path = tmp.path().join(rel);
        let got = stack.classify(&path, *is_dir);
        assert_eq!(
            got, *expected,
            "classify({rel}, {is_dir}) expected {expected:?}, got {got:?}"
        );
    }
}

/// `!important.env` negation un-ignores a file that would otherwise match `*.env`.
#[test]
fn test_negation_pattern() {
    let tmp = TempDir::new().unwrap();
    // .env.* is in hardened defaults, so use a fresh ruleset with explicit patterns.
    let content = ".env\n.env.*\n!.env.important\n";
    let rules = parse_smritiignore(content, tmp.path()).unwrap();
    let stack = IgnoreStack::new(rules);

    // Regular .env files should be ignored.
    assert_eq!(
        stack.classify(&tmp.path().join(".env"), false),
        PathClassification::Ignored
    );
    assert_eq!(
        stack.classify(&tmp.path().join(".env.local"), false),
        PathClassification::Ignored
    );

    // The negated file should NOT be ignored — it should be Indexed.
    // Gitignore semantics: later rule (!.env.important) overrides earlier (.env.*).
    assert_eq!(
        stack.classify(&tmp.path().join(".env.important"), false),
        PathClassification::Indexed,
        ".env.important should be un-ignored by negation"
    );
}

/// Trailing `/` patterns match directories only, not files with the same name.
#[test]
fn test_directory_only_pattern() {
    let tmp = TempDir::new().unwrap();
    let content = "build/\n";
    let rules = parse_smritiignore(content, tmp.path()).unwrap();
    let stack = IgnoreStack::new(rules);

    // The directory named "build" should be ignored.
    assert_eq!(
        stack.classify(&tmp.path().join("build"), true),
        PathClassification::Ignored,
        "directory 'build' should be Ignored"
    );

    // A file named "build" (no trailing slash) should NOT be ignored.
    assert_eq!(
        stack.classify(&tmp.path().join("build"), false),
        PathClassification::Indexed,
        "file 'build' should NOT be Ignored by a dir-only pattern"
    );
}

/// A pattern with a leading `/` is anchored to the ignore file's base directory.
/// `/foo` matches `<base>/foo` but not `<base>/sub/foo`.
#[test]
fn test_anchored_pattern() {
    let tmp = TempDir::new().unwrap();
    // Leading slash anchors to the root of the ignore file's dir.
    let content = "/secret.txt\n";
    let rules = parse_smritiignore(content, tmp.path()).unwrap();
    let stack = IgnoreStack::new(rules);

    // Direct match at root — should be Ignored.
    assert_eq!(
        stack.classify(&tmp.path().join("secret.txt"), false),
        PathClassification::Ignored,
        "anchored pattern should match at root"
    );

    // Nested path — should NOT be Ignored (anchored patterns don't recurse).
    assert_eq!(
        stack.classify(&tmp.path().join("sub/secret.txt"), false),
        PathClassification::Indexed,
        "anchored pattern should NOT match in subdirectory"
    );
}

/// `~/foo/` in a smritiignore must anchor to base_dir (intended to be HOME).
/// Gitignore semantics don't expand `~`, so the parser strips it so the pattern
/// becomes `/foo/` (anchored). Regression test for the manifest bug where
/// `~/Downloads/` in ~/.smritiignore matched nothing.
#[test]
fn test_tilde_prefix_anchors_to_base() {
    let tmp = TempDir::new().unwrap();
    let content = "[catalog]\n~/Downloads/\n~/dev/\n";

    // base = tmp acts as HOME. The scanner classifies directories and
    // skips matching subtrees, so we only need ~/Downloads/ to match the
    // directory itself; child files aren't visited.
    assert_eq!(
        classify_with(tmp.path(), content, "Downloads", true),
        PathClassification::Cataloged,
        "~/Downloads/ must match the Downloads dir at base"
    );
    assert_eq!(
        classify_with(tmp.path(), content, "dev", true),
        PathClassification::Cataloged,
        "~/dev/ must match the dev dir at base"
    );

    // Anchoring: ~/Downloads/ should NOT match a nested Downloads dir.
    assert_eq!(
        classify_with(tmp.path(), content, "sub/Downloads", true),
        PathClassification::Indexed,
        "~/Downloads/ is anchored and must not match a nested Downloads dir"
    );
}

/// `!~/foo/` (negation with tilde) parses correctly.
#[test]
fn test_tilde_negation_parses() {
    let tmp = TempDir::new().unwrap();
    // Catalog everything, then un-catalog ~/keep/.
    let content = "[catalog]\n*\n!~/keep/\n";
    let rules = parse_smritiignore(content, tmp.path());
    assert!(rules.is_ok(), "!~/keep/ must parse");
}
