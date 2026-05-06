# Plan: daemon architecture, triage, USB workflow

Implements the design in [daemon-sketch.md](../daemon-sketch.md).
Ordered by dependency — later waves depend on earlier ones.

## Wave 1 — Root enable/disable + complete the scan

**Goal:** Get a full scan working on ~, add root state management.

### Issue 1.1: Root enable/disable

Add an `enabled` boolean to `~/.smriti/roots.conf` format. Currently
roots.conf is one path per line. Extend to support a disabled marker:

```
/home/josh
#disabled /mnt/usb-backup
```

Or use a simple TOML-like format:

```
/home/josh
/mnt/usb-backup  disabled
```

Commands:
- `smriti roots enable <path>`
- `smriti roots disable <path>`
- `smriti roots list` — shows enabled/disabled status + last scan time

Scanner skips disabled roots. All existing index data preserved.

Files: `src/roots.rs`, `src/scanner.rs` (skip disabled), `src/main.rs` (CLI)

### Issue 1.2: Complete full scan on ~

From handoff: clean up stale WAL/SHM, run full scan, benchmark parallel
hashing. This is prerequisite for everything else — triage needs data.

## Wave 2 — Triage

**Goal:** `smriti triage` command with editor-based UX.

### Issue 2.1: Triage analysis engine

Query the index to produce recommendations. Heuristics (in priority order):

1. Known regenerable dirs (target/, node_modules/, .cache/, etc.)
   Detect by: directory name + build manifest in parent
2. Git-recoverable dirs (has .git/ with remote)
3. Large homogeneous dirs (>90% same MIME family, >1GB)
4. XDG cache/temp paths
5. Content duplicates (same blake3 hash, different paths)

Output: a `Vec<Recommendation>` with path, suggested action, reason, size.

Files: new `src/triage.rs`

### Issue 2.2: Triage CLI with $EDITOR flow

`smriti triage` runs analysis, writes a tempfile in the triage format
(see daemon-sketch.md), opens `$EDITOR`, reads back the edited file,
applies changes to `.smritiignore`.

The format is a simple text table with ACTION, PATH, SIZE, REASON columns.
Lines starting with `#` are comments (headers, instructions). Parser reads
non-comment lines, splits on whitespace.

"Apply" means: append path patterns to the appropriate `.smritiignore`
section (`[catalog]`, `[ignored]`, or root section). `keep` is a no-op.

Files: `src/triage.rs` (format + parse + apply), `src/main.rs` (CLI)

### Issue 2.3: Triage via MCP

Expose triage analysis as an MCP tool (read-only — returns the
recommendations as structured data). The agent can present them however
it wants. No editor flow over MCP — that's CLI-only.

Files: `src/mcp.rs`

## Wave 3 — USB / backup audit

**Goal:** Compare roots to identify redundant, unique, and stale files.

### Issue 3.1: backup-audit command

`smriti backup-audit <root>` compares all files under `<root>` against
all other enabled roots. Classification:

- **Redundant**: same content_hash exists under another root
- **Unique**: content_hash only exists under this root
- **Stale**: same path relative to root exists elsewhere with different
  hash and newer mtime

This is pure SQL over the existing `documents` + `paths` tables. Join
paths on content_hash, group by root.

Output: same editor-based format as triage, but actions are
`delete` / `keep` / `skip`. `delete` is dangerous — defer actual
deletion to a future version. For now, `delete` just prints the paths
to stdout for the user to act on manually.

Files: new `src/backup.rs`, `src/main.rs` (CLI)

### Issue 3.2: backup-audit via MCP

Same as triage — expose as read-only MCP tool.

## Wave 4 — Streamable HTTP transport

**Goal:** Replace stdio MCP with streamable HTTP.

### Issue 4.1: HTTP transport for MCP server

Switch `smriti serve` (currently `smriti daemon` over stdio) to streamable
HTTP. The `rmcp` crate supports this — check current rmcp version for
streamable HTTP server support.

```
smriti serve --port 7333
```

Bind to localhost by default. Optional `--host` flag. Keep stdio as a
fallback for testing (`smriti serve --stdio`).

Configuration for mcpjungle changes from a Unix socket to an HTTP endpoint.

Files: `src/daemon.rs` → rename to `src/serve.rs`, `src/main.rs`

## Wave 5 — Filesystem watcher

**Goal:** `smriti watch` for incremental index updates.

### Issue 5.1: Watcher core

Add `notify` crate dependency (cross-platform filesystem watcher, uses
inotify on Linux).

Core loop:
1. On startup: walk all enabled roots, register recursive watches
2. Receive events → debounce per-path (500ms window via tokio timer)
3. Process batch: classify → hash → DB upsert → emit event
4. On root enable/disable: add/remove watches dynamically

Move detection: `notify` exposes rename events with cookie matching on
Linux. Emit `Moved` directly instead of the two-phase finalize heuristic.

Error handling: if inotify watch limit is hit, log a clear error with
the sysctl command to fix it. Continue watching roots that fit.

Files: new `src/watch.rs`, `src/main.rs` (CLI)

### Issue 5.2: Periodic full scan as safety net

The watcher schedules a lightweight full scan at a configurable interval
(default: daily, env: `SMRITI_WATCH_FULL_SCAN_INTERVAL`). Uses the existing
scanner pipeline — mtime short-circuit makes it fast when nothing changed.

This catches events missed due to inotify queue overflow, races during
startup, or filesystem changes that bypass inotify (e.g., NFS).

### Issue 5.3: Scan request coordination

MCP server needs to trigger scans. Add a `scan_requests` table:

```sql
CREATE TABLE scan_requests (
  id INTEGER PRIMARY KEY,
  requested_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  root TEXT,  -- NULL = all roots
  status TEXT NOT NULL DEFAULT 'pending',  -- pending, running, complete
  completed_at TEXT
);
```

Watcher polls this table (every few seconds) and runs requested scans.
MCP `scan` tool inserts a row and can poll for completion.

## Wave 6 — Systemd services

### Issue 6.1: Service files + install command

`smriti install-services` generates and installs systemd user service files
for both `smriti-watch` and `smriti-serve`. Enables and starts them.

`smriti uninstall-services` stops, disables, and removes them.

Include the sysctl recommendation for inotify watch limit in the install
output.

## Dependencies between waves

```
Wave 1 (roots + scan)
  ↓
Wave 2 (triage) ← needs scan data
  ↓
Wave 3 (backup-audit) ← needs triage patterns, root enable/disable
  
Wave 4 (HTTP transport) ← independent of 2/3, but needs roots
  ↓
Wave 5 (watcher) ← needs HTTP transport for coordination
  ↓
Wave 6 (systemd) ← needs both serve + watch
```

Waves 2 and 4 can run in parallel. Wave 3 depends on 1+2.
Wave 5 depends on 4. Wave 6 depends on 5.
