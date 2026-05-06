# DB-only coordination between Watcher and Serve

Watcher and Serve communicate exclusively through SQLite tables —
`scan_requests` for triggered scans, `watcher_heartbeat` for
liveness — plus an inotify watch on `~/.smriti/roots` for config
propagation. There are no Unix sockets, signals, or other RPC
mechanisms between the two processes.

## Considered alternatives

- **SIGUSR1 wake-signal** layered on top of DB polling. Reduces the
  ~1s polling latency for scan requests to near zero. Rejected for v1
  because the latency floor isn't a real problem (human-triggered
  scans, not interactive flows) and the signal handler adds a second
  IPC mechanism with its own failure modes.
- **Unix socket / RPC** for streaming scan progress to MCP clients.
  Plausibly nice for showing live progress bars in agents, but adds a
  whole second IPC surface (concurrent connections, framing,
  serialisation). Polling `scan_runs` is sufficient.
- **A control-channel table** (`control_requests`) generic enough to
  cover all coordination needs. Rejected as premature; the only
  bidirectional flows we have are scans and roots-config, and they
  don't share a schema usefully.

## Consequences

- If the DB is unavailable, both processes fail the same way; there is
  no "partially working" coordination state.
- Scan-trigger latency from MCP tool call to scan start is bounded by
  the `scan_requests` poll interval (default 1s). Acceptable.
- Roots-file changes are picked up via the Watcher's inotify watch on
  `~/.smriti/roots`. CLI commands that edit roots are blissfully
  unaware of the Watcher; they just write the file.
- Adding any future watcher feature that needs IPC requires either a
  new table or revisiting this decision.
