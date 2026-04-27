//! Integration tests for the privacy gate (Issue 4).
//!
//! Every test uses a real (non-WAL-critical) DB file on disk via `NamedTempFile`
//! so that WAL mode works correctly.  In-memory SQLite is avoided because WAL
//! mode requires a real file path.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::{NamedTempFile, TempDir};

use smriti::db;
use smriti::error::SmritiError;
use smriti::ignore::hardened_defaults;
use smriti::privacy::PrivacyGate;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Open an on-disk SQLite DB (WAL mode) via a NamedTempFile.
/// Returns (conn, _tempfile) — keep _tempfile alive for the test's duration.
fn open_test_db() -> (rusqlite::Connection, NamedTempFile) {
    let f = NamedTempFile::new().expect("create tempfile for DB");
    let conn = db::open(f.path()).expect("open test DB");
    (conn, f)
}

/// Build a `PrivacyGate` using hardened defaults anchored to `base_dir`.
fn make_gate(roots: Vec<PathBuf>, base_dir: &Path) -> PrivacyGate {
    let rules = hardened_defaults(base_dir);
    PrivacyGate::new(roots, rules).expect("PrivacyGate::new")
}

/// Create a regular file at `path` with `content`.
fn write_file(path: &Path, content: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

// ---------------------------------------------------------------------------
// test_allowlist_enforcement
//
// A read request whose canonical path lies outside all configured roots must
// return SmritiError::OutsideAllowlist.
// ---------------------------------------------------------------------------

#[test]
fn test_allowlist_enforcement() {
    let root_dir = TempDir::new().unwrap();
    let other_dir = TempDir::new().unwrap();

    // Put a real file in other_dir (which is NOT a root).
    let target = other_dir.path().join("secret.txt");
    write_file(&target, b"outside the allowlist");

    let (conn, _db) = open_test_db();
    let gate = make_gate(vec![root_dir.path().to_path_buf()], root_dir.path());

    let err = gate.read_file(&conn, &target, None).unwrap_err();
    assert!(
        matches!(err, SmritiError::OutsideAllowlist { .. }),
        "expected OutsideAllowlist, got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// test_ignored_path_rejection
//
// Files matching hardened-default patterns (e.g. `.env`) must be rejected
// with SmritiError::IgnoredPath even when they are inside an allowed root.
// ---------------------------------------------------------------------------

#[test]
fn test_ignored_path_rejection() {
    let root_dir = TempDir::new().unwrap();

    // .env is in the hardened-defaults ignored section.
    let dotenv = root_dir.path().join(".env");
    write_file(&dotenv, b"SECRET_KEY=hunter2");

    let (conn, _db) = open_test_db();
    // Anchor hardened_defaults to the root so patterns like `.env` match.
    let gate = make_gate(vec![root_dir.path().to_path_buf()], root_dir.path());

    let err = gate.read_file(&conn, &dotenv, None).unwrap_err();
    assert!(
        matches!(err, SmritiError::IgnoredPath { .. }),
        "expected IgnoredPath for .env, got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// test_tier1_read_allowed
//
// A regular, non-ignored file inside an allowed root must succeed.
// ---------------------------------------------------------------------------

#[test]
fn test_tier1_read_allowed() {
    let root_dir = TempDir::new().unwrap();
    let file = root_dir.path().join("notes.md");
    write_file(&file, b"# My Notes\n\nSome content.");

    let (conn, _db) = open_test_db();
    let gate = make_gate(vec![root_dir.path().to_path_buf()], root_dir.path());

    let result = gate.read_file(&conn, &file, None);
    assert!(result.is_ok(), "expected Ok, got: {:?}", result.unwrap_err());

    let rr = result.unwrap();
    assert!(!rr.content.is_empty(), "content must not be empty");
    assert!(!rr.content_hash.is_empty(), "content_hash must not be empty");
    assert_eq!(rr.content, b"# My Notes\n\nSome content.");
}

// ---------------------------------------------------------------------------
// test_read_audit_logging
//
// Every successful read_file call must insert exactly one row into read_audit.
// ---------------------------------------------------------------------------

#[test]
fn test_read_audit_logging() {
    let root_dir = TempDir::new().unwrap();
    let file = root_dir.path().join("document.txt");
    write_file(&file, b"audit me");

    let (conn, _db) = open_test_db();
    let gate = make_gate(vec![root_dir.path().to_path_buf()], root_dir.path());

    // No audit rows yet.
    let before: i64 = conn
        .query_row("SELECT COUNT(*) FROM read_audit", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, 0);

    gate.read_file(&conn, &file, Some("test-caller")).unwrap();

    let after: i64 = conn
        .query_row("SELECT COUNT(*) FROM read_audit", [], |r| r.get(0))
        .unwrap();
    assert_eq!(after, 1, "exactly one audit row should be inserted per read");

    // Verify the stored values make sense.
    let (stored_path, stored_caller): (String, Option<String>) = conn
        .query_row(
            "SELECT path, caller FROM read_audit LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();

    assert!(
        stored_path.contains("document.txt"),
        "stored path should contain filename"
    );
    assert_eq!(stored_caller.as_deref(), Some("test-caller"));

    // A second read produces a second row.
    gate.read_file(&conn, &file, None).unwrap();
    let final_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM read_audit", [], |r| r.get(0))
        .unwrap();
    assert_eq!(final_count, 2);
}

// ---------------------------------------------------------------------------
// test_path_traversal_blocked
//
// A path that escapes the allowlisted root must be rejected after
// canonicalization — regardless of how the path is constructed (absolute,
// `..`-relative, or via a symlink).
//
// Two sub-cases:
//   A. Direct absolute path outside root → OutsideAllowlist
//   B. Symlink inside root that points outside root → OutsideAllowlist
//      (symlinks are resolved before the allowlist check)
// ---------------------------------------------------------------------------

#[test]
fn test_path_traversal_blocked() {
    let root_dir = TempDir::new().unwrap();

    // ── Sub-case A: direct absolute path outside the root ────────────────────
    //
    // Use a second temp dir as the "target outside the root".  We place a
    // real file there so canonicalize succeeds but the allowlist rejects it.
    let outside_dir = TempDir::new().unwrap();
    let outside_file = outside_dir.path().join("secret.txt");
    write_file(&outside_file, b"outside root content");

    let gate = make_gate(vec![root_dir.path().to_path_buf()], root_dir.path());

    // Direct path — canonicalize succeeds (file exists), allowlist rejects.
    let err = gate.can_read(&outside_file).unwrap_err();
    assert!(
        matches!(err, SmritiError::OutsideAllowlist { .. }),
        "direct path outside root must be OutsideAllowlist, got: {err:?}"
    );

    // ── Sub-case B: symlink inside root pointing outside root ────────────────
    //
    // Create a symlink inside root_dir that resolves to outside_file.
    // After canonicalization, the target is outside the root → rejected.
    let link_inside_root = root_dir.path().join("escape-link.txt");
    std::os::unix::fs::symlink(&outside_file, &link_inside_root).unwrap();

    let err = gate.can_read(&link_inside_root).unwrap_err();
    assert!(
        matches!(err, SmritiError::OutsideAllowlist { .. }),
        "symlink escaping root must be OutsideAllowlist after canonicalize, got: {err:?}"
    );

    // ── Sub-case C: `..`-path that resolves outside root ─────────────────────
    //
    // Construct a path that uses `..` to walk up to outside_dir.
    // This requires the path to exist on disk, so we manually build a valid
    // traversal: <root>/link-to-outside/../secret.txt.
    // The link already points to outside_file's parent implicitly… instead
    // let's link to the directory.
    let link_to_outside_dir = root_dir.path().join("link-to-outside-dir");
    std::os::unix::fs::symlink(outside_dir.path(), &link_to_outside_dir).unwrap();

    // <root>/link-to-outside-dir/secret.txt canonicalizes to outside_file.
    let via_link_dir = link_to_outside_dir.join("secret.txt");
    let err = gate.can_read(&via_link_dir).unwrap_err();
    assert!(
        matches!(err, SmritiError::OutsideAllowlist { .. }),
        "path via dir-symlink escaping root must be OutsideAllowlist, got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// test_non_home_root
//
// Roots do not have to be under `~`.  A root at a /tmp path (or any absolute
// path not under the home directory) must work correctly.
// ---------------------------------------------------------------------------

#[test]
fn test_non_home_root() {
    // Use a real /tmp-based temp dir — not under $HOME.
    let tmp_root = TempDir::new().unwrap();
    let file = tmp_root.path().join("backup-manifest.txt");
    write_file(&file, b"file1.tar.gz\nfile2.tar.gz\n");

    let (conn, _db) = open_test_db();
    // Anchor hardened_defaults to the root so patterns don't accidentally
    // match the short /tmp path.
    let gate = make_gate(vec![tmp_root.path().to_path_buf()], tmp_root.path());

    let result = gate.read_file(&conn, &file, Some("backup-agent"));
    assert!(result.is_ok(), "non-home root should work; got: {:?}", result.unwrap_err());

    let rr = result.unwrap();
    assert_eq!(rr.content, b"file1.tar.gz\nfile2.tar.gz\n");

    // Confirm audit row recorded.
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM read_audit", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

// ---------------------------------------------------------------------------
// Additional coverage: non-existent root is skipped without error
// ---------------------------------------------------------------------------

#[test]
fn test_nonexistent_root_skipped() {
    // Gate created with a non-existent root must not fail — it just skips it.
    let fake_root = PathBuf::from("/tmp/smriti-nonexistent-root-xyzzy-12345");
    let rules = hardened_defaults(Path::new("/tmp"));
    let gate = PrivacyGate::new(vec![fake_root], rules);
    assert!(gate.is_ok(), "PrivacyGate::new must succeed even when root doesn't exist");
}

// ---------------------------------------------------------------------------
// Additional coverage: can_read on non-existent file → Io error
// ---------------------------------------------------------------------------

#[test]
fn test_nonexistent_file_returns_io_error() {
    let root_dir = TempDir::new().unwrap();
    // Don't create the file.
    let missing = root_dir.path().join("does-not-exist.txt");

    let gate = make_gate(vec![root_dir.path().to_path_buf()], root_dir.path());
    let err = gate.can_read(&missing).unwrap_err();
    assert!(
        matches!(err, SmritiError::Io(_)),
        "expected Io error for non-existent path, got: {err:?}"
    );
}
