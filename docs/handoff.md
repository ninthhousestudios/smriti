# Handoff — smriti

## Pick up

Mid-crank on the v0.1 implementation plan. Waves 1–3 (Issues 1–5) done and committed. **Resume at Wave 4 — Issue 6 (Search + CLI)**, the first user-visible milestone.

### State of the world

- Repo is now a git repo (`git init` was done this session). Branch: `main`.
- Three wave commits on top of the initial design-docs commit. `git log --oneline` to see.
- `cargo build` clean. `cargo test` = 48 pass, 1 ignored (fuzzy_move_plus_edit, deferred to v0.2).
- sqlite-vec extension loads cleanly (the highest ecosystem risk per the plan was validated in Wave 1: v0.1.9-alpha.3).

### What's done

| Wave | Issue | Commit | Files |
|---|---|---|---|
| 1 | 1 — scaffold + core types | `7ecfee1` | Cargo.toml, lib/main, config, error, envelope, db, roots, migrations/0001_initial.sql, stub modules for later issues |
| 2 | 2 — ignore parser | `0476595` | src/ignore.rs, src/ignore_defaults.txt, tests/ignore_test.rs |
| 2 | 3 — hasher + metadata | `0476595` | src/hasher.rs, src/metadata.rs (inline tests) |
| 2 | 4 — privacy gate | `0476595` | src/privacy.rs, tests/privacy_test.rs |
| 3 | 5 — scanner | (latest) | src/scanner.rs, tests/{scan,move_detection,mtime_shortcircuit}_test.rs |

### What's next

**Wave 4 — Issue 6: Search + CLI (first milestone).**
Files: `src/search.rs`, `src/main.rs` (full impl, replace the stub clap handlers), `tests/integration_test.rs`.
After Issue 6 ships, `smriti init && smriti roots add ~/Documents && smriti scan && smriti audit` should produce a real backup audit. That's the "useful from the CLI" line.

**Wave 5 (parallel after Wave 4) — Issues 7 + 8: MCP server + daemon, Embedding pipeline.**
- Issue 7: `src/mcp.rs`, `src/daemon.rs`, `src/main.rs` (daemon subcommand)
- Issue 8: `src/embedding.rs`, `src/search.rs` (additions for dense + RRF, gated behind `embedding` feature)

### Decisions made this session worth remembering

- **`PrivacyGate.conn` was wrong in the plan.** Connection by-ref per-call is correct; daemon will hold `Arc<Mutex<Connection>>`, CLI uses short-lived conn.
- **`SectionRules` isn't `Clone`** because `ignore::gitignore::Gitignore` isn't `Clone`. Scanner builds fresh `hardened_defaults(root)` per root rather than fighting it. Privacy gate classifies directly against the public `ignored`/`cataloged` `Gitignore` fields — bypassing `IgnoreStack` to avoid ownership transfer.
- **Schema column names diverged from the plan in places** — the migration in `migrations/0001_initial.sql` is the source of truth. Notably `paths` uses `(appeared, disappeared)` timestamps not an `is_current` bool; `documents` has only `first_seen`; `snapshots` has a single `timestamp`. Issue 6 search queries should follow the actual schema.
- **rusqlite `bundled-full` enables `extra_check`** which makes `pragma_update` reject row-returning PRAGMAs. Already worked around in `db.rs` via `pragma_update_and_check(…, |_| Ok(()))`. If Issue 7's daemon needs more PRAGMAs, use the same pattern.
- **Symlinks: skipped with debug log in v0.1.** Plan called this pragmatic; full link-entry recording deferred.
- **Short-circuit re-scan path collision:** the `paths` table has `UNIQUE(content_hash, path, appeared)`. Same-second re-scans of unchanged files would collide on a fresh insert; scanner now un-disappears (NULLs `disappeared`) instead of inserting a new row. See `src/scanner.rs`.

### Blockers / risks for Wave 4–5

- **rmcp `serve()` ownership** for the daemon — plan flags this as needing early verification. Worth confirming whether `serve()` moves `self` before designing the Unix-socket per-connection spawn pattern.
- **sqlite-vec dense queries from rusqlite** — extension loads, but Issue 8 will be the first time we actually run `vec0` ANN queries. Validate with a tiny smoke insert+query before wiring scanner storage.
- **`src/main.rs` is a hot file** across Issues 6 and 7. Wave 5 must branch from post-Issue-6 SHA (already set up — Issue 6 commits before Issue 7 starts).

### How to resume

```bash
cd /home/josh/soft/smriti
git log --oneline -5    # confirm where we are
cargo test              # baseline: 48 pass, 1 ignored
# Then continue /crank — Wave 4 is Issue 6 alone, Wave 5 spawns Issues 7+8 in parallel.
```

TaskList state at pause: Issue 6 pending (was blocked-by Issue 5, now unblocked), Issues 7+8 pending blocked-by Issue 6.

### Open threads

- Decision: do we want full symlink recording in v0.1, or accept the v0.2 deferral as committed?
- Decision: is the `fuzzy_move_plus_edit` test worth implementing now, or is `Deleted+Created` for that case acceptable?
- Sangha advisory lock for daemon scans (mentioned in plan §7) — Sangha MCP wasn't available this session; check at Wave 5 start.

### Transcript

`/home/josh/.claude/projects/-home-josh-soft-smriti/edf9be41-c7c0-48fc-a3de-ade3fb4d2355.jsonl`

### Note on Chitta

Chitta MCP was not available in this session — `mcp__chittars__*` tools weren't surfaced. Decisions and observations above were not stored to long-term memory. If you want them in Chitta, ask in a session where Chitta is up and reference this handoff.
