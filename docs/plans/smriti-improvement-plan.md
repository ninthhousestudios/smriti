# smriti — improvement plan

Status: implementation plan
Date: 2026-04-30
Source: `docs/smriti-overall-refactor.md` (candidate list)
Companion: `docs/smriti-next-steps.md` (kosha-integration roadmap — separate track)

## scope

Implement the seven candidates from `smriti-overall-refactor.md` as focused PRs. Each wave below is one logical unit of work with a clear verification step. The waves are ordered for dependency safety: quick wins first, then the standalone bugfix, then the larger structural refactors.

This plan does **not** cover the kosha integration items in `smriti-next-steps.md` (event subscription, `smriti_events_since`, README honesty pass). Those land on a separate track. Wave 4 (search split) does, however, leave a natural seam where an `events.rs` module can later host `smriti_events_since`.

## how to read each wave

Each wave has:
- **Goal** — one sentence
- **Files** — primary touch points (line ranges where helpful)
- **Steps** — ordered, each with a verify check
- **Done when** — what makes the PR mergeable
- **Risk** — and what to watch for

Commit at the end of each wave (one PR per wave) per CLAUDE.md commit discipline.

---

## Wave 1: utilities + editor dedup + migration versioning (quick wins)

**One PR.** All three are mechanical, share no code, and together form a low-risk warmup that exercises every layer touched by later waves.

### 1a. Duplicated utilities extraction (#5)

**Files:** `src/main.rs`, `src/triage.rs`, `src/backup.rs`, `src/metadata.rs` → new `src/display.rs`

**Steps:**
1. Create `src/display.rs` with `format_bytes`, `path_display`, `parse_duration_string`. → verify: `cargo build`.
2. Add a unit test per function in `src/display.rs` (these are the first easy unit tests in the project — set the pattern). → verify: `cargo test display`.
3. Replace duplicates in `main.rs`, `triage.rs`, `backup.rs` with `use smriti::display::*`. → verify: `cargo build && cargo test`.
4. Move `detect_mime_type` from `metadata.rs` to a new `src/mime.rs`. → verify: `cargo build`.

**Done when:** No `format_bytes` or `path_display` definition outside `display.rs`. `detect_mime_type` lives in `mime.rs`. All tests green.

**Risk:** Trivial. Pure moves.

### 1b. Editor-workflow deduplication (#6)

**Files:** `src/main.rs:547–637` (`cmd_triage`, `cmd_backup_audit`)

**Steps:**
1. Extract `pub fn edit_in_external_editor(content: &str) -> Result<String>` to `src/display.rs` (or `src/editor.rs` — see decision D1).
2. Replace the inline blocks in both commands with calls to it. → verify: `cargo build`, manual smoke: run `smriti triage` and `smriti backup audit` and confirm $EDITOR opens.

**Done when:** One function, two call sites.

**Risk:** Trivial. Manual smoke is the only verification (no test infra for spawning $EDITOR).

### 1c. Migration version table (#7)

**Files:** `src/db.rs:94–132`, new `migrations/0003_schema_version.sql`

**Steps:**
1. Add `migrations/0003_schema_version.sql` creating `schema_version (version INTEGER PRIMARY KEY)`. → verify: SQL parses (`sqlite3 :memory: < migrations/0003_schema_version.sql`).
2. Rewrite `run_migrations` to:
   - `CREATE TABLE IF NOT EXISTS schema_version`.
   - Read current version (default 0).
   - For each migration N: if `version < N`, apply DDL, `INSERT OR REPLACE INTO schema_version VALUES (N)`.
3. Bootstrap existing DBs: if `schema_version` is empty but `paths.last_seen_scan` column exists, seed `version = 2`. (See decision D2.) → verify: open an existing index.db from a backup, run migrations, confirm no DDL re-runs.
4. Delete the `has_last_seen` probe and the inline DDL fallback. → verify: `cargo test`, plus a new `tests/migrations_test.rs` that runs migrations twice on the same in-memory DB and confirms idempotence.

**Done when:** `run_migrations` is forward-only and version-driven. Adding migration 0004 requires only a new SQL file + a one-line entry in the migration list.

**Risk:** Low. The bootstrap on existing DBs is the only sharp edge — write the test against an actual snapshot of a pre-migration DB if one is available.

---

