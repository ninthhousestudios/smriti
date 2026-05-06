# read_audit lives in a separate audit.db

`read_audit` — the per-read privacy log written by Serve on every
`smriti_read` call — moves out of `index.db` into its own SQLite file
at `~/.smriti/audit.db`. Serve owns it entirely; the Watcher never
touches it.

## Considered alternatives

- **Strict single-writer with no carve-outs.** Serve writes audit rows
  by enqueueing them in a table the Watcher drains. Every file read
  becomes a round-trip through the Watcher. Rejected — pathologically
  expensive for what is operational logging.
- **Carve-out within `index.db`.** Single-writer rule applies only to
  "data that affects query results"; Serve retains write access to
  `read_audit` only. Pragmatic but leaves an exception in an otherwise
  clean invariant — every future reader of the design has to remember
  the exception exists.

## Consequences

- The single-writer invariant on `index.db` becomes absolute, with no
  exceptions to remember or test for.
- `audit.db` can be rotated, backed up, or purged independently of
  the index.
- Pre-upgrade `read_audit` history in `index.db` is dropped on
  migration. Audit history is operational logging, not state — losing
  it on an explicit upgrade is acceptable.
- One additional SQLite file to manage, one additional connection
  inside Serve. Trivial.
