# Single-writer architecture: the Watcher owns all writes to index.db

`index.db` has exactly one writer process at any time, enforced by an
advisory file lock at `~/.smriti/writer.lock`. The Watcher holds it for
its lifetime; Serve is read-only against `index.db`. Any CLI tool that
needs to write must acquire the lock and refuse if held.

## Considered alternatives

- **Multi-writer with WAL contention.** Both Watcher and Serve hold
  write connections, rely on `busy_timeout` and WAL retries to
  serialise. Cheaper to ship — Serve keeps its current in-process
  scanner — but loses the invariant that there is one place where
  state mutations originate, which makes ordering bugs across the two
  processes much harder to reason about.
- **Watcher as a tokio task inside Serve.** One process, one
  connection, no IPC. Couples the failure domains: a Serve crash kills
  the Watcher and vice versa. Rejected on the grounds already listed
  in the daemon design spec — the failure domains should be
  independent.

## Consequences

- Serve cannot scan. Every scan trigger (incremental, full, on-demand
  from an MCP tool call) goes through `scan_requests` and is executed
  by the Watcher. The MCP `smriti_scan` tool blocks on `scan_requests`
  status until completion or timeout.
- A `smriti scan` CLI cannot coexist with a running Watcher; it must
  acquire the lock and refuse if held. Resolution of the CLI's exact
  shape is deferred — the design constraint is fixed.
- `read_audit` cannot live in `index.db` because Serve must remain
  read-only there; it moves to a separate `audit.db`. See ADR 0003.