## Wave 2: scan setup consolidation (#2)

**Goal:** One code path for scan setup. Fix the real bug where MCP scans ignore the user's `~/.smritiignore`.

**Files:** `src/main.rs:178–229`, `src/mcp.rs:117–158`, new `src/scan_setup.rs` (or in `src/scanner.rs` — see decision D3).

**Steps:**
1. Move `load_user_smritiignore` from `main.rs:170–189` into the library (`scan_setup.rs`). → verify: `cargo build`.
2. Add `pub fn prepare_scan(config: &Config, root_override: Option<Vec<PathBuf>>, jobs: Option<usize>) -> Result<(Connection, Config, SectionRules)>`. Move into it: rayon pool init, root resolution (`load_roots`), the empty-roots check, the `db::open` + `checkpoint_wal` calls, and `load_user_smritiignore`. → verify: `cargo build`.
3. Update `cmd_scan` (main.rs) to call `prepare_scan` then `scanner::scan`. → verify: `cargo test`, manual `smriti scan`.
4. Update `smriti_scan` (mcp.rs) to call `prepare_scan` then `scanner::scan`. Drop `SectionRules::empty()` — the user's smritiignore now applies to MCP scans too. → verify: integration test that places a path matched by `~/.smritiignore` and confirms MCP scan skips it (new test in `tests/mcp_scan_ignore_test.rs`).
5. Add a CHANGELOG/handoff note: MCP scans now respect `~/.smritiignore`. This is a behavior change.

**Done when:** Both entry points call `prepare_scan`. New integration test covers the behavior fix.

**Risk:** Low. Behavior change is a fix, not a regression — but call it out in the commit message so anyone who happened to rely on the broken behavior knows.

---

## Wave 3: scanner phase extraction (#1)

**Goal:** Decompose the 600-line `scan_batched` into testable phases.

**Files:** `src/scanner.rs:194–793` (the function), plus surrounding types `WalkEntry`, `CurrentEntry`, `Event`.

**Steps:**
1. Collapse `scan` → `scan_batched` indirection (line 134→195). One function. → verify: `cargo test`.
2. Extract phase 1: `fn walk_roots(roots: &[PathBuf], config: &Config, ignore: &SectionRules) -> Result<Vec<WalkEntry>>`. Move the 140-line walk closure verbatim, then make it a free function. → verify: `cargo test`, plus new `tests/scanner_walk_test.rs` that walks a temp dir with a fixture tree and asserts the entry list.
3. Extract phase 2: `fn load_prev_state(conn: &Connection, scan_id: i64) -> Result<(HashMap<PathBuf, PrevPathEntry>, HashMap<String, String>)>`. → verify: in-memory DB test.
4. Extract phase 3: `fn hash_entries(walk: Vec<WalkEntry>, config: &Config) -> Vec<CurrentEntry>` (parallel rayon pass). → verify: existing scan tests still pass; add a unit test of hash_entries against a known input.
5. Extract phase 4: keep `flush_batch` as is (already a function); ensure it takes inputs explicitly with no closure capture. → verify: `cargo test`.
6. Extract phase 5: `fn finalize(events_in: Vec<Event>, seen_paths: &HashSet<PathBuf>, /*...*/) -> Vec<Event>`. **Return** a new vector instead of mutating in place. This is the tightest coupling — see decision D4. → verify: `cargo test` + new finalize unit test using pre-built event vectors.
7. Extract phase 6: `fn write_catalog(conn: &mut Connection, /*...*/) -> Result<u64>`. → verify: existing catalog tests pass.
8. The new `scan_batched` body is ~30 lines: call each phase, plumb data between them.

**Done when:** `scanner.rs` has six named phase functions. Each has at least one direct unit/integration test that does not require running the whole `scan_batched`.

**Risk:** Medium. The shared mutable maps (`all_events`, `seen_paths`, `prev_paths`, etc.) are the hard part. Mitigation: do this PR on a branch, run the full smriti scan against `~/soft` and diff the resulting `events` table against a baseline before/after. If the event counts and types don't match, a phase boundary leaked state.

**Decision needed:** D5 (sub-module vs. flat file).

---

## Wave 4: search.rs decomposition (#3)

**Goal:** Five responsibilities in one file → one file per responsibility. Fix the N+1.

