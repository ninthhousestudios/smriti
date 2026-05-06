# smriti daemon architecture sketch

Two long-lived processes sharing one SQLite database.

## Processes

### smriti-serve (MCP server)

Streamable HTTP transport. Answers queries, serves search, exposes tools to
agents. Stateless beyond the DB — can restart without losing anything.

```
smriti serve --port 7333
```

Responsibilities:
- Search (FTS, hybrid, semantic)
- File reads (privacy-gated)
- Health, audit, manifest
- Trigger on-demand scans (writes a request to DB or signals the watcher)
- Triage analysis (read-only over scan data)

Transport: Streamable HTTP (like chitta-rs). Not stdio — smriti is a
system-level service, not per-session. Not SSE — streamable HTTP is the
current MCP standard and simpler to proxy.

### smriti-watch (filesystem watcher)

Long-lived background process. Monitors roots for changes, keeps the index
fresh. Stays running even when no MCP clients are connected.

```
smriti watch
```

Responsibilities:
- Register inotify watches on all enabled roots (recursive)
- Debounce events (500ms–1s window per path)
- Incremental pipeline: classify → hash → DB upsert → emit event
- Periodic full scan as consistency check (configurable, default daily)
- Respect root enable/disable state

#### inotify details

Events watched: `IN_CLOSE_WRITE`, `IN_CREATE`, `IN_DELETE`, `IN_MOVED_FROM`,
`IN_MOVED_TO`, `IN_DELETE_SELF`.

Recursive setup: walk each root on startup, add a watch on every directory.
New directories get watches dynamically. Deleted directories are cleaned up
by the kernel.

**Watch limits:** default `fs.inotify.max_user_watches` is 8192 on most
systems. A home directory can have 50k+ subdirectories. Options:

1. Recommend bumping sysctl (VS Code, JetBrains do this too)
2. Budget watches: if limit is hit, fall back to polling for overflow roots
3. Future: fanotify with `FAN_REPORT_FID` (mount-level, no per-dir limit,
   needs `CAP_DAC_READ_SEARCH`)

Start with option 1 + a clear error message. Option 2 as graceful
degradation. Option 3 as a later upgrade.

**Debounce:** a `cargo build` or `npm install` touches thousands of files
in seconds. Coalesce events per-path over a window. Process the batch after
the window closes. Use a hashmap of path → latest event, flushed by a timer.

#### Incremental pipeline

For each debounced event:

```
path changed
  → check ignore stack (Ignored? Cataloged? Indexed?)
  → if Indexed: hash file, upsert document + path, emit event
  → if Cataloged: recalculate dir size/count, upsert catalog
  → if Ignored: skip
  → if Deleted: mark path disappeared, emit Deleted event
```

Move detection: `IN_MOVED_FROM` + `IN_MOVED_TO` with matching cookie →
emit `Moved` event directly, no need for the finalize-phase heuristic.

### Shared state: SQLite

Both processes access `~/.smriti/index.db` in WAL mode. SQLite handles
concurrent readers well. For writes:

- The watcher is the primary writer (incremental updates)
- The MCP server is primarily a reader
- On-demand full scans (triggered via MCP) could either:
  (a) Signal the watcher to run a full scan
  (b) Run in-process on the MCP server with a write lock

Option (a) is cleaner — the watcher owns all write paths.

Communication between processes: simple signals or a tiny control channel.
Options:
- Unix signal (SIGUSR1 = "run full scan now")
- A `scan_requests` table in the DB (MCP writes a row, watcher polls it)
- A Unix socket control channel

The DB table approach is simplest and auditable.

## Systemd integration

```ini
# ~/.config/systemd/user/smriti-watch.service
[Unit]
Description=smriti filesystem watcher

[Service]
ExecStart=%h/.cargo/bin/smriti watch
Restart=always

[Install]
WantedBy=default.target
```

```ini
# ~/.config/systemd/user/smriti-serve.service
[Unit]
Description=smriti MCP server
After=smriti-watch.service

[Service]
ExecStart=%h/.cargo/bin/smriti serve --port 7333
Restart=always

[Install]
WantedBy=default.target
```

The watcher has no dependency on the server. The server can come and go.

## Triage UX

`smriti triage` analyzes the index and opens recommendations in `$EDITOR`.
The user edits the action column, saves, and smriti applies the changes.

### Flow

```
$ smriti triage
Analyzing index... 47,231 files, 312 GB across 3 roots.
Opening recommendations in $EDITOR...
```

Editor opens a file like:

```
# smriti triage — 2026-04-28
# Edit the action for each recommendation. Save and close to apply.
#
# Actions:
#   catalog   — move to tier 2 (size+count only, no content indexing)
#   ignore    — stop tracking entirely
#   keep      — leave as tier 1 (no change)
#   (delete)  — delete the line to skip this recommendation
#
# The REASON column explains why smriti flagged this.
# Recommendations are sorted by reclaimable index space.

# ACTION    PATH                                    SIZE        REASON
catalog     ~/code/bigproject/target/                12.3 GB     cargo build output
catalog     ~/code/webapp/node_modules/              4.1 GB      npm dependency cache
catalog     ~/.cache/                                8.7 GB      XDG cache dir
keep        ~/Music/                                 89.2 GB     large but not regenerable
keep        ~/Photos/raw/                            43.1 GB     large but not regenerable

# DUPLICATES — same content hash at multiple paths
# Action applies to the SECOND path (first is kept).
#
# ACTION    PATH                                    SIZE        DUPLICATE OF
keep        ~/Desktop/report-v2.pdf                 14 MB       ~/Documents/report-v2.pdf
keep        ~/Downloads/slides.pptx                 8 MB        ~/work/presentations/slides.pptx
```

