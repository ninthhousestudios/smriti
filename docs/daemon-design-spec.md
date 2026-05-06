# Watcher daemon — design spec

`smriti-watch` is a long-lived background process that keeps the smriti
index fresh by reacting to filesystem events. This document is the
canonical design; it supersedes [archived/daemon-sketch.md](archived/daemon-sketch.md).

## Status

Spec. Not yet implemented. Depends on the scanner decomposition described
in [smriti-overall-refactor.md](smriti-overall-refactor.md) — the watcher
and the existing batch scanner share a per-path core, which must be
extracted before this work begins.

## Position in the system

```
┌──────────────────────────────────────────────────────────┐
│                                                          │
│   smriti-serve (MCP, HTTP)        smriti-watch           │
│   ────────────────────────        ────────────           │
│   - all reads                     - inotify              │
│   - returns freshness envelope    - per-path core        │
│   - writes scan_requests rows     - full scans           │
│   - writes audit.db               - drains scan_requests │
│                                   - heartbeat            │
│            │                            │                │
│            └──────────┬─────────────────┘                │
│                       │                                  │
│            ┌──────────┴───────────┐                      │
│            │ ~/.smriti/index.db   │  ← watcher writes    │
│            │   (WAL mode)         │    serve reads       │
│            └──────────────────────┘                      │
│            ┌──────────────────────┐                      │
│            │ ~/.smriti/audit.db   │  ← serve writes      │
│            │ (read_audit)         │                      │
│            └──────────────────────┘                      │
│            ┌──────────────────────┐                      │
│            │ ~/.smriti/writer.lock│  ← watcher holds     │
│            └──────────────────────┘                      │
│                                                          │
└──────────────────────────────────────────────────────────┘
```

`smriti-serve` answers MCP queries over HTTP. It never writes to
`index.db`. `smriti-watch` is the sole writer.

## Core invariants

1. **Single writer.** Exactly one process writes to `index.db` at a time.
   Enforced by an advisory file lock at `~/.smriti/writer.lock`. The
   watcher holds it for its lifetime; any other writer (e.g. a CLI scan)
   must acquire it and refuse if held.
2. **Per-path atomicity.** Each path's state change commits in a single
   SQLite transaction. Multi-path batching, if used, must remain safe to
   abort partway. Crash recovery relies on this.
3. **Idempotent per-path core.** Running the per-path pipeline twice on
   the same path produces the same DB state. No mutexes are used to
   serialise watcher events against periodic-scan writes; correctness
   comes from idempotency.
4. **DB is the only IPC.** No sockets, no signals between watcher and
   serve. Coordination is via SQLite tables (`scan_requests`,
   `watcher_heartbeat`) and a watch on `~/.smriti/roots`.

## Process model

### smriti-watch