**Files:** `src/search.rs` (669 lines) → `src/search.rs` + `src/audit.rs` + `src/manifest.rs` + `src/health.rs` + `src/events.rs` (history + future events_since)

**Steps:**
1. Move `audit` + `AuditResult` + `ExtensionStats` + `CatalogEntry` into `src/audit.rs`. Rewrite `audit`'s extension counting as a `GROUP BY` SQL query rather than a Rust HashMap loop. → verify: existing audit integration tests pass; new test asserting extension counts match an explicit fixture.
2. Move `manifest` + `ManifestResult` into `src/manifest.rs`. → verify: existing tests.
3. Move `health` + `HealthResult` + `count_documents` + `freshness_envelope` into `src/health.rs`. → verify: `smriti health` CLI manual smoke.
4. Move `history` + `HistoryEvent` + `HistoryResult` into `src/events.rs`. (Sets up the seam for `smriti_events_since` later — see decision D6.) → verify: existing history tests.
5. In the remaining `search.rs`: merge `search_path` and `search_extension` into `pub fn search_by_pattern(conn, like_pattern, config) -> Result<PathSearchResult>`. Both call sites become thin wrappers (or inline). → verify: existing search-by-extension and search-by-path tests pass.
6. Fix the N+1: rewrite `search_fts` and `search_hybrid` to JOIN `current_path` into the main query rather than calling `current_path(conn, &content_hash)` per result row. → verify: existing find tests pass; add a microbench or counted-query test confirming queries-per-call dropped from O(k) to O(1).
7. Update `mcp.rs::smriti_outline` if it imports from `search.rs` → re-import from new homes. (Bypassing-search issue per `smriti-overall-refactor.md` §secondary findings is deferred to wave 5 — touching it twice is wasteful.)

**Done when:** Each new file has a single responsibility nameable in one sentence. Search N+1 is gone. `cargo test` green.

**Risk:** Low — mostly file-level moves. The N+1 fix is the only real query change; verify hit ordering hasn't shifted (BM25 ranking should be identical when JOIN is on `paths.content_hash`).

---

## Wave 5: store abstraction (#4)

**Goal:** All SQL behind a `Store` struct. Modules become testable against a constructed store rather than a real connection.

**Files:** `src/store.rs` (new), then incrementally migrate `search.rs`, `audit.rs`, `manifest.rs`, `health.rs`, `events.rs`, `scanner.rs`, `triage.rs`, `backup.rs`, `mcp.rs`, `db.rs`.

**Strategy:** This is the biggest refactor. Do it incrementally on a single branch with multiple commits, but ship it as one PR (otherwise half-migrated state is more confusing than the destination).

**Steps:**
1. Create `src/store.rs` with `pub struct Store { conn: Connection }`. Constructor takes `Connection`. Add a `pub fn from_path(path: &Path) -> Result<Store>` and `pub fn open_readonly(path: &Path) -> Result<Store>` mirroring `db::open*`. → verify: `cargo build`.
2. Migrate **read-only search queries first** (lowest risk):
   - `store.search_fts(query, k)` ← from search.rs
   - `store.search_by_pattern(like_pattern)` ← from search.rs
   - `store.search_hybrid(query, k, embedder)` ← from search.rs (see decision D7 for embedder coupling)
   - `store.get_document(content_hash)` ← from search.rs
   - `store.list_history(path, since, until)` ← from events.rs
   → verify: `cargo test` after each migration.
3. Migrate audit/manifest/health: `store.audit(filters)`, `store.manifest(format)`, `store.health()`. → verify: existing integration tests.
4. Migrate scanner writes: `store.load_prev_state(scan_id)`, `store.flush_batch(entries, ...)`, `store.write_catalog(...)`, `store.start_scan_run()`, `store.finish_scan_run(id, status)`. → verify: scan tests, including the `tests/scan_batched_test.rs` regression test.
5. Migrate triage and backup. `backup.rs::analyze` N+1: collapse into a single `store.backup_summary()` query. → verify: backup tests.
6. Migrate `mcp.rs`: every tool method now talks to `Arc<Mutex<Store>>` (or whatever the current shape is) instead of `Arc<Mutex<Connection>>`. The `smriti_outline` inline SQL becomes `store.outline(path)`. → verify: integration tests + manual MCP smoke.
7. After migration: grep for `&Connection` outside `store.rs` and `db.rs`. Should be zero hits in non-test code (tests can still use raw connections for setup). → verify: `rg '&Connection' src/ | grep -v store.rs | grep -v db.rs` returns nothing.
8. Add `tests/store_test.rs` exercising at least one method per category (read, write, scan-write, audit) against an in-memory `Store`. This locks in the testability win.

