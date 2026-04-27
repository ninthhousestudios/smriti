# Handoff — smriti

## Pick up

### 1. Install and test batched scanner on real data

```bash
cargo install --path . --force
SMRITI_SCAN_BATCHED=1 RUST_LOG=smriti=info smriti scan
# Watch for batch progress logs every ~10 batches
# In another terminal: smriti scan-status
smriti health
```

Compare doc/event counts against a legacy scan (without SMRITI_SCAN_BATCHED).
If results match, flip the default to batched and delete legacy code.

### 2. Daemon workflow / systemd

The daemon (`smriti daemon`) runs MCP over stdio — designed for editor/agent
integration, not filesystem watching. It does NOT auto-scan on file changes.

Questions to resolve:
- Should there be a `smriti watch` command that uses inotify/fanotify for
  incremental updates? (Out of scope per the plan, but the natural next step.)
- Should `smriti scan` run on a cron/systemd timer (e.g., every 5 min)?
- Should the daemon be a systemd user service so MCP clients can connect?

The daemon is untested on real data. Test it via the MCP stdio protocol.

### 3. After batched scan is validated

- Flip default: remove the `SMRITI_SCAN_BATCHED` gate, make batched the only path
- Delete `scan_legacy()` and the dispatch logic in `scan()`
- Add `scan-status` as an MCP tool (currently CLI only)

## What's solid

- Batched scanner implemented and tested (80 tests pass, 7 new batched tests)
- Three bug fixes shipped: roots remove validation, embed_excluded threading,
  stale FTS cleanup
- Migration 0002 is idempotent (probes column existence before ALTER TABLE)
- Legacy scan path preserved as fallback behind feature gate