Started by systemd as a user unit (see [Systemd integration](#systemd-integration)).
Long-lived. `Restart=always`. Has `RUST_LOG`-controlled logging to stderr,
captured by journald.

Internally it runs a tokio runtime hosting:

- An **inotify task** receiving raw kernel events into a debounce buffer.
- A **debounce flusher task** firing per-path timers and dispatching
  flushed events through the per-path core.
- A **scan-driver task** running startup full scan, periodic safety-net
  scans, and any scans pulled from `scan_requests`.
- A **heartbeat task** writing `watcher_heartbeat` every 5s.
- A **roots-config task** watching `~/.smriti/roots` and reconciling the
  active watch set when the file changes.

All tasks share one writer SQLite connection, serialised through a
mutex (or via a single-threaded tokio runtime — implementation choice).

### smriti-serve

Existing MCP server, mode-shifted to HTTP transport (Wave 4 of the plan).
Read-only against `index.db`. Writes only to `audit.db` (`read_audit`)
and `scan_requests` in `index.db`. No other writes.

The single-writer rule applies to `index.db` only. `scan_requests` is on
`index.db` and is therefore an exception that the watcher polls — but
the rule we enforce in code is "serve never writes any other table."
Concretely: serve's connection has the same advisory-write semantics
but only ever issues `INSERT INTO scan_requests` and reads from every
other table.

This is enforceable by static check: serve's DB module exposes a
`fn enqueue_scan(...)` and a read-only query API; no other write
methods exist.

## Database schema additions

### scan_requests

```sql
CREATE TABLE scan_requests (
  id            INTEGER PRIMARY KEY,
  requested_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  kind          TEXT NOT NULL,            -- 'incremental' | 'full' | 'subtree'
  root          TEXT,                     -- NULL = all roots; required for 'subtree'
  status        TEXT NOT NULL DEFAULT 'pending',
                                          -- 'pending' | 'running' | 'complete' | 'failed' | 'cancelled'
  scan_run_id   INTEGER,                  -- FK → scan_runs.id once started
  started_at    TEXT,
  completed_at  TEXT,
  error         TEXT
);
CREATE INDEX scan_requests_status ON scan_requests(status, requested_at);
```

Polled by the watcher's scan-driver task every ~1s. The watcher claims
a row by transitioning `pending → running`, runs the scan, then writes
the terminal state. `scan_run_id` links to the existing `scan_runs`
table for progress.

### watcher_heartbeat

```sql
CREATE TABLE watcher_heartbeat (
  id                          INTEGER PRIMARY KEY CHECK (id = 1),  -- single row
  pid                         INTEGER NOT NULL,
  started_at                  TEXT NOT NULL,
  updated_at                  TEXT NOT NULL,
  state                       TEXT NOT NULL,
                              -- 'starting'|'reconciling'|'watching'|'scanning'|'stopping'
  watch_count                 INTEGER NOT NULL DEFAULT 0,
  pending_events              INTEGER NOT NULL DEFAULT 0,
  last_event_processed_at     TEXT,
  last_full_scan_started_at   TEXT,
  last_full_scan_completed_at TEXT
);
```

Updated every 5s while the main loop runs. Consumers (`smriti_health`,
`smriti-serve` general staleness checks) treat `now - updated_at > 30s`
as "watcher down."

### read_audit moves to audit.db

A migration in `index.db` drops the `read_audit` table. A new
`audit.db` at `~/.smriti/audit.db` is created on first use by serve,
with the same schema as today's `read_audit`. Pre-upgrade history is
not migrated — start fresh.

## Inotify event handling

### Watch registration

On startup, walk every enabled root and register a watch on each
directory using the `notify` crate (inotify backend). New directories
discovered later (via `IN_CREATE` on a directory) get watches added
dynamically. Deleted directories are cleaned up by the kernel.

### Event mask

Subscribe to:
- `IN_CLOSE_WRITE` — primary signal of "writer finished."
- `IN_CREATE` — file or directory appeared.
- `IN_DELETE` — child of a watched directory was removed.
- `IN_MOVED_FROM`, `IN_MOVED_TO` — rename, paired by cookie.
- `IN_DELETE_SELF`, `IN_MOVE_SELF` — the watched directory itself.

Explicitly **not** subscribed: `IN_MODIFY` (chatty; CLOSE_WRITE replaces
it) and `IN_ATTRIB` (chmod/chown don't affect index state).

### Debouncing

Per-path debounce, two-knob:
- **Idle window**: 1s of no further events for a path → flush.
- **Max wait**: 5s after the first buffered event → force flush, even
  if events keep arriving.

`IN_CLOSE_WRITE` is treated as a strong "ready to flush" hint — it can
shorten the idle window for that path.

The debounce buffer is a `HashMap<PathBuf, BufferedEvent>` keyed by path,
latest-event-wins for non-MOVE events.

### Move detection

`IN_MOVED_FROM` and `IN_MOVED_TO` carry a matching cookie. Pair them in
a small in-memory table with a short TTL (say 1s — kernel typically
delivers the pair back-to-back). When matched, emit a single `Moved`
event into the buffer keyed on the new path; the old path is recorded
in the move payload, not as a separate event. An `IN_MOVED_FROM`
without a matching `IN_MOVED_TO` (move out of any watched root) is
treated as a delete.

### Watch limit

If `inotify_init`/`inotify_add_watch` fails with `ENOSPC`
(`fs.inotify.max_user_watches` exhausted), the watcher refuses to start
with an actionable error:

```
error: inotify watch limit reached after registering N watches.
       Increase fs.inotify.max_user_watches:

         echo "fs.inotify.max_user_watches=524288" \
           | sudo tee /etc/sysctl.d/40-smriti.conf
         sudo sysctl -p /etc/sysctl.d/40-smriti.conf

       Current limit: <current>
       Watches needed (estimated): <count>
```

No polling fallback in v1. Loud failure beats silent partial coverage.

### Queue overflow

If the inotify task receives an `IN_Q_OVERFLOW`, the kernel has dropped
events. The watcher logs at `warn`, marks state as `reconciling`, and
triggers a full reconciliation scan immediately — same code path as the
periodic safety net. Until the scan completes, every MCP response
includes `is_stale: true` with reason `queue_overflow`.

### Network and FUSE filesystems

On startup the watcher reads `/proc/mounts` and notes any roots that
sit on filesystems known to bypass inotify (NFS, CIFS, FUSE, sshfs).
For these roots the watcher logs a `warn` ("inotify on $path may miss
remote changes; relying on periodic safety-net scan") and proceeds.
No special code path — inotify still works for local changes, the
periodic scan picks up remote ones.

## Pipeline semantics

### Per-path core

Extracted from `scanner.rs` into a function with shape roughly:

```rust
fn process_path(
    conn: &mut Connection,
    path: &Path,
    classification: Classification,    // from IgnoreStack
    prev_state: Option<&PathRecord>,   // current DB row, if any
    scan_id: Option<i64>,              // Some during batch scans, None for watcher events
) -> Result<EventOutcome>;
```

Both the batch scanner (looped over a walk) and the watcher (called
per debounced event) invoke this function. Identical semantics in
both contexts. Each invocation runs in its own SQLite transaction.

The batch scanner adds two wrappers around the loop:
- a startup phase that loads `prev_state` for every path in the
  affected roots,
- a finalize phase that detects disappeared / moved / copied paths.

The watcher needs neither: events directly state what happened, and
disappearance is signalled by `IN_DELETE`/`IN_MOVED_FROM`.

### Directory create

`IN_CREATE` on a directory means "a subtree may have appeared whose
contents we haven't seen yet." The watcher responds by invoking the
batch scanner with that directory as a single root. This is cheaper to
build and reason about than running per-file `IN_CLOSE_WRITE`-driven
inserts on every file in the new subtree, and falls naturally out of
the shared-core design.

### Failure handling per event

| Condition                                  | Action                                                   |
|--------------------------------------------|----------------------------------------------------------|
| File no longer exists (`ENOENT`)           | Skip silently; next event catches up.                    |
| Permission denied (`EACCES`)               | Log `info` once per path per session; skip.              |
| Special file (FIFO, device, socket)        | Skip silently.                                           |
| Symlink to nothing                         | Skip silently.                                           |
| Decode failure (metadata extraction)       | Store with empty metadata; still hash/index.             |
| Transient I/O error                        | Skip, log `warn`; next event retries.                    |
| `SQLITE_BUSY`                              | Auto-retried via `busy_timeout` (5s).                    |
| Repeated failures on same path             | After 5 failures in session, log once at `warn`, suppress. |
| File torn-read (changed during hash)       | Accept the snapshot hash; next CLOSE_WRITE re-hashes.    |
| DB unreachable / corrupted                 | Fail loud, exit non-zero, systemd restarts.              |
| Out of disk space                          | Fail loud (v1); v2 may add a "degraded" mode.            |
| Panic in any task                          | Bubble up, exit non-zero.                                |

The repeat-failure counter lives in memory (per-path, session-scoped).
Lost on restart — that's fine.

## Startup sequence

```
1. Acquire writer.lock (advisory exclusive flock).
   On failure: another writer is active → exit 1 with message.

2. Open index.db, run migrations.

3. Mark any scan_runs(status='running') as 'crashed' with
   error='watcher restarted'. (Recovery from prior crash.)

4. Write watcher_heartbeat: state='starting', pid, started_at, updated_at.

5. Load roots from ~/.smriti/roots, filter to enabled.
   Detect network/FUSE mounts; warn-log per such root.

6. Register inotify watches recursively across all enabled roots.
   On ENOSPC: error with sysctl hint and exit.

7. State='reconciling'. Begin a full scan across all enabled roots
   using the batch scanner. Inotify events that arrive during this
   phase are queued in the debounce buffer; they flush after the
   scan completes (and converge with the scan's writes via the
   idempotent per-path core).

8. Scan completes. State='watching'. Heartbeat continues. Debounce
   flusher and scan-driver tasks process their queues.
```

### Why register watches before walking

If we walked first and registered watches after, anything that changed
between "walked this dir" and "watched this dir" is missed. Registering
first means events for not-yet-walked files can arrive — fine, the
shared per-path core converges on the right state.

### Reads during reconciliation

`smriti-serve` continues serving queries throughout. Reads see WAL
snapshot isolation and are never blocked by the writer. The freshness
envelope (`is_stale`, `as_of`) reflects the watcher's state — during
`reconciling`, `is_stale: true` with reason `reconciling`.

## Periodic safety-net scan

Every 24h since the last completed full scan, the watcher kicks off
another full scan. The clock is "time since `last_full_scan_completed_at`,"
not wall-clock cron — laptops sleep, machines reboot, scans of varying
duration mess up cron.

Configured via `SMRITI_WATCH_FULL_SCAN_INTERVAL` (a duration string
like `24h`, `12h`, `6h`).

A new root appearing in `~/.smriti/roots` (added or enabled) triggers an
immediate scan for that root, scoped to the new root only. This serves
two purposes: USB-mounts get indexed without waiting for the periodic
clock, and adding a root has predictable latency to first results.

Periodic scans run concurrently with inotify event processing.
Per-path idempotency means the two paths can both write and converge.
No mutex.

## Coordination with smriti-serve

### Read-only contract

`smriti-serve` opens `index.db` connections without a writer-lock attempt.
Its DB module exposes:
- a read-only query API
- a single write method: `enqueue_scan(kind, root) -> request_id`

That's it. The compiler enforces that no other write paths exist in
serve's code.

### scan_requests lifecycle

```
serve calls smriti_scan tool
   │
   └─► INSERT INTO scan_requests(kind, root, status='pending')
          returns request_id
   │
   └─► poll: SELECT * FROM scan_requests WHERE id = request_id
          (timeout configurable, default 5min)
   │
   ├─► status='complete' → return result envelope to client
   ├─► status='failed'   → return error
   └─► timeout exceeded  → return {request_id, status:'running'}
                            client may poll separately
```

If the watcher's heartbeat is stale at the start of the call, serve
fails fast with "watcher not running" rather than enqueue forever.

### Roots configuration

`~/.smriti/roots` is the source of truth for roots and their
enabled/disabled state. CLI commands (`smriti roots add/disable/...`)
edit the file directly. The watcher places an inotify watch on the
file and reconciles its active watch set when it changes:

- New root appeared → walk and watch it; trigger immediate per-root
  scan.
- Existing root toggled disabled → remove its watches; preserve DB rows.
- Existing root toggled enabled → walk and watch; trigger immediate
  per-root scan.
- Root removed entirely → same as disabled. Decision to actually
  *delete* the DB rows for a removed root is left to a separate
  `smriti roots remove --purge` command, not the watcher.

## Lifecycle

### Graceful shutdown (SIGTERM, SIGINT)

```
1. Stop accepting new inotify events (drop the inotify task).
2. Stop polling scan_requests.
3. Drain debounce buffer with a 10s deadline:
     for each pending path: process via per-path core.
4. If a full scan is in progress:
     UPDATE scan_runs SET status='aborted', error='shutdown'
                       WHERE id = current.
5. UPDATE watcher_heartbeat SET state='stopping', updated_at=now.
6. Release writer.lock (implicit on close).
7. exit 0.
```

If the 10s drain deadline expires, drop the unflushed events and
proceed. Next startup's full scan reconciles them.

### SIGHUP

No-op. Env-var-only configuration; nothing to reload without restart.

### Hard crash (SIGKILL, OOM, panic)

- Kernel releases `writer.lock` automatically.
- `watcher_heartbeat.updated_at` ages out; serve reports `running:false`.
- On next startup, `scan_runs(status='running')` rows are reset to
  `crashed`, the always-full-scan reconciles missed events, and life
  resumes.

### systemd unit

```ini
# ~/.config/systemd/user/smriti-watch.service
[Unit]
Description=smriti filesystem watcher

[Service]
Type=simple
ExecStart=%h/.cargo/bin/smriti watch
Restart=always
RestartSec=2
TimeoutStopSec=30
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
```

`TimeoutStopSec=30` gives the 10s drain deadline plenty of margin.
`RestartSec=2` avoids tight loops if the watcher fails on startup.

## Observability

### Logging

`tracing` + `tracing_subscriber` with `EnvFilter` from `RUST_LOG`. All
output to stderr; journald handles persistence and rotation.

Default levels:
- `info`: lifecycle (start/stop/restart, scan start/complete with counts,
  root added/removed), recoverable errors with context.
- `warn`: queue overflow, watch-limit-near, network mount detected,
  repeated path failure.
- `error`: unrecoverable (DB unreachable, lock failure, panic).
- `debug`: per-event flow.
- `trace`: per-path classify/hash decisions.

### watcher_heartbeat

Updated every 5s. Fields documented above. Consumers (MCP `smriti_health`,
the in-flight CLI) read this to surface watcher state.

Freshness threshold: `now - updated_at > 30s` → "watcher down."
30s = 6× heartbeat cadence; tolerates one missed beat without false
alarms but flags real outages quickly.

### smriti_health

Existing tool gains a `watcher` block:

```json
{
  "watcher": {
    "running": true,
    "state": "watching",
    "pid": 12345,
    "uptime_seconds": 47291,
    "watch_count": 38201,
    "pending_events": 0,
    "last_event_processed_at": "2026-04-29T18:00:13Z",
    "last_full_scan_completed_at": "2026-04-29T03:14:02Z"
  }
}
```

If heartbeat is stale: `running: false`, other fields preserved from
last seen state for diagnostic value.

### Metrics / OpenTelemetry

Out of scope for v1. The heartbeat table answers any operational
question we currently know we'll have. Add a metrics exporter later
when there's a real question we can't answer.

## Configuration

All via environment variables, read by `Config::from_env()`. Defaults
in parentheses.

| Variable                              | Default | Purpose                                                        |
|---------------------------------------|---------|----------------------------------------------------------------|
| `SMRITI_WATCH_DEBOUNCE_IDLE_MS`       | `1000`  | Per-path idle window before flush.                             |
| `SMRITI_WATCH_DEBOUNCE_MAX_WAIT_MS`   | `5000`  | Max wait after first buffered event.                           |
| `SMRITI_WATCH_FULL_SCAN_INTERVAL`     | `24h`   | Time since last completed full scan before next one.           |
| `SMRITI_WATCH_HEARTBEAT_INTERVAL_MS`  | `5000`  | Heartbeat cadence.                                             |
| `SMRITI_WATCH_HEARTBEAT_STALENESS_MS` | `30000` | Threshold above which serve reports watcher down.              |
| `SMRITI_WATCH_SHUTDOWN_DRAIN_MS`      | `10000` | Graceful-shutdown debounce-drain deadline.                     |
| `SMRITI_WATCH_SCAN_REQUEST_POLL_MS`   | `1000`  | scan_requests poll cadence.                                    |
| `RUST_LOG`                            | `info`  | tracing filter.                                                |

No config file. Env vars are the only interface, set in the systemd
unit or by `dotenvy`.

## Testing

### Per-path core

Unit-tested in isolation. Construct a `Connection`, classification,
optional `prev_state`, call `process_path`, assert resulting rows.
Property tests for idempotency: `process_path(p) == process_path(p);
process_path(p)`. The core is the most bug-prone code; this is where
the test budget goes.

### Watcher integration

Spawn `smriti watch` as a subprocess pointing at a temp DB and a temp
root. Touch files, sleep past the debounce window, query the DB,
assert state. Real inotify; no mocking. Tests are slower but verify
the actual kernel-event path.

Specific scenarios to cover:
- Single file create/modify/delete with debounced flush.
- Atomic-write (`tmp + rename`) lands as one logical change.
- Move within a root → `Moved` event with both paths.
- Move across roots → delete + create, no `Moved` paired.
- Directory create with N children → batch scan picks up all of them.
- Crash mid-scan (kill -9, restart) → scan_runs marked `crashed`,
  reconciliation completes.
- Permission denied on a single file → other files still process.
- Roots file edit (add, disable, enable) → watch set updates.
- Heartbeat staleness via `kill -STOP <pid>` → serve reports down.

### Full-scan parity

Property test: walk a tree, run the watcher to steady state, run a
batch scan, assert the DB state is identical at both ends. Catches
divergence between the per-path-core's two callers.

### Performance

Acceptance benchmark (run pre-merge, not per-PR):
- 50k-file tree, cold cache, fresh DB: full reconcile completes
  under 5 minutes.
- 50k-file tree, warm cache, no changes since last scan: full
  reconcile completes under 30 seconds (mtime short-circuit).
- Steady-state event handling: 1000 file modifications in 10s
  process within 30s of the last event.

These are not contracts; they're regression guards. If a change
makes any of these significantly worse, that's a signal to stop and
investigate.

## Non-goals (v1)

Explicitly out of scope:

- **Embedding generation in the watcher.** Deferred until there's a
  clear case for indexing content beyond metadata.
- **fanotify backend.** Plausible upgrade path for the watch-limit
  problem; not v1.
- **Polling fallback** when inotify limit is hit. Refuse-with-error
  is the v1 behaviour.
- **Streaming scan progress** to MCP clients via socket. Polling
  `scan_runs` is sufficient.
- **Multi-host / shared-filesystem coordination.** smriti is per-user,
  per-host.
- **Degraded mode on disk-full.** v1 fails loud.
- **Prometheus / OpenTelemetry exporters.** Heartbeat table is enough.
- **Audit history migration** from the old `index.db.read_audit` to
  the new `audit.db`. History dropped on upgrade.

## Migration / upgrade

Existing users on the in-process scanner upgrade by:

1. `cargo install` the new binary.
2. Run `smriti install-services` (existing Wave 6 plan) to drop the
   systemd unit files.
3. `systemctl --user enable --now smriti-watch.service smriti-serve.service`.
4. Migrations run automatically on first DB open: `scan_requests` and
   `watcher_heartbeat` tables added, `read_audit` dropped from `index.db`.
   `audit.db` created lazily by serve.

Pre-upgrade `read_audit` history is not preserved.

## Dependencies on other work

- **Scanner decomposition** (smriti-overall-refactor.md #1) — extract
  the per-path core. Prerequisite.
- **HTTP transport for MCP** (Wave 4 of plans/daemon-triage-usb.md) —
  serve runs as a separate long-lived process. Prerequisite for the
  two-process model in practice.
- **Roots enable/disable** (Wave 1) — needed for the roots-file
  reconciliation in this design.

## Open questions

None blocking. Items deferred for later passes:

- Actual `smriti scan` CLI behaviour (must respect single-writer; fold
  into watcher trigger).
- Whether `smriti roots remove` should purge DB rows or just disable.
- Whether the pre-merge performance benchmarks become CI-gated.
