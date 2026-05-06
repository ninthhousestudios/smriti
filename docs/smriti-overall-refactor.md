# smriti — overall refactor

Status: candidate list (pre-plan)
Date: 2026-04-29
Context: architecture review of the smriti codebase using the improve-codebase-architecture skill. These are deepening opportunities — refactors that turn shallow modules into deep ones, improving testability and AI-navigability. Each candidate needs a grilling session before implementation.

## how to use this doc

Pick a candidate. Run the grilling loop (walk the design tree, nail down interfaces, identify dependencies). Then implement. Don't batch — each candidate is a focused PR.

## candidates, in suggested order

The ordering reflects dependencies: earlier items unblock or simplify later ones.

### 1. Scanner phase extraction

**Files:** `src/scanner.rs` (lines 194–793)

**Problem:** `scan_batched` is ~600 lines with six numbered phases (walk → load state → hash → batch flush → finalize → catalog). The phases are marked by comment banners but share five mutable maps that bridge 300-line gaps. The walk phase is a 140-line closure. The only unit-testable function in the file is `truncate_to_char_boundary`. Everything else requires a live SQLite connection.

**What to do:**
- Extract each phase into a standalone function with explicit inputs and outputs.
- The handoff types already exist: `WalkEntry`, `CurrentEntry`, `Event`.
- The walk closure becomes `fn walk_roots(roots, config, ignore_rules) -> Result<Vec<WalkEntry>>`.
- The finalize phase's event-type upgrade returns a new `Vec<Event>` rather than mutating in place.
- `scan` → `scan_batched` indirection (line 133→138) is a pass-through — collapse it.

**What it buys:** Each phase testable in isolation. Walk phase testable with a temp directory. Flush testable with in-memory DB. Finalize testable with pre-built event vectors. Locality — a bug in hashing doesn't require understanding finalization.

**Depends on:** Nothing. Can be done first.

**Risk:** Medium. The shared mutable maps are the hard part — teasing apart what each phase needs as input vs. what it produces requires care. The finalize phase's in-place mutation of `all_events` is the tightest coupling.

---

### 2. Scan setup consolidation

**Files:** `src/main.rs` (lines 191–229), `src/mcp.rs` (lines 117–158)

**Problem:** `cmd_scan` in main.rs loads user smritiignore rules. `smriti_scan` in mcp.rs uses `SectionRules::empty()` — a real behavioral divergence. MCP scans index files the user intended to ignore. Both functions also duplicate root-loading and config-override logic.

**What to do:**
- Extract `fn prepare_scan(config, root_override) -> Result<(Connection, Config, SectionRules)>` in library code.
- Both CLI `cmd_scan` and MCP `smriti_scan` call it.
- Rayon thread pool setup moves into the library function too.

**What it buys:** Fixes a real bug. Adding future scan entry points (daemon, watcher, `smriti_events_since` implementation) gets correct behavior automatically. One place to change scan setup.

**Depends on:** Nothing. Can be done independently or after #1.

**Risk:** Low. Straightforward extraction.

---

### 3. search.rs decomposition

**Files:** `src/search.rs` (669 lines, 26 symbols)

**Problem:** Five unrelated responsibilities in one file: search, audit, manifest, health, history. `search_path` and `search_extension` are near-identical. `current_path` called per result row is an N+1 query. `audit` reimplements `GROUP BY` in Rust. `manifest` embeds JSON in the data layer.

**What to do:**
- Split into: `search.rs` (FTS + hybrid + helpers), `audit.rs`, `manifest.rs`, `health.rs`. History can stay with search or get its own file.
- Merge `search_path`/`search_extension` into `search_by_pattern(conn, like_pattern, config)`.
- Fix the N+1: join `current_path` into the main FTS/hybrid query.
- Move `audit`'s extension counting into SQL (`GROUP BY`).

**What it buys:** AI-navigability — "how does search work" → one file. N+1 fix is a direct performance improvement. Audit and manifest become independently modifiable.

**Depends on:** Nothing, but easier after #4 (store abstraction) if doing both.

**Risk:** Low. Mostly file-level moves with a few query optimizations.

---

### 4. Store abstraction

**Files:** All of `search.rs`, `scanner.rs`, `mcp.rs`, `triage.rs`, `backup.rs`, `db.rs`

**Problem:** Raw `&Connection` everywhere. ~50 SQL statements scattered across 6 files. `mcp.rs::smriti_outline` bypasses search with inline SQL. No module is testable without a real database. `backup.rs::analyze` does N+1 queries because SQL is hand-written per-row.

