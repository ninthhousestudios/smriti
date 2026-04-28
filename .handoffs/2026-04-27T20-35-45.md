# Handoff — smriti

## Pick up

### 1. Check the batched scan results

A batched scan (`SMRITI_SCAN_BATCHED=1`) was kicked off on `/home/josh` this
session. Check whether it completed:

```bash
smriti scan-status
smriti health
```

If it completed, look at doc/event counts and spot-check a few files with
`smriti find` and `smriti history`. If it failed, check the error — the DB
locking fix should have resolved the previous "database is locked" crash.

### 2. Explore smriti for the backup problem

Josh wants to see if/how smriti can help with his backup situation. Relevant
commands:

- `smriti audit` — shows tier 1 (back this up) vs tier 2 (regenerable) breakdown
  with byte totals
- `smriti manifest` — exports tier-1 paths for rsync/restic/borg
- `smriti manifest --format ndjson` — richer output with hashes

Questions to explore:
- Does the tier 1/2 split match what Josh actually cares about backing up?
- Are there directories that should be cataloged (tier 2) but aren't in the
  hardened defaults? Might need a `~/.smritiignore` with `[catalog]` entries.
- What does the byte breakdown look like? How much is tier 1 vs tier 2?
- Can `smriti manifest | rsync ...` be a practical backup workflow?

### 3. Decide next steps

Based on scan results and backup exploration, decide:
- Flip batched scanner to default? (delete legacy code)
- Add a user-level `~/.smritiignore` for Josh's specific setup?
- Wire up `smriti watch` or a cron/systemd timer for regular scans?
- Any other features needed for the backup use case?

## What changed this session

- **DB locking fix**: `busy_timeout(5s)` added to all connections;
  `wal_checkpoint(TRUNCATE)` moved from `db::open()` to `db::checkpoint_wal()`
  called only before scans. Read-only commands no longer contend with active
  scans.
- **README.md**: comprehensive project README covering features, commands,
  config, MCP tools, current state, and planned features.
- **docs/architecture.md**: module breakdown, data flow diagrams, full schema,
  concurrency model.
- **docs/index.md**: updated to include architecture doc.

## What's solid

- DB locking fix compiled, installed, scan running with it
- 80 tests pass (as of last full run)
- README and architecture docs complete
