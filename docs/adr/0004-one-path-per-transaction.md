# One path per transaction is the atomicity unit

Each path's state change commits in a single SQLite transaction inside
the Per-path core. Multi-path batching, where it appears (e.g. the
batch scanner's per-N-files commits), must remain safe to abort
partway — i.e. each path's writes within the batch are independently
consistent.

## Considered alternatives

- **Multi-path transactions for throughput.** Group N events from the
  debounce flush into one transaction, amortising fsync cost. Rejected
  because crash recovery becomes harder to reason about: a partial
  commit could leave some paths' writes applied and others not, and
  the only correct recovery is "redo the whole batch," which the
  always-full-scan-on-startup already gives us — but at the cost of
  more state to track during steady-state operation.
- **No explicit transactions** (autocommit). Every statement is its own
  transaction. Slow because every write goes to the WAL synchronously,
  and a single conceptual change like "compute hash, upsert document,
  upsert path, insert event" would not be atomic — readers could see
  half-applied state.

## Consequences

- Crash recovery is trivial: any committed transaction is consistent;
  any uncommitted one is discarded by SQLite WAL on next open. No
  half-applied path state ever exists.
- The Watcher and a concurrent periodic scan can both write the same
  path; the per-path-per-transaction guarantee plus idempotency in the
  Per-path core means convergence without coordination.
- Throughput is bounded by SQLite write commit rate. For our workload
  (~thousands of events per minute peak) this is comfortably enough.
  If batching ever becomes necessary, it has to preserve the
  per-path-safe-to-abort property — i.e. not introduce cross-path
  invariants within a single transaction.
