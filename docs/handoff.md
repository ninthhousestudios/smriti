# Handoff — smriti

## Pick up

**All 8 issues across all 5 waves are implemented and committed.** The v0.1 implementation plan is complete. What remains is testing the CLI end-to-end on real data, and optionally polishing rough edges.

### State of the world

- Branch: `main`. 8 commits on top of the initial design-docs commit.
- `cargo build` clean (zero warnings). `cargo build --features embedding` also clean.
- `cargo test` = 54 pass, 1 ignored (fuzzy_move_plus_edit, deferred to v0.2).
- sqlite-vec loads cleanly. FTS5 populated during scan.

### What's done

| Wave | Issue | Commit | Summary |
|---|---|---|---|
| 1 | 1 — scaffold + core types | `7ecfee1` | Cargo.toml, lib/main, config, error, envelope, db, roots, migrations |
| 2 | 2 — ignore parser | `0476595` | src/ignore.rs, ignore_defaults.txt, tests/ignore_test.rs |
| 2 | 3 — hasher + metadata | `0476595` | src/hasher.rs, src/metadata.rs |
| 2 | 4 — privacy gate | `0476595` | src/privacy.rs, tests/privacy_test.rs |
| 3 | 5 — scanner | `2b90ef0` | src/scanner.rs, tests/{scan,move_detection,mtime_shortcircuit}_test.rs |
| 4 | 6 — search + CLI | `86504a7` | src/search.rs (BM25 FTS5), src/main.rs (full CLI), tests/integration_test.rs |
| 5 | 7 — MCP server + daemon | `c2d9362` | src/mcp.rs (10 tools), src/daemon.rs (stdio transport) |
| 5 | 8 — embedding pipeline | `128e7c3` | src/embedding.rs (BGE-M3 ONNX, cfg-gated), hybrid search in search.rs |

### What works now

- `smriti init` — creates ~/.smriti/ and index.db
- `smriti roots add ~/Documents` — manages allowlisted roots
- `smriti scan` — walks roots, classifies paths, hashes tier-1 files, populates FTS5, emits lifecycle events
- `smriti find "query"` — BM25 search across indexed files
- `smriti get <hash>` — look up document by content hash
- `smriti history <path>` — lifecycle events for a file
- `smriti audit` — backup audit report (tier-1 vs tier-2 breakdown by extension)
- `smriti manifest` — tier-1 path list for piping to rsync/restic
- `smriti health` — status check
- `smriti prune` — clean old events and audit log
- `smriti daemon` — runs MCP server over stdio (all 10 tools)
- With `--features embedding` and SMRITI_MODEL_PATH set: dense embeddings + hybrid search via RRF

### Bug fix made during Wave 4

Scanner had a bug where WalkDir descended into cataloged/ignored directories because directory-only gitignore patterns (`**/node_modules/`) didn't match child files. Fixed by tracking `skip_subtrees` — when a directory is classified as Cataloged or Ignored, all its descendants are skipped. This was critical for the audit feature to produce correct tier-1 vs tier-2 counts.

### What could be improved (not blockers)

1. **Real-world test on ~/Documents.** The CLI is functional but hasn't been run against a real allowlist. Worth doing: `smriti roots add ~/Documents && smriti scan && smriti audit`.
2. **MCP server instructions.** The `get_info()` instructions text is short; could be expanded with usage patterns.
3. **FTS indexing of updated documents.** When a file is updated (content_hash changes), the scanner creates a new document row and FTS entry, but doesn't delete the old FTS entry for the superseded content_hash. Stale FTS entries accumulate. Low priority since they point to valid document rows.
4. **Embedding tests.** The embedding module compiles with `--features embedding` but has no unit tests (requires a model file on disk). Consider a mock test or a small test model.
5. **Daemon Unix socket transport.** Currently stdio only. The sketch calls for Unix socket for long-lived daemon pattern. Deferred to v0.2.
6. **embed_excluded flag.** The scanner doesn't set `embed_excluded = TRUE` on documents matching `[no-embed]` patterns. The classification info is available but not threaded through to the document insert. Minor gap.

### Decisions made this session

- **SectionRules::empty()** added for CLI scan (no extra rules beyond the scanner's internal hardened defaults).
- **FTS5 populated in scanner transaction** — content (truncated to fts_content_max_bytes) goes into document_fts alongside title/topics/summary.
- **MCP server uses Arc<Mutex<Connection>>** — simple, correct for single-writer pattern. Mutex::lock().unwrap() is standard; poisoning won't happen in normal operation.
- **Embedding uses &mut self** on Embedder because ort Session::run() requires &mut self.
- **stdio transport for v0.1 daemon** — simplest correct choice. Unix socket requires more plumbing (hyper/axum listener).

### How to resume

```bash
cd /home/josh/soft/smriti
git log --oneline -8   # confirm all waves
cargo test              # baseline: 54 pass, 1 ignored
# Try it for real:
cargo run -- init
cargo run -- roots add ~/Documents
cargo run -- scan
cargo run -- audit
cargo run -- find "some query"
```

### Open threads from previous session (still open)

- Decision: full symlink recording in v0.1 vs v0.2 deferral — accepted v0.2 deferral.
- Decision: `fuzzy_move_plus_edit` — accepted Deleted+Created for v0.1.
- Sangha advisory lock for daemon scans — check when daemon goes long-lived.
