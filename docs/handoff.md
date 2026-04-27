# Handoff — smriti

## Pick up

The new `~/.local/bin/smriti` binary has the UTF-8 panic fix. The user's scan
against /home/josh should now run to completion (~30+ min, single
monolithic transaction — see plan below for why and what to do about it).

### Immediate next step (verify)

Re-run the user's scan with the new binary and confirm it commits:

```bash
RUST_LOG=smriti=info smriti scan
# Expect after ~4 min: "walk complete: N files current, M events queued, beginning commit"
# Expect after ~30 min: "Scan complete in Xms"
smriti health  # last_scan should now be recent
```

If it crashes again, capture stderr and look for new panics — there may be
other latent issues that the old SIGBUS / UTF-8 panic shadowed.

### Next major work item: scanner refactor

Plan: `docs/plans/scanner-batched-commits.md`. Per-batch commits with a
scan-generation pattern. This is the right structural fix; the patches
shipped today only stop today's specific crashes. Until this lands, the
scanner remains a 30-minute all-or-nothing operation that fails any time
a single file triggers any code path bug.

**Inline addition the user approved at plan line 143**: include the
`smriti scan-status` command (polls `scan_runs WHERE status = 'running'`,
prints `files_seen / wall_time`) — make it part of the initial implementation,
not a follow-up.

Suggested implementation order (each its own commit):
1. Migration `0002_scan_generations.sql` + run_migrations idempotency check.
2. New `scan_batched()` function next to `scan()`, gated by
   `SMRITI_SCAN_BATCHED=1`. Reuse the walk loop; only the commit/diff/event
   pipeline changes.
3. Move/copy detection upgrade in finalize txn (requires `events.scan_id`).
4. `smriti scan-status` CLI command + corresponding MCP tool.
5. Test on /home/josh end-to-end. Compare doc/event counts vs legacy.
6. Flip default to batched. Delete legacy code path.

### Known smaller bugs (not blockers)

- `smriti roots remove` accepts non-existent paths silently (e.g.
  `/home/josh/Downlaods` typo). Should error or warn. Caught yesterday,
  noted in archived `.handoffs/2026-04-27T04-37-09.md`.
- `embed_excluded` flag on documents is never set; classification info isn't
  threaded through to the document insert.
- Stale FTS entries accumulate when a file's content_hash changes — old FTS
  row for the superseded hash is not deleted.

### What's solid

- v0.1 implementation complete: all 8 issues across 5 waves.
- 57 tests pass (`cargo test`), zero warnings.
- WAL-checkpoint-on-open prevents the SIGBUS class of crash.
- char-boundary-safe FTS truncation with 3 regression tests.
- Migration is idempotent (CREATE IF NOT EXISTS), so re-opening the DB
  doesn't fail.
- Daemon (MCP over stdio) and embedding pipeline (BGE-M3, feature-gated) are
  in but untested on real data.
