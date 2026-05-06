# docs manifest

- [smriti-kosha-architecture-sketch.md](smriti-kosha-architecture-sketch.md) — Split of responsibilities between smriti (perception) and kosha (comprehension); composition story and interface seams.
- [smriti-next-steps.md](smriti-next-steps.md) — Pre-design notes enumerating the changes smriti needs to support the smriti+kosha architecture; basis for a later detailed plan.
- [smriti-overall-refactor.md](smriti-overall-refactor.md) — Seven deepening opportunities from an architecture review: scanner decomposition, scan setup consolidation, search split, store abstraction, utilities extraction, editor dedup, migration versioning.
- [smriti-sketch.md](smriti-sketch.md) — "That which is remembered." Filesystem perception and lifecycle tracking.
- [first-sutra-sketch.md](first-sutra-sketch.md) — "Thread/Rule." AST-based code intelligence.
- [grantha-sketch.md](grantha-sketch.md) — "Text/Work." Document intelligence (page extraction, OCR placeholder).
- [architecture.md](architecture.md) — System architecture, module responsibilities, data flows, schema.
- [handoff.md](handoff.md) — Active session handoff and context for next steps.
- [plans/scanner-batched-commits.md](plans/scanner-batched-commits.md) — Refactor `scanner::scan` from a single monolithic transaction to per-batch commits with a scan-generation pattern.
- [plans/daemon-triage-usb.md](plans/daemon-triage-usb.md) — Implementation plan for daemon architecture, triage, USB workflow, watcher (6 waves).
- [daemon-design-spec.md](daemon-design-spec.md) — Canonical design spec for the Watcher daemon (`smriti-watch`): single-writer architecture, inotify mechanics, lifecycle, coordination, schema, testing. Supersedes the daemon sketch.
- [archived/daemon-sketch.md](archived/daemon-sketch.md) — Historical: original two-process sketch (MCP server + filesystem watcher) with triage UX, USB workflow, alternatives considered. Superseded by daemon-design-spec.md.
- [plans/triage-find-audit-improvements.md](plans/triage-find-audit-improvements.md) — Four improvements: smarter triage canonical selection, directory-level duplicate grouping, audit summary mode, path/extension search in find.
- [plans/smriti-improvement-plan.md](plans/smriti-improvement-plan.md) — Implementation plan for the seven candidates in `smriti-overall-refactor.md`, batched into 5 waves with verification steps and design-decision options.
- [adr/0001-single-writer-watcher.md](adr/0001-single-writer-watcher.md) — `index.db` has exactly one writer process; Serve is read-only.
- [adr/0002-db-only-coordination.md](adr/0002-db-only-coordination.md) — Watcher and Serve communicate only via SQLite tables; no sockets or signals.
- [adr/0003-audit-db-separate.md](adr/0003-audit-db-separate.md) — `read_audit` lives in its own `audit.db` to preserve the single-writer invariant on `index.db`.
- [adr/0004-one-path-per-transaction.md](adr/0004-one-path-per-transaction.md) — Each path's state change commits atomically; multi-path batches must be safe to abort partway.
