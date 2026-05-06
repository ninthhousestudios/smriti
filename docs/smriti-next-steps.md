# smriti — next steps

Status: pre-design notes
Date: 2026-04-29
Companion to: `smriti-kosha-architecture-sketch.md`

This is the basis for a more detailed design + implementation plan, not the plan itself. It enumerates the changes smriti needs to make the smriti+kosha architecture real, and the smaller honesty fixes that fall out of the rethink.

## guiding principle

Smriti's job is *perception and access*. Anything that drifts away from that should be retired or moved. Anything that strengthens those two roles is in scope.

## work items, ordered by priority

### 1. README and docs honesty pass

**Problem.** The current README leads with "separating what you created from what tools generated" — a disk-cleanup framing that gdu does better. It also describes FTS over `content` as a first-class semantic-search feature, while the practical reach is "title/topics/summary + 100 KB of text-file body." Binary docs (PDF, epub) get filename + size only, which the README does not state plainly.

**Change.**
- New lead framing: "filesystem perception layer for agents and downstream tools."
- Honest reach statement on search: text files only, 100 KB cap, point at kosha for deep document search.
- Add a "Planned" section pointing at the smriti+kosha split.

**Cost.** Documentation only. No code.

**Why first.** Cheapest, biggest clarity win, and unblocks anyone (including future-us) trying to figure out what smriti is for.

### 2. Scan event stream — the kosha integration point

**Problem.** Kosha needs to know when smriti discovers or updates a PDF. Today kosha would have to poll smriti or watch the filesystem itself, both of which defeat the architectural split.

**What's needed.**
- Persisted event log already exists (`events` table). What's missing is a *subscription* interface — a way for an external process to receive new events as they're committed.
- A simple shape: a "since" cursor (event id or timestamp), a polling MCP tool (`smriti_events_since`), and later an HTTP SSE / streaming push if polling proves insufficient.
- Filter parameters: by event type, file extension, mime type, or root.
- Idempotency: the same event id returned twice should be safe; consumers track their own cursor.

**Open questions.**
- Push vs. pull for v1? Pull (polling) is simpler and matches the existing rmcp tool model. Push (SSE) is nicer but adds transport complexity. Probably pull first.
- Retention. Today `smriti prune` clears events older than 30 days. Subscribers that fall behind for >30 days lose events. Document this; revisit if it bites.
- Where does the cursor live? In the consumer (kosha), not in smriti. smriti stays stateless w.r.t. subscribers.

**Cost.** Modest. The data is already in `events`. Need a new MCP tool, CLI command, and probably a small `event_watermark` table is *not* needed if cursors live in consumers.

**Why high priority.** This is the load-bearing seam between smriti and kosha. Without it the architecture is theoretical.

### 3. Bulk / efficient read path for downstream ingestion

**Problem.** When kosha ingests a 500-page PDF, it needs the file bytes. Today the only audited read path is `smriti_read`, which is shaped for "give me one file's content" — fine for that, but kosha may want streaming or to receive a pointer + a verification token rather than the bytes themselves.

**Options.**
- **A.** Have kosha call `smriti_read` per file. Simple. Logs every kosha read. Probably fine for v1 — even a few thousand PDFs is not a hot path.
- **B.** Add a `smriti_open` that returns `{path, content_hash, allowed: bool, reason}` after gate check, letting the caller open the file directly. Cheaper but bypasses the read audit; a less-strict variant for trusted local consumers.
- **C.** Stream-by-chunks variant of `smriti_read` for very large files. Probably premature.

**Recommendation.** Start with A. Revisit if kosha's ingestion shows the audit log growing pathologically or if MCP transport becomes a bottleneck.

**Cost.** Small for option A (already implemented). Medium for B (new tool + audit policy decision).

### 4. Honesty in the search story — name what FTS actually covers

**Problem.** FTS over `content` is presented uniformly across file types in the README and architecture doc. The 100 KB cap and binary-skip behavior are real but undocumented in user-facing copy.

**Change.**
- Rename or scope the FTS section to "shallow search" or "text-file search" in user docs.
- Surface the cap (`SMRITI_FTS_CONTENT_MAX_BYTES`) more prominently — it's the difference between "smriti found my note" and "smriti found a stub of my note."
- Decide whether dense embeddings, when feature-gated on, should also be over title/topics/summary or attempt the same content slice. Today it's the same shallow material, which makes the dense feature underwhelming. Consider deprecating the smriti-side dense embedding path entirely once kosha is real, and steering "deep semantic search" to kosha unconditionally.

**Cost.** Documentation + a potential decision to retire the embedding feature flag in smriti.

### 5. Planned: file watcher (already on the roadmap)

