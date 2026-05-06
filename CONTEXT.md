# smriti

Filesystem perception: smriti scans, hashes, and tracks files; serves
metadata and search results to agents over MCP. "That which is remembered."

This file pins down terminology specific to smriti's domain — concepts
that came up in design conversations and would otherwise drift. Project
conventions, generic engineering terms, and library names don't belong
here.

## Language

**Watcher**:
The `smriti-watch` process. Long-lived background daemon that holds the
sole writer lock on `index.db`, reacts to inotify events, and runs full
reconciliation scans on startup and on a periodic safety-net schedule.
_Avoid_: daemon (ambiguous — there are two long-lived processes), `smriti-watch` in prose (use the term Watcher; reserve `smriti-watch` for command-line and unit-file references).

**Serve**:
The `smriti-serve` process. Long-lived MCP server over HTTP that answers
queries from agents. Read-only against `index.db`; writes only to
`audit.db` and `scan_requests`.
_Avoid_: MCP server (ambiguous when multiple MCPs are in play); daemon.

**Per-path core**:
The shared function (extracted from `scanner.rs`) that classifies, hashes,
diffs, and persists state for a single path. Called in a loop by the
batch scanner and per-event by the Watcher. The single source of truth
for "how a file becomes a row." Must be idempotent — same path processed
twice produces the same DB state.
_Avoid_: `process_path` in prose (use Per-path core; reserve the function
name for code).

**Reconciliation**:
A full-tree scan triggered to catch up changes that inotify either
missed or never saw. Distinct from a periodic scan (which is just a
clock-driven reconciliation) and a triggered scan (one enqueued via
`scan_requests`). Reconciliation is the *purpose*; full scan is the
*mechanism*.
_Avoid_: catch-up scan; recovery scan.

**Writer lock**:
The advisory file lock at `~/.smriti/writer.lock` that enforces the
single-writer invariant on `index.db`. The Watcher holds it for its
lifetime. Any other writer (e.g. a CLI scan, in some yet-undecided
form) must acquire it and refuse if held. Kernel-released on process
exit, so stale locks are not a concern.
_Avoid_: PID file (different mechanism); flock (the Linux primitive,
not the smriti concept).

## Relationships

- The **Watcher** is the sole writer to `index.db`. **Serve** is read-only.
- Both the **Watcher** (per event) and the batch scanner (per file in a
  walk) invoke the **Per-path core**.
- A **Reconciliation** is a full-tree scan run by the Watcher. It is
  triggered on startup, on a 24h periodic schedule, on inotify queue
  overflow, or on demand via `scan_requests`.
- The **Writer lock** is held only by whatever process is currently the
  writer. In normal operation, that is the Watcher; if the Watcher is
  not running, a CLI tool may acquire it for a one-shot scan.

## Example dialogue

> **Dev:** "If the **Watcher** is processing an inotify event for `foo.md`
> and a **Reconciliation** is running at the same time, do we lock the
> path?"
>
> **Domain expert:** "No. The **Per-path core** is idempotent — both
> writers can process the same path and converge. The only lock is the
> **Writer lock** itself, which the Watcher holds for the whole process,
> so there is no in-process race to coordinate."

## Flagged ambiguities

- "scan" was used loosely to mean any of: a single per-path event,
  a periodic scan, a startup scan, an inotify-overflow recovery,
  and an on-demand triggered scan. Resolved: a **Reconciliation** is
  the full-tree variant and is what the term "full scan" refers to in
  the design spec; per-event work is just "event processing."
- "daemon" was used to refer to both the Watcher and Serve. Resolved:
  use the proper-noun terms; reserve "daemon" for the systemd-unit
  context where both are meant collectively.
