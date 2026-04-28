# handoff

## just completed

All 4 waves from `docs/plans/triage-find-audit-improvements.md` are implemented, tested (64/64 pass), and installed.

- `find --path` / `--ext` for path/extension search
- `audit` compact summary by default, `--full` / `--ext` / `--tier2` drill-down
- triage canonical scoring using smritiignore rules
- triage directory-pair duplicate collapsing

README.md updated with new command flags and install-from-source section.

## pick up next

- **MCP parity**: `FindParams` in `src/mcp.rs` doesn't expose `--path` / `--ext` yet. Add optional `path` and `ext` fields and wire them to `search_path` / `search_extension`.
- **audit --tier2 perf**: currently runs the full audit query even when only tier2 data is needed. Could add a separate tier2-only query if it becomes a bottleneck.
- **search_extension perf**: uses `LOWER(p.path) LIKE` which scans the full paths table. If slow on large DBs, consider a generated column or index on the extension.
- **triage testing**: no unit tests for `canonical_score` or dir-pair collapsing. Worth adding if the scoring logic gets more complex.

## context

- `SectionRules` (ignore crate's `Gitignore`) does not implement `Clone`. We added `SectionRules::classify()` as the public API instead of requiring ownership.
- Dir-pair collapsing keys on `(canonical_parent, dup_parent)` using wave-3 scoring order, not lexical. Inconsistent scoring across a pair means files stay individual (correct behavior).