**Done when:** Zero `&Connection` references in business logic. Every module imports `Store` instead. New store-level tests exist.

**Risk:** Medium-high. Touch every file. Mitigation: the incremental order above means after each step `cargo test` should still pass. Don't move on if it doesn't.

**Decisions needed:** D7 (embedder coupling), D8 (Store signature for write-heavy paths).

---

## verification across the whole plan

After each wave:
- `cargo build && cargo test` green.
- `smriti scan` against a known root produces the same event count as before (record a baseline before Wave 3).
- `smriti find <known query>` returns the same hits in the same order (record a baseline before Wave 4).
- `smriti health` reports the same totals.

After Wave 5:
- A re-scan of an already-scanned tree produces zero new events (idempotence regression).
- `smriti audit` and `smriti manifest` outputs byte-identical to a pre-refactor baseline (pure refactor — output should not change).

## sequencing rationale

```
Wave 1 (quick wins)           — 1 PR, low risk, exercises every layer.
Wave 2 (scan setup + bug)     — 1 PR, fixes a real bug. Independent of other waves.
Wave 3 (scanner phases)       — 1 PR, biggest single-module win. After 2 because both touch scan.
Wave 4 (search split)         — 1 PR, prepares the ground for Wave 5.
Wave 5 (store)                — 1 PR, biggest. Last because it benefits from the prior splits.
```

Total: 5 PRs.

The `smriti-overall-refactor.md` doc suggested 7 PRs (one per candidate). I've batched #5/#6/#7 into Wave 1 because they're all <50-line mechanical changes with no shared surface — splitting them is more review overhead than it saves. If you'd rather see them as separate PRs, that's decision D9.

---

## design decisions you need to make

### D1. Where does `edit_in_external_editor` live?

- **Option A:** `src/display.rs` (joins the other "presentation" helpers).
- **Option B:** `src/editor.rs` (one-purpose module, room to grow if we add `$VISUAL` polish, syntax-aware temp file naming, etc.).
- **Option C:** Inline `pub` in `main.rs` (it's CLI-only; library code never spawns editors).

**My suggestion:** **C**. It's only used by the CLI. Hoisting it to a library module is speculative. Make it `fn edit_in_external_editor` private to `main.rs` and call it from both `cmd_triage` and `cmd_backup_audit`. If the daemon ever needs interactive editing (it shouldn't), revisit.

### D2. How to bootstrap `schema_version` on existing databases?

- **Option A:** Probe for `paths.last_seen_scan` (and any other migration-N marker) once at first run, infer version, write it. Then forward-only.
- **Option B:** Use `IF NOT EXISTS` everywhere for past migrations and assume migration N is "done enough" if the DDL is idempotent. Skip detection.
- **Option C:** Ship a one-shot CLI `smriti migrate --bootstrap` that the user runs manually on existing DBs.

**My suggestion:** **A**. The probe is bounded to a single fallback path that runs once and never again. We already have the probe; we're just generalizing and capping it. B works for additive DDL but fails the moment a migration does something non-idempotent (data backfill, column drops). C puts a manual step in the user's path for no benefit.

### D3. Where does `prepare_scan` live?

- **Option A:** New `src/scan_setup.rs`.
- **Option B:** Top of `src/scanner.rs`.
- **Option C:** `src/lib.rs` (pub re-export from scanner submodule).

**My suggestion:** **B** for now. `scanner.rs` is the natural home. After Wave 3, when scanner.rs becomes a directory module, `prepare_scan` becomes `src/scanner/setup.rs`. Don't pre-create the file structure.

### D4. Finalize phase: return new Vec or mutate in place?

The `smriti-overall-refactor.md` doc says "returns a new `Vec<Event>` rather than mutating in place." This is correct for testability but means allocating a new vector for ~50k events on a full scan.