`smriti watch` (inotify/fanotify) is already in the planned section. With the event stream from item 2, the watcher becomes far more useful — incremental events flow to kosha within seconds of a file change instead of waiting for the next manual scan. Worth coordinating its design with the event-stream design so they share an event shape.

### 6. Planned: systemd user service

Persistent daemon for `smriti serve` and `smriti watch`. Already roadmapped. Mostly packaging work. Becomes more important once kosha is a daily-driver consumer.

### 7. Reframe or retire `smriti triage` as the headline UX

**Problem.** Triage was framed as the on-ramp for "smriti for cleanup." With gdu owning that use case, triage is less central. It still has a role — codifying classification decisions into `.smritiignore` so future scans respect them — but it should be positioned as **classifier maintenance**, not **disk cleanup**.

**Change.** Documentation framing only. The feature is fine; the pitch needs adjusting.

**Cost.** Documentation.

### 8. Catalog tier — keep, but reframe

The cataloged (tier 2) feature tracks "exists, big, regenerable" without indexing. Useful for: backup audits, knowing what's there without paying the metadata cost. Worth keeping. Worth documenting as "for the perception layer to know about a directory's *shape* without indexing its *contents*."

Not a code change — a framing change.

### 9. Eventual: a `smriti subscribe` CLI for debugging

Once event subscription exists, a CLI that tails events to stdout (`smriti subscribe --since=… --types=created,updated`) is invaluable for debugging kosha's pipeline. Easy to bolt on after item 2.

## items NOT to do

These came up while thinking about the rethink and are explicitly out of scope:

- **PDF text extraction in smriti.** This is kosha's job. Doing it in smriti would couple the perception tier to format-specific tooling that needs to evolve fast.
- **Per-page anything in smriti.** smriti's unit is the file. Period.
- **A second embedding model for multimodal.** Qwen3-VL lives in kosha. smriti's optional dense embedding can stay as-is or be retired, but it should not grow.
- **Storing file content blobs.** The "Content blob store + revert" planned item is a maybe-someday, not a near-term move. It collides with kosha's storage if not designed carefully.
- **Building a UI.** smriti is library + CLI + MCP. UIs sit on top. No web frontend, no TUI, no overlap with gdu.

## sequencing suggestion

Roughly:

1. README + docs honesty pass (item 1) — days, not weeks. Unblocks understanding.
2. Event stream MCP tool (item 2) — the load-bearing change. Could be a single focused PR.
3. kosha-side subscriber prototype that consumes the stream and triggers ingestion. (Out of smriti's repo, but proves the seam works.)
4. Watcher (item 5) — integrates cleanly once the event shape is settled.
5. Everything else — cosmetic/framing/packaging, can happen in any order.

## what this enables

After items 1-3 land:

- A user can type "find passages about samadhi in my classical-text library" and an agent can sequence smriti_find → kosha_search to answer it.
- Adding a new PDF to `~/library/` automatically triggers ingestion in kosha (after watcher lands too).
- Other future consumers (e.g. chitta auto-importing markdown notes that match a tag, an agent that watches `~/Downloads` for new code repos) can plug into the same event stream without smriti needing to know about them.

## open questions for the design phase

- Event stream transport: MCP-tool polling vs. SSE vs. Unix-socket pub/sub.
- Schema additions: do we need an explicit `subscribers` table, or are stateless cursors enough?
- Backpressure: if kosha falls behind, do events get dropped at the prune horizon? Should subscribers be able to bump the retention of events they care about?
- Versioning: how do we evolve the event schema without breaking existing consumers?
- Trust model for option B (`smriti_open`): if a consumer can fetch a path-plus-allow-token and read the file directly, what's the audit story?

These are the questions the next, more detailed design doc should answer.

## update: kosha architecture now nailed down (2026-04-29)

The kosha architecture has been grilled and documented at `../../kosha/docs/architecture.md`. Key decisions that affect smriti's next steps:

- **`smriti_events_since` is confirmed as the integration point.** kosha will poll this tool with a cursor (last `events.id` processed). No push/SSE needed for v1. The existing `events` table schema is sufficient — no changes required. This is item 2 above and remains the priority code change.
- **kosha reads files via `smriti_read`.** Option A (per-file reads) is confirmed for v1. No bulk read path needed yet.
- **kosha filters the event stream itself.** smriti does not need subscriber registries, per-consumer state, or event routing. smriti stays stateless w.r.t. subscribers.
- **Retention caveat is accepted.** If kosha falls behind for >30 days (prune horizon), it loses events and must do a full reconciliation. Document this in the `smriti_events_since` tool description; no code change needed.
- **kosha uses Postgres, not SQLite.** This is a kosha-side decision with no impact on smriti.
