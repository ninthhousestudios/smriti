# Scanner refactor: batched commits with scan generations

Status: proposed
Owner: josh
Drafted: 2026-04-27

## Problem

The current `scanner::scan` wraps the entire scan cycle in a single transaction
(`src/scanner.rs:459` to `src/scanner.rs:660`). For a /home/josh-sized root
(~50k files, ~100-200GB) this produces:

- ~30 minutes of wall time with no DB visibility — the main `index.db` cannot
  grow until the final `tx.commit()`.
- WAL growth past 300MB before commit. Yesterday's SIGBUS in `walFindFrame`
  was caused by exactly this WAL pressure.
- ~780MB RSS because every `CurrentEntry`, `Event`, and catalog row is
  buffered in `Vec`s until commit.
- All-or-nothing failure semantics: a single panic anywhere in the file
  loop (e.g., today's UTF-8 boundary panic at `scanner.rs:491`) discards the
  entire scan. Two real bugs in two days have hit this design.

The atomicity requirement that motivated the monolithic transaction is the
"mark all paths disappeared, then un-disappear what we still see" logic at
`scanner.rs:545-548`. If a partial scan committed under that scheme, files in
unwalked roots would be falsely marked deleted.

## Goal

Replace the disappear/un-disappear scheme with a **scan-generation** pattern.
Commit per-batch during the walk; compute disappearances in a small final
transaction only on success. Bound WAL and RSS regardless of disk size. Make
progress observable.

## Design

### Schema changes (migration `0002_scan_generations.sql`)

```sql
-- Each scan invocation gets a row. Status transitions running -> complete | failed.
CREATE TABLE IF NOT EXISTS scan_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at  TIMESTAMP NOT NULL,
    finished_at TIMESTAMP,
    status      TEXT NOT NULL CHECK (status IN ('running', 'complete', 'failed')),
    files_seen  INTEGER NOT NULL DEFAULT 0,
    error       TEXT
);

-- Stamp every active paths row with the scan that last observed it.
ALTER TABLE paths ADD COLUMN last_seen_scan INTEGER
    REFERENCES scan_runs(id);

CREATE INDEX IF NOT EXISTS idx_paths_last_seen ON paths(last_seen_scan)
    WHERE disappeared IS NULL;
```

Backfill on migration: `UPDATE paths SET last_seen_scan = 0 WHERE last_seen_scan IS NULL`
(scan_id 0 is a sentinel meaning "pre-generation-tracking"). The first new scan
treats anything with `last_seen_scan < scan_id` AND `disappeared IS NULL` as
"prev state to diff against," same as today.

### Scan flow

1. **Begin** (small txn, autocommit): insert `scan_runs (status='running')`,
   capture `scan_id`. Log `info!("scan {scan_id} started")`.

2. **Walk + write in batches.** Walk roots as today, but accumulate into a
   bounded buffer (default `batch_size = 500`). When the buffer fills:
   - Open a transaction.
   - For each entry: upsert `documents` (current code in `scanner.rs:462-541`),
     UPSERT into `paths` setting `last_seen_scan = scan_id` and clearing
     `disappeared = NULL`, insert non-Deleted `events` for this batch.
   - Update `scan_runs.files_seen += batch.len()`.
   - Commit. Log `debug!("batch committed: {seen}/{?total} files")`.
   - Drop the buffer.

   Memory stays flat at O(batch_size). WAL drains every batch via SQLite's
   default auto-checkpoint (1000 pages / ~4MB).

3. **Finalize** (single small txn after walk completes successfully):
   - `UPDATE paths SET disappeared = now WHERE disappeared IS NULL AND last_seen_scan < scan_id` — the rows we never re-stamped this generation.
   - For each newly-disappeared row: `INSERT INTO events (event_type='deleted', ...)`.
   - Insert `snapshots` row.
   - `UPDATE scan_runs SET finished_at = now, status = 'complete' WHERE id = scan_id`.
   - Commit.

4. **Failure path.** Any error during step 2 leaves `scan_runs.status =
   'running'` (or we set `'failed'` with the error message). Crucially, the
   disappear-pass in step 3 never runs on failure, so partial scans do **not**
   produce false Deleted events. Re-running smriti scan starts a new scan_id;
   files committed under the failed scan are still in `paths` with their old
   `last_seen_scan`, so they get re-stamped on next success without any
   special recovery code.

### Diff logic relocation

Today's diff happens in memory in steps 4–5 of `scanner::scan` against
`prev_paths` and `prev_hash_to_paths` HashMaps. With per-batch commits we
must either:

(a) **Keep prev_paths / prev_hash_to_paths in memory** (loaded once at scan
    start, used by all batches). For 50k files this is a few MB — fine. This
    is the simplest port of existing logic. **Recommended.**

(b) Query the DB per-batch for diff inputs. Cleaner but adds N small queries.
    Defer.

Move/copy detection currently uses `current_hash_to_paths` built across all
roots — that map needs all current entries to be visible. Two options:
- Keep building the full current_hash_to_paths in memory (same cost as
  prev_paths — bounded at ~50k entries with paths). Emit Created/Updated
  events per-batch but defer Move/Copy events to the finalize phase, scanning
  `events WHERE scan_id = ?` to upgrade Created→Moved where appropriate.
- Or: accept slightly weaker move detection (only detect moves whose
  source+target appear in the same batch). I would not pick this; move
  detection is a feature.

**Recommended:** keep `current_hash_to_paths` in memory across batches; emit
provisional Created events during batches; upgrade to Move/Copy in the
finalize transaction by SQL `UPDATE events ...` keyed by `scan_id`. Add
`scan_id` column to events for this.

### Config

Add to `Config`:
- `scan_batch_size: usize` (default 500, env `SMRITI_SCAN_BATCH_SIZE`)

### Observability

Replace the silent walk with periodic info logs:
- `info!("scan {scan_id} started, batch_size={N}")`
- `info!("walked {N} files in {root}")` per root
- `info!("batch {n} committed: {seen} files")` every K batches (K=10 by default)
- `info!("scan {scan_id} complete: {seen} files, {events} events, {ms}ms")`

`smriti health` already reads `last_scan` from snapshots; will continue to
work since snapshots is written in the finalize txn.

Add a `smriti scan --watch` or `smriti scan-status` command that polls
`scan_runs WHERE status = 'running'` and prints `files_seen / wall_time`.
Nice-to-have, not blocker.
Yes add it

## Migration & rollout

1. **Land migration 0002** with the schema additions above. Verify
   `run_migrations` is idempotent over both fresh and existing DBs.
2. **Implement the new scan path behind a feature gate** initially — env var
   `SMRITI_SCAN_BATCHED=1`. Old code stays as fallback for one release.
3. **Test on /home/josh** end-to-end. Compare doc counts and event counts
   against the legacy scan run on a snapshot. They should match within a
   small tolerance (events-only differ for files the scans saw at different
   instants).
4. **Flip the default** to batched in the next release. Delete the legacy
   transaction code.

## Tests

- Unit: in-memory DB, fake walker, assert per-batch commits visible to a
  separate read connection mid-scan.
- Unit: simulated panic in batch 3 of 5 — verify scan_runs is left in
  `running`/`failed`, no Deleted events, paths from batches 1–2 are present
  with `last_seen_scan = scan_id`.
- Unit: re-run after a failed scan — verify files re-stamp cleanly, no
  duplicate Created events.
- Integration: `tests/scan_batched.rs` over a small temp tree of ~5k synthetic
  files. Assert wall-time, peak RSS (via `/proc/self/status`), max WAL size.
- Regression: today's UTF-8 file (a 100KB+ file with a multi-byte char near
  position 102400) — must not panic, must commit successfully.

