# triage, find, and audit improvements

## issues

### 1. triage: canonical selection ignores smritiignore classification

**Problem**: `format_triage_file` picks `paths[0]` (first alphabetically from `GROUP_CONCAT`) as canonical. If that path is under a cataloged directory (e.g. `~/Downloads`), it gets "keep" while the real copy elsewhere gets "catalog" — backwards.

**Fix**: In `query_duplicates` or `format_triage_file`, score each path for "canonical-ness":
- Path is under a `[catalog]` pattern in `~/.smritiignore` → low score (prefer to catalog it)
- Path is under a known regenerable dir (`.venv`, `node_modules`, etc.) → low score
- Shorter path depth → slight preference
- Pick the highest-scoring path as canonical

This requires loading `~/.smritiignore` rules inside the triage module (currently triage only reads from the DB, not ignore rules). Pass `SectionRules` into `analyze()`, or add a scoring function that takes the global rules.

**Files**: `src/triage.rs` (query_duplicates, format_triage_file), `src/main.rs` (pass rules to analyze)

### 2. triage: group duplicates by directory

**Problem**: Duplicate groups are listed individually — if 50 files in dir A are duplicated in dir B, you see 50 separate groups scattered across the file. Hard to read, hard to act on.

**Fix**: Post-process duplicate groups before formatting:
1. Build a map: `(dir_a, dir_b) → Vec<DuplicateGroup>` for groups where all paths share exactly two parent dirs
2. If a dir pair accounts for N+ files (threshold: 3?), collapse into a single "directory duplicate" entry:
   ```
   catalog    ~/Downloads/project-backup/       12.5 MB   duplicates ~/soft/project/ (47 files)
   ```
3. Remaining individual file dups get listed after directory dups, still grouped together (all dups for one content_hash in a contiguous block — this already works, the problem is they're interleaved with recommendations)

The real layout fix: **separate recommendations from duplicates in the triage file** with clear section headers. Currently `format_triage_file` does this but the duplicates section still interleaves with the recommendations section in the editor. Verify the sections have blank-line separation and that duplicates are always at the bottom, sorted by directory.

**Files**: `src/triage.rs` (format_triage_file, new directory-dedup collapsing logic)

### 3. audit: output too long, no way to drill in

**Problem**: `smriti audit` dumps everything at once. With many extensions and large tier-2 lists, the output scrolls past the terminal.
no actually it does this:
Tier 1 (indexed — back this up):
  Files: 351747
  Size:  100.6 GB
  By extension:
    .safetensors     60 files  20.8 GB
    (none)        33713 files  10.7 GB
    .png          12220 files  9.4 GB
    .zip            239 files  7.1 GB
    .iso              3 files  6.5 GB
    .eph              2 files  5.2 GB
    .db              90 files  4.0 GB
    .json          9970 files  2.8 GB
    .se1           2458 files  2.4 GB
    .onnx_data        1 files  2.1 GB
    .pack            43 files  2.1 GB
    .mp3            139 files  2.0 GB
    .bin           4101 files  2.0 GB
    .wav              6 files  1.8 GB
    .pickle         862 files  1.6 GB
    ... and 3330 more extensions

**Fix — subcommands or flags**:
```
smriti audit                    # summary only: totals, top 5 extensions, top 5 tier-2
smriti audit --full             # current behavior (all extensions, all tier-2)
smriti audit --ext .iso         # filter: show all files with this extension
smriti audit --tier2            # show only tier-2 catalog entries
```

The `--ext` flag is the one Josh actually wants right now — it overlaps with `find` but operates on path metadata rather than FTS content. Implement as a SQL query against `paths` table filtered by extension.

**Files**: `src/main.rs` (CLI args for Audit), `src/search.rs` (audit function or new query)

### 4. find: FTS-only, can't search by path/extension

**Problem**: `smriti find ".iso"` searches the FTS index (title, topics, summary, content). A bare extension like `.iso` won't match anything meaningful — FTS indexes document content, not file paths.

One question about FTS: we removed any fts content from the db because smriti is meant
to be an index not hold file content itself. is there no fts content, or is it just the
metadata, not any actual file content? that is ideal, to have metadata to be able to
look a bit into the file, but definitely not storing any file content is correct.

**Fix — add `smriti locate` (path search)**:
```
smriti locate "*.iso"           # glob against paths table
smriti locate --ext .iso        # shorthand
smriti locate "~/Downloads/**"  # everything under a dir
```

This is a simple SQL query: `SELECT path, byte_size FROM paths WHERE path LIKE ? AND disappeared IS NULL`. For glob patterns, convert to SQL LIKE or use the `glob()` SQLite function.

Alternatively, add a `--path` flag to the existing `find` command:
```
smriti find --path "*.iso"      # path glob search
smriti find "python tutorial"   # FTS search (default, unchanged)
```

I'd lean toward the `--path` flag on `find` rather than a new subcommand — fewer commands to remember.

**Files**: `src/main.rs` (CLI args for Find), `src/search.rs` (new path search function)

## implementation order

1. **find --path / --ext** — smallest change, immediately useful (Josh wants this now)
2. **audit summary mode + --ext filter** — default to compact, add drill-down
3. **triage canonical scoring** — load smritiignore rules, score paths
4. **triage directory grouping** — post-process duplicates, collapse dir pairs

## questions

None — all four are straightforward. Wave 1 (find) and wave 2 (audit) are independent and could be done in parallel.
