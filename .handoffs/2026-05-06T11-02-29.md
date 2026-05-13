# Handoff

Last updated: 2026-04-29 (appended at session end)

## Track C: Watcher daemon design (NEW — ready for to-issues)

Full design spec done this session. Spec is at `docs/daemon-design-spec.md`,
with `CONTEXT.md` at the repo root and four ADRs in `docs/adr/` capturing
the load-bearing decisions. Old `daemon-sketch.md` archived.

**Next step:** the mp-skills pipeline says `grill-with-docs → to-issues → tdd`.
When ready to plan implementation, run:
1. `setup-matt-pocock-skills` once on this repo (configures issue tracker
   + triage labels — prereq for to-issues).
2. `to-issues` pointed at `docs/daemon-design-spec.md` to break the spec
   into vertical-slice GitHub issues.
3. Implement with `tdd` skill.

**Hard prerequisite before implementation:** the scanner decomposition
refactor (smriti-overall-refactor.md #1, "Scanner phase extraction" in
Track A) extracts the per-path core that both the batch scanner and the
watcher will share. Cannot start the watcher implementation until that's
done. This means Track C effectively requires Track A item #3 first.

**Key commitments to remember when planning:**
- Single-writer on `index.db` (Watcher only); MCP becomes read-only.
- `read_audit` moves out of `index.db` into its own `audit.db`.
- DB-only coordination — no sockets, no signals between Watcher and Serve.
- Always-full-scan on watcher startup; readers see WAL snapshot, never blocked.
- Embeddings: deferred / out of scope for the watcher (smriti stores
  metadata only, embeddings may not be worth doing at all).

**Open thread:** the existing `docs/plans/daemon-triage-usb.md` predates
the spec. Wave 5 (watcher) of that plan is now superseded by the design
spec; the plan should either be updated to reference the spec, or
retired in favour of the issues that come out of `to-issues`.

---



## What to pick up next

Two tracks are ready for planning:

### Track A: smriti refactor (7 candidates)

`docs/smriti-overall-refactor.md` has 7 deepening opportunities from a full
architecture review, with suggested sequencing:

1. Quick wins first: utilities extraction (#5), editor dedup (#6), migration
   version table (#7) — could be one PR.
2. Scan setup consolidation (#2) — fixes a real bug where MCP scans skip
   user smritiignore rules.
3. Scanner phase extraction (#1) — break the 600-line `scan_batched` into
   testable phases.
4. search.rs decomposition (#3) — split 5 responsibilities into focused modules.
5. Store abstraction (#4) — the biggest, benefits from all prior work.

Each candidate needs a grilling session (design tree walk) before
implementation.

### Track B: smriti_events_since MCP tool

The load-bearing seam for the kosha integration. Design is clear from the
grilling session — polling tool over the existing events table, cursor in
the consumer, no smriti schema changes needed. Ready for a focused
implementation PR. Details in `docs/smriti-next-steps.md` (item 2 + the
appended kosha architecture update).

## New docs this session

- `../kosha/docs/architecture.md` — proper kosha architecture doc, supersedes
  kosha-sketch.md. All 15 design decisions nailed down.
- `docs/smriti-overall-refactor.md` — 7 refactor candidates with files,
  problems, solutions, dependencies, risk, sequencing.

## Context the next session should have

- kosha architecture is settled. Name is "kosha" (not vedakosha). Storage is
  Postgres+pgvector. Core concepts: book (any ingested document), segment
  (format-native decomposition unit), citation ({book_id, segment_index,
  segment_label}).
- The MCP scan ignoring user smritiignore rules is a real bug — prioritize
  fix #2 if doing any smriti code work.
- Qwen3-VL vs bge-m3 benchmark is pending. Affects whether the ecosystem
  consolidates on one embedding model.