## Risks

- **Move/copy detection complexity** in finalize. Worth the cost; alternative
  is feature regression.
- **Auto-checkpoint behavior under per-batch commits** — should Just Work
  with default `wal_autocheckpoint = 1000` pages, but verify WAL stays under
  ~10MB during a scan.
- **Concurrent reads (smriti find / mcp)** during a scan see partial state.
  This is already true today (other connections see committed-up-to-now); the
  difference is they'll now see partial scan progress instead of no change.
  Acceptable: search results will simply lag the current scan by one batch.
  Document this in the MCP tool descriptions.

## Out of scope

- Parallelizing the walk or the hash phase. Worth doing later; the hash phase
  is the dominant cost and is embarrassingly parallel. Separate plan.
- Incremental scanning (watch-mode). Separate plan.
- Switching from the `ignore` crate to something parallel. Separate plan.

## Two small fixes to land first (not blocked by this plan)

These are independent of the refactor and should ship immediately so the user
unblocks today.

### Fix 1: UTF-8 boundary panic at `scanner.rs:491`

Current code:
```rust
let max = config.fts_content_max_bytes as usize;
if s.len() > max { s[..max].to_string() } else { s.to_string() }
```

Panics when byte `max` falls inside a multi-byte UTF-8 char. Replace with a
char-boundary-safe truncation:

```rust
let max = config.fts_content_max_bytes as usize;
if s.len() > max {
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
} else {
    s.to_string()
}
```

(Or use `s.char_indices().take_while(|(i, _)| *i <= max).last()`.) Add a unit
test with a string containing a 3-byte char straddling `max`.

### Fix 2: progress log at start of commit phase

In `scanner.rs:459`, before `let tx = conn.transaction()?;`, emit:

```rust
tracing::info!(
    "walk complete: {} files current, {} events, beginning commit",
    current_entries.len(),
    events.len(),
);
```

Without this, the user has no signal between "walk done" and "commit done"
for the 20+ minutes the transaction is writing. Cheap, high-value.

These two go in one PR/commit, then this plan is the next PR.
