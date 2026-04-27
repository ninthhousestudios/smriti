# Handoff — smriti

## Pick up

Implementation plan is complete and pre-mortem passed. No code exists yet. Start with **Issue 1** (project scaffold + core types).

### Key artifacts

- **Implementation plan:** `.agents/plans/2026-04-26-smriti-v01-implementation.md` — 8 issues, 5 waves, symbol-level detail
- **Pre-mortem:** `.agents/council/2026-04-26-pre-mortem-smriti-v01.md` — all 12 findings addressed in plan
- **Design sketch:** `docs/smriti-sketch.md` — comprehensive design (schema, tools, algorithms)
- **Research:** `.agents/research/2026-04-26-smriti-implementation.md` — chitta-rs patterns, crate ecosystem

### Implementation order

```
Wave 1: scaffold (Cargo.toml, config, error, db, roots, migrations)
Wave 2: ignore parser || hasher+metadata || privacy gate  (parallel)
Wave 3: scanner
Wave 4: search + CLI  ← first milestone: smriti scan + smriti audit
Wave 5: MCP+daemon || embedding  (parallel)
```

### Validate first

**sqlite-vec loading** is the highest ecosystem risk. After `cargo build` succeeds in Issue 1, immediately test that `sqlite-vec` extension loads and the `vec0` virtual table works with rusqlite. If it doesn't, everything downstream is blocked.

### Critical design decisions to remember

1. **`ignore` crate, not `globset`** — for full gitignore semantics. Use `ignore::gitignore::Gitignore` struct as standalone matcher.
2. **Incremental `IgnoreStack`** — push/pop layers as scanner enters/exits directories with local .smritiignore files. Not batch loading.
3. **Path canonicalization** in privacy gate — `std::fs::canonicalize()` prevents traversal attacks.
4. **Module-owns-queries** — db.rs handles connection + migrations only. Each module (scanner, search, privacy) defines its own SQL functions taking `&Connection`.
5. **Roots are arbitrary absolute paths** — not assumed under `~`. Backup drives, NAS mounts work.
6. **FTS content threshold** — `SMRITI_FTS_CONTENT_MAX_BYTES` (100KB). Large files get title+topics+summary only.
7. **Metadata extraction cap** — `SMRITI_MAX_METADATA_BYTES` (500MB). Above this → `is_binary=true`, skip extraction.
8. **Stale socket detection** — daemon tries connect before bind, removes stale socket.

### Suggestions from gemini-flash review (2026-04-26)

Source: `~/soft/manas/docs/gemini-flash-suggestions.md`. Two items relevant to smriti:

1. **Default-deny indexing.** Indexing `~` by default is a security risk — embeddings of secrets are recoverable from a vector index. Smriti should require explicit allowlisted roots (no implicit `~`). Aligns with existing design decision #5 ("roots are arbitrary absolute paths") but should be enforced: refuse to scan unless at least one root is explicitly added, and never auto-add `~`.

2. **Freshness envelopes on all read tools.** Every smriti tool that returns indexed data should include `as_of` (ms since epoch when the data was indexed/scanned) and `is_stale` (boolean against some threshold). Lets callers reason about whether they're looking at a fresh scan or a three-day-old snapshot. This is a cross-subsystem manas principle — see `manas/docs/freshness-envelopes.md` (TBD) for the shared rule.

Sideband sync between smriti and chitta (file-move propagation without LLM round-trip) was also suggested but is already known — track in the broader manas roadmap, not here.

### Upstream docs (unchanged from prior session)

- Grantha sketch: `docs/grantha-sketch.md`
- Opus 4.7 review: `~/soft/chitta/docs/manas-opus47-review.md`
- Master roadmap: `~/soft/manas/docs/roadmap.md`
- Manas architecture: `~/soft/chitta/docs/manas-architecture.md`