- **Option A:** Return new Vec (per the doc).
- **Option B:** Take `&mut Vec<Event>` and mutate (cheaper, less testable — same problem as today).
- **Option C:** Return new Vec but only in the test-friendly path; production path uses `Vec::drain` to reuse the allocation.

**My suggestion:** **A**. The cost of one extra vector allocation per scan is negligible compared to the I/O cost of a scan. Don't optimize what hasn't been measured. The testability win is real.

### D5. Scanner: flat file or sub-module?

After Wave 3, `scanner.rs` will still be ~1000 lines but with six named phases. Do we:

- **Option A:** Keep `scanner.rs` as one file with phase functions.
- **Option B:** Convert to a directory module: `src/scanner/{mod.rs, walk.rs, hash.rs, flush.rs, finalize.rs, catalog.rs}`.

**My suggestion:** **A** for Wave 3. The file is readable once the phases are named functions with explicit inputs. Splitting into a directory is a separate cosmetic change — do it later if/when phase files want their own internal helpers. Premature directory structure adds navigation friction.

### D6. Where does `history` live, given `smriti_events_since` is coming?

- **Option A:** `src/events.rs` (new), houses both `history` and the future `events_since`.
- **Option B:** Keep `history` in `search.rs`. Add `events.rs` later when `events_since` lands.

**My suggestion:** **A**. The seam is obvious — both functions read from the `events` table. Doing it now means the `smriti-next-steps.md` work item lands as "add a function to events.rs" rather than "extract events.rs and then add a function." The cost is one extra file move in Wave 4.

### D7. `search_hybrid` and `&mut Embedder` — how does Store handle this?

The existing `search_hybrid(conn, query, k, config, embedder: &mut Embedder)` couples search to embedding internals. After Wave 5, does the embedder live in the store, get passed in, or get isolated?

- **Option A:** `store.search_hybrid(query, k, embedder: &mut Embedder)` — keep the param, embedder ownership stays with the caller.
- **Option B:** Store owns an `Option<Embedder>`. `store.search_hybrid(query, k)` reads the embedder from self. Cleaner API; the Store becomes a god-object holding both DB and ML state.
- **Option C:** Compute the embedding outside the store, pass the resulting `Vec<f32>` in: `store.search_hybrid_with_embedding(query, k, embedding: &[f32])`. Embedder stays out of the store entirely.

**My suggestion:** **C**. The embedder is conceptually separate from data access. Computing the embedding is pure (`embedder.embed(query) -> Vec<f32>`); the store does retrieval given a vector. This keeps Store testable without an embedder fixture and isolates the ONNX/model dependency. The call site adds one line; in return Store stays focused.

### D8. Store and write-heavy scanner paths

The scanner does batch inserts inside a transaction with bind-once-execute-many for performance. A naive `store.insert_path(entry)` per row would regress.

- **Option A:** Expose `store.flush_batch(entries: &[CurrentEntry], scan_id: i64) -> Result<Vec<Event>>` — one method per coarse operation. Hide the prepared-statement reuse inside.
- **Option B:** Expose lower-level `store.begin_tx() -> Tx<'_>` returning a transaction handle with `tx.insert_path(...)`, `tx.commit()`. More flexible, leaks transaction shape.

**My suggestion:** **A**. Coarse methods preserve the perf-critical structure (one prepared stmt, bind in a loop) without exposing transaction internals. The scanner's flush is the only batch hot-path; design for it.

### D9. Wave 1 — one PR or three?

- **Option A:** One PR for utilities + editor + migrations (my plan above).
- **Option B:** Three PRs, one per candidate (the source doc's structure).

**My suggestion:** **A**. They share no code, are <100 lines each, and reviewing three trivial PRs costs more attention than one slightly-larger trivial PR. But this is purely process preference — if you split PRs as a discipline, go with B.

---

## what this plan does not cover

- Kosha integration (`smriti_events_since`, README honesty pass) — separate track per `smriti-next-steps.md`.
- Watcher daemon implementation — separate spec at `docs/daemon-design-spec.md`.
- `mcp.rs::smriti_outline` inline SQL — resolved as part of Wave 5.
- `triage.rs::analyze` decomposition — touched as part of Wave 5 store migration; not a separate wave.
- `db.rs::enable_scan_pragmas` RAII guard — Wave 3 will create a clear scan lifecycle, address opportunistically.

These are intentionally excluded to keep each wave's blast radius bounded.
