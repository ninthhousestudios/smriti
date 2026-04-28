# Handoff — smriti

## Pick up

### 1. Fix stale WAL/SHM and benchmark full scan on ~

Josh moved `~/.smriti/index.db` but left the `-wal` and `-shm` files behind,
causing corruption. Before scanning:

```bash
rm ~/.smriti/index.db ~/.smriti/index.db-shm ~/.smriti/index.db-wal
smriti init
smriti roots add ~
```

Then benchmark the new parallel hashing pipeline:

```bash
# Full power (all 12 hyperthreads)
time smriti scan

# Leaving headroom
time smriti scan -j 4
```

Compare wall time and check `smriti scan-status` / `smriti health` afterward.

### 2. Consider guarding against orphaned WAL/SHM

`smriti init` could detect stale `-wal`/`-shm` files when the `.db` is missing
or empty, and clean them up automatically. Low effort, prevents user confusion.

### 3. Remaining feature work

From the README planned section, in rough priority order:

- **Parallel walk** — the walk phase is still single-threaded via `walkdir`.
  Could switch to `ignore::WalkParallel` or `jwalk` for parallel directory
  traversal. Separate from the hash parallelism already implemented.
- **`smriti watch`** — inotify/fanotify for incremental updates instead of
  full rescans. Would make smriti usable as a background service.
- **systemd user service** — run the daemon as a persistent service.
- **Hybrid search in CLI/MCP** — `search_hybrid` exists in the codebase but
  isn't wired to commands yet.

### 4. Explore smriti for the backup problem

Still open from prior session — see if the tier 1/2 split and `smriti manifest`
can drive Josh's backup workflow. Need a complete scan on ~ first.

## What's solid

- Three-phase scan pipeline: walk → parallel hash (rayon) → batched DB commit
- `-j/--jobs` flag for thread control
- 80 tests pass, legacy scanner deleted
- Binary installed at `~/.cargo/bin/smriti`