**What to do:**
- Introduce a `Store` struct (not a trait — one adapter = hypothetical seam) that owns the connection.
- Expose domain methods: `store.search_fts(query)`, `store.insert_batch(entries)`, `store.get_events_since(cursor)`, `store.get_document(hash)`, etc.
- All SQL concentrates behind this struct. Callers never see `Connection`.
- `mcp.rs` tools call store methods instead of raw queries.

**What it buys:** Massive locality gain — all SQL in one file, schema changes touch one place. Every module becomes testable against a constructed store. The deletion test confirms depth: removing raw SQL from scanner/search/triage would just push it into every caller. A store earns its keep.

**Depends on:** Easier after #1 (scanner decomposition) and #3 (search split) — those reduce the blast radius of the store migration. But not strictly blocked.

**Risk:** Medium-high. Largest refactor. Touch every file. Worth doing incrementally — start with search queries, then scanner writes, then triage/backup.

---

### 5. Duplicated utilities extraction

**Files:** `src/main.rs`, `src/triage.rs`, `src/backup.rs`, `src/metadata.rs`

**Problem:** Three copies of `format_bytes`. Two copies of `path_display` (tilde contraction). `parse_duration_string` stranded in main.rs. `detect_mime_type` in metadata.rs is a pure lookup unrelated to metadata extraction.

**What to do:**
- Create `src/display.rs` for `format_bytes`, `path_display`, `parse_duration_string`.
- Move `detect_mime_type` to `src/mime.rs` or into display/util.
- Delete the duplicates. Update imports.

**What it buys:** One definition of each utility. Unblocks other modules from importing `backup` or `triage` just for a helper.

**Depends on:** Nothing. Can happen anytime. Good warmup PR.

**Risk:** Trivial. Pure moves + import updates.

---

### 6. Editor-workflow deduplication

**Files:** `src/main.rs` (lines 547–637)

**Problem:** `cmd_triage` and `cmd_backup_audit` have ~20 identical lines: write to temp file, launch `$VISUAL`/`$EDITOR`, read back.

**What to do:**
- Extract `fn edit_in_external_editor(content: &str) -> Result<String>`.
- Both commands call it.

**What it buys:** One place for editor-launch logic. Tiny scope, zero design risk.

**Depends on:** Nothing.

**Risk:** Trivial.

---

### 7. Migration version table

**Files:** `src/db.rs` (lines 94–132)

**Problem:** `run_migrations` probes for a column name (`last_seen_scan`) as a proxy for whether migration 0002 ran. The DDL from `migrations/0002_scan_generations.sql` is partially duplicated inline. A future migration could break the idempotency check.

**What to do:**
- Add a `schema_version` table with an integer version column.
- Each migration checks `version < N`, applies DDL, bumps version.
- Remove the column-probing heuristic.

**What it buys:** Adding migration 0003+ becomes mechanical. Eliminates fragile idempotency logic.

**Depends on:** Nothing. Can happen anytime.

**Risk:** Low. Standard pattern. Only tricky part is bootstrapping the version table for existing databases that have already run migrations 0001 and 0002.

---

## secondary findings (not separate candidates)

These are smaller issues that will be resolved by the candidates above:

- **`db.rs::enable_scan_pragmas` / `restore_default_pragmas`** — no RAII guard for scan-mode settings. Will be addressed naturally when scanner decomposition (#1) creates a clear scan lifecycle.
- **`db.rs::db_file_size`** — filesystem call in a DB module. Moves to utilities (#5) or store (#4).
- **`metadata.rs::parse_heading` / `heading_level`** — redundant `#` scanning. Minor cleanup, not a separate candidate.
- **`triage.rs::analyze` at 168 lines** — classification loop with embedded heuristics. Worth decomposing but not architecturally distinct from the other pipeline concerns in triage. Can be addressed when triage is touched for store migration (#4).
- **`backup.rs::analyze` N+1 queries** — resolved by store abstraction (#4) which would compose the query properly.
- **`mcp.rs::smriti_outline` bypassing search** — resolved by store abstraction (#4) or search decomposition (#3).
- **`search.rs::search_hybrid` leaking `&mut Embedder`** — coupling to embedding internals. Address when search is decomposed (#3).

## suggested sequencing

```
#5 (utilities) ─┐
#6 (editor)  ───┤── quick wins, can be one PR
#7 (migrations) ┘

#2 (scan setup) ──── standalone fix for a real bug

#1 (scanner phases) ── biggest single-module improvement

#3 (search split) ──── decompose before store migration

#4 (store) ──── last, benefits from all prior work
```

Total: ~7 focused PRs over multiple sessions.

## what this doc is not

This is a candidate list, not an implementation plan. Each candidate needs a grilling session to walk the design tree — nail down interfaces, identify edge cases, decide what tests look like. That happens when we pick one up to implement.