User changes actions, saves, closes editor. smriti reads the file back:

```
Applied 3 changes:
  catalog  ~/code/bigproject/target/     (added to .smritiignore [catalog])
  catalog  ~/code/webapp/node_modules/   (added to .smritiignore [catalog])
  catalog  ~/.cache/                     (added to .smritiignore [catalog])
Skipped 4 items (kept or deleted).
Next scan will reclassify affected paths.
```

### What "apply" does

- For `catalog` and `ignore` actions: appends the path pattern to the
  appropriate section of `~/.smritiignore` (or the root-level one).
- For `keep`: no-op.
- For duplicates with `delete` action: future feature — actually remove
  the file. Not in v1; too dangerous without a confirmation step.

### Heuristics for recommendations

Sorted by confidence, roughly:

1. **Known regenerable patterns**: `target/`, `node_modules/`, `.cache/`,
   `__pycache__/`, `build/`, `dist/`, `.gradle/`, `.m2/repository/` — match
   by name + presence of build manifest in parent.
2. **Git-recoverable**: directory is a git repo with a clean remote. All
   tracked files are recoverable. (Check via `git remote -v` + `git status`
   during scan or triage.)
3. **Large homogeneous directories**: >90% of files share a MIME type family
   (audio/*, video/*, image/*) and total size > threshold. Flag for review,
   default action `keep`.
4. **XDG cache/temp**: `$XDG_CACHE_HOME`, `/tmp` patterns, `~/.local/share/Trash`.
5. **Content duplicates**: same BLAKE3 hash at different paths. Flag the
   less-canonical location (heuristic: deeper path, or in Downloads/Desktop).

## USB / removable root workflow

### Root states

Roots gain an `enabled` flag (default true):

```
$ smriti roots list
  /home/josh          enabled   last scanned 2m ago
  /mnt/usb-backup     disabled  last scanned 3d ago
```

```
$ smriti roots enable /mnt/usb-backup
$ smriti roots disable /mnt/usb-backup
```

When disabled:
- Scanner skips the root entirely
- Watcher removes its inotify watches
- All existing path/document data is preserved
- Search still returns results from the root (with a staleness note)

### Backup audit

Once both live roots and the USB are scanned:

```
$ smriti backup-audit /mnt/usb-backup
Comparing /mnt/usb-backup against live roots...

  REDUNDANT (safe to delete from USB — identical content exists on live roots):
    /mnt/usb-backup/Documents/taxes-2024.pdf
      = /home/josh/Documents/taxes-2024.pdf  (same hash)
    ... (1,247 more files, 34.2 GB)

  USB-ONLY (exists only on USB — keep or decide):
    /mnt/usb-backup/old-projects/thesis-draft-3.tex  (420 KB)
    ... (89 files, 1.2 GB)

  STALE (same path, different content — USB copy is older):
    /mnt/usb-backup/Documents/budget.xlsx
      live: 2026-03-15  usb: 2025-11-02
    ... (23 files)

  Summary: 1,359 files on USB.
    1,247 redundant (34.2 GB reclaimable)
    89 unique to USB
    23 stale copies
```

Same editor-based UX as triage for acting on results (though "delete from
USB" needs careful confirmation).

## Alternatives considered

### Single process (MCP + watcher combined)

Pros: shared DB connection pool, no IPC needed, simpler deployment.
Cons: MCP restart kills the watcher. Watcher crash kills search.
**Rejected** — the failure domains should be independent.

### Watcher as a library inside MCP, spawned as a background task

A middle ground: one binary, one process, but the watcher runs as a tokio
task. The MCP server owns its lifecycle.

Pros: single binary, shared connection pool, no IPC.
Cons: still coupled — if the MCP server process dies, the watcher dies.
Could be acceptable if the systemd service restarts quickly. Simpler than
two services.

**Worth trying** if two-service coordination proves annoying in practice.
Start with two processes; collapse to one if the overhead isn't justified.

### fanotify instead of inotify

fanotify operates at the mount/filesystem level — one mark covers an entire
mount, no per-directory watches, no watch limit problem. Needs
`CAP_DAC_READ_SEARCH` (settable via systemd `AmbientCapabilities`).

Pros: no watch limit, lower kernel memory, catches all events.
Cons: requires capabilities, more complex event parsing, Linux-only
(inotify is also Linux-only but better documented in the Rust ecosystem).

**Worth trying** after inotify works. The `notify` crate supports fanotify
on Linux via feature flag. Could be a config option: `SMRITI_WATCH_BACKEND=inotify|fanotify`.

### Event sourcing / write-ahead log between processes

Instead of both processes hitting SQLite directly: the watcher writes events
to a WAL-like journal, the MCP server tails it and applies to its read
replica.

**Rejected** — over-engineered for this scale. SQLite WAL mode with
`busy_timeout` handles the concurrency fine. Only revisit if write
contention becomes measurable.
