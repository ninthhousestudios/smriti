# smriti — design sketch

Status: sketch (revised 2026-04-26 incorporating opus 4.7 review — see `docs/manas-opus47-review.md`)
Date: 2026-04-26
Context: `docs/manas-architecture.md` — perception: filesystem subsystem

## what it is

Smriti (स्मृति — "that which is remembered") is a content-addressed filesystem indexer with temporal history. It tracks files by identity (content hash), not by path. It knows what you have, where it is now, where it was before, and what matters vs. what's regenerable.

Rooted at `~`. Not per-project — everything under your home directory. One index for your entire digital life.

Think of it as a bespoke git for your filesystem — content-addressed tracking, semantic indexing, and an agent-native query interface. v0.2 adds time travel (content storage + revert).

## what it is not (v0.1)

- Not a document store. Files live on the filesystem. Smriti tracks them. (v0.2 adds content storage for revert.)
- Not a search engine. It knows *what exists* and *what it's about*. Deep full-text search is a possible extension, not the core job.
- Not chitta. It doesn't store decisions, observations, or mental models. It tracks the filesystem.
- Not a backup tool. But it produces the manifest that makes backup tractable.
- **Not a free-roaming indexer.** It only indexes paths under explicitly allowlisted roots. The default scope is conservative, not "all of `~`" — see *privacy posture* below.

## first consumer: the backup problem

The immediate motivation: backing up `/home/josh` is hard because there's no way to separate what you created from what tools generated. Pacman cache, yay cache, Flutter build dirs, `node_modules`, `target/` — gigabytes of regenerable data mixed in with the stuff that actually matters.

Smriti solves this by classifying everything into two tiers. The backup manifest falls out naturally: tier 1 is "back this up," tier 2 is "don't bother, it's regenerable."

---

## core concepts

### privacy posture (default-deny)

Smriti is rooted at `~` *conceptually* but does not index `~` by default. The user's home directory contains too much sensitive material to scan opportunistically: `.ssh/`, `.gnupg/`, `.aws/credentials`, browser auth stores under `.config/<browser>/` and `.mozilla/`, password-manager dumps, `.env*` files in every project, OAuth refresh tokens scattered across tool configs.

Embedding such files into a SQLite index makes the index file itself a high-value target. Even with file permissions, BGE-M3 embeddings of short strings (API keys, tokens) can be partially recovered with model access. **Privacy is not a tier-3 concern; it shapes the schema.**

The model:

1. **Allowlisted roots.** Smriti indexes nothing until the user names roots explicitly: `~/Documents`, `~/notes`, `~/projects`, etc. Configured via `SMRITI_ROOTS` env var or a config file under `~/.smriti/`.
2. **Hardened ignore defaults.** Even within an allowlisted root, smriti ships with a fail-closed default `.smritiignore` covering known secret-bearing patterns: `.env*`, `*.pem`, `*.key`, `id_rsa*`, `*.kdbx`, `secrets.*`, `credentials*`, `.aws/`, `.gnupg/`, `.ssh/`, `cookies*`, etc. The user can relax these per-root if they really want to.
3. **Embedding gate, independent of indexing tier.** A file may be tier 1 (hashed, lifecycle-tracked, BM25-searchable on title/topic) but excluded from dense embedding via a `[no-embed]` section in `.smritiignore`. License keys, signed manifests, anything where short-string embedding leakage is a concern. Hashing is fine; vectorizing is opt-in for sensitive paths.

`smriti_audit` reports both: indexed roots (allowlist) and ignored-by-default patterns that were observed under those roots. The user sees what was excluded as well as what was kept.

### two tiers

Within allowlisted roots, files fall into one of two tiers:

**Tier 1 — Indexed.** Semantically understood. Content-hashed, lifecycle-tracked, searchable by topic. Your documents, notes, configs, source code, astrology material, project files. The stuff you'd want to *find by meaning*. The stuff you'd *lose* if the disk died.

**Tier 2 — Cataloged.** Aware of existence and size, but not deeply tracked. Build artifacts, package caches, downloaded dependencies, generated files. You don't need to search inside them, but you need to know "this directory exists, it's 4.7 GB, and it's regenerable."

A third implicit category: **Ignored.** Not even cataloged. `.git/` internals, swap files, temp files, secret-bearing patterns. Configured via `.smritiignore`.

### .smritiignore

Lives at `~/.smriti/ignore` (shipped defaults, hardened) and `~/.smritiignore` (user overrides). Additional `.smritiignore` files in subdirectories (like nested `.gitignore`). Gitignore-style glob patterns with two custom sections.

```
# fully ignored — not even cataloged
.git/
*.swp
*.tmp
*~
*.pyc

# secret-bearing patterns shipped as defaults — fail closed
.env
.env.*
*.pem
*.key
id_rsa*
*.kdbx
secrets.*
credentials*
.aws/
.gnupg/
.ssh/
**/cookies*
**/.config/google-chrome/
**/.mozilla/firefox/*/

# catalog only — track existence + size, don't index contents
[catalog]
**/node_modules/
**/target/
**/.flutter/
**/__pycache__/
**/.cache/
**/.local/share/Trash/
**/build/
**/.gradle/
**/.dart_tool/
/var/cache/pacman/
.yay/

# tier 1 but skip dense embedding — hash + path-track + BM25 only
[no-embed]
**/*.lic
**/license-keys/
**/signed-manifests/
```

Within allowlisted roots, anything not matching any rule is tier 1 (indexed) by default. Outside of allowlisted roots, smriti does nothing.

The `[catalog]` section is the key innovation over `.gitignore`. It says "I know this exists, I know it's big, but don't waste time hashing every file inside it." For catalog entries, smriti records: path, total size, file count, last modified timestamp, and a `regenerable` flag.

The `[no-embed]` section is the privacy-gate counterpart. Tier 1 still applies (hash, lifecycle, BM25 over title/topic), but no dense embedding vector is computed or stored.

### document identity

A document's identity is a blake3 hash of its **whole content**. Applies to tier 1 files only.

Earlier drafts proposed stripping YAML frontmatter before hashing, on the theory that metadata-only edits shouldn't create new identities. This turns out to conflate two things the user actually cares about distinguishing: a `status: draft → published` edit is a real change, not a no-op. Hashing whole content keeps identity honest.

To recover the "minor edit" signal that frontmatter-stripping was reaching for, smriti emits a separate `minor_change` event when (a) the new content differs from the previous version, (b) the diff is contained entirely in a recognized header block (YAML frontmatter, TOML preamble), and (c) the body hash is unchanged. The agent can filter on event type if it wants to ignore metadata churn.

This means:
- Rename `docs/principles.md` → `docs/governance/principles.md`: same document, new path. Smriti records a move event.
- Edit body of `docs/principles.md`: new version. Old identity → superseded. New identity → current. Same path. Event: `updated`.
- Edit only frontmatter of `docs/principles.md`: new identity (whole-content hash changes). Event: `minor_change`. Body hash stable so reverse-lookup tools can still group versions.
- Copy a file: two paths, same identity. Smriti tracks both.

### events

Smriti records lifecycle events for tier 1 files:

| Event | Meaning |
|---|---|
| `created` | New content hash seen for the first time at a path |
| `moved` | Same content hash, new path, old path gone |
| `updated` | Same path, new content hash, body changed |
| `minor_change` | Same path, new content hash, body unchanged (frontmatter / preamble edit) |
| `deleted` | Path gone, content hash not seen elsewhere |
| `copied` | Same content hash appears at a second path |
| `hardlinked` | Distinct from `copied` — modifying one modifies all |

Events are timestamped. This is smriti's temporal history — it owns the lifecycle of files, just as chitta owns the lifecycle of understanding.

**Upstream note for grantha:** events should carry the file extension (and ideally MIME type) so downstream consumers like grantha can filter by file type without re-statting. Cheap to include from day one.

### semantic metadata

For each tier 1 document version, smriti extracts and stores:

- **Title.** First `# heading` or filename.
- **Topics/tags.** Derived from headings, content, or explicitly declared.
- **Structure.** Section headings and their hierarchy (for structured formats like markdown).
- **Summary.** Short description. Heuristic extraction (first paragraph, preamble fields) — no LLM on the index path.

Binary files (PDFs, images) in tier 1: hashed and path-tracked, but no semantic extraction at the smriti layer. Marked `is_binary=true`. Document intelligence (per-page text extraction, page-level embeddings, citation anchoring) is the responsibility of **grantha**, a separate tool that sits on top of smriti. See `docs/grantha-sketch.md`.

### scan cycle

Smriti works by scanning allowlisted roots and diffing against its last known state:

1. For each allowlisted root, walk it, respecting `.smritiignore` rules (shipped defaults + user overrides + nested ignores).
2. For ignored paths: skip entirely.
3. For catalog paths: record path, total size, file count, last modified.
4. For tier 1 paths: **mtime+size short-circuit first** — if the path's `(mtime, size)` matches the last snapshot, reuse the prior content_hash without re-reading the file. Only re-hash when `(mtime, size)` disagree.
5. For files that need (re)hashing: blake3 the whole content.
6. Emit events for tier 1 changes (`created`, `moved`, `updated`, `minor_change`, `deleted`, `copied`, `hardlinked`).
7. Extract/update semantic metadata for new/changed tier 1 files. Skip dense embedding for files matching `[no-embed]`.
8. Store new snapshot.

The mtime+size short-circuit is not an optimization — it is a v0.1 requirement. A real `~` allowlist (a few hundred thousand tier-1 files across `~/Documents`, `~/projects`, `~/notes`) re-hashed on every scan takes minutes. Indexes go stale, the system loses trust. Re-hash only on disagreement.

Trigger options:
- **On demand.** Agent or user calls `smriti_scan`. The default in v0.1.
- **Session-end hook.** `/done` enqueues a rescan. The scan runs *async* in the smriti daemon (see *transport* below); `/done` does not block on it.
- **Filesystem watch.** `inotify`. Most responsive, most complexity. v0.1.5 / v0.2.
- **Periodic.** Timer-based. Middle ground.

v0.1 ships on-demand + /done-enqueued. Progress reporting matters: the scan tool reports incrementally and `smriti_health` exposes a `scan_in_progress` flag with ETA when one is running.

---

## storage

SQLite with **`sqlite-vec`** for vector ANN and **FTS5** for BM25. Both are SQLite extensions / virtual tables, so the "single inspectable DB file" property is preserved — the human can still `sqlite3 ~/.smriti/index.db ...` and read everything. No external services, no separate index directories (rules out tantivy).

The human can `sqlite3 ~/.smriti/index.db "SELECT path, byte_size FROM documents JOIN paths USING(content_hash) WHERE disappeared IS NULL"` to see all current files.

### tables (sketch)

```sql
-- each unique file version (tier 1)
CREATE TABLE documents (
    content_hash TEXT PRIMARY KEY,
    body_hash TEXT,          -- hash of body without frontmatter; powers minor_change detection
    title TEXT,
    summary TEXT,
    structure TEXT,          -- JSON: section headings hierarchy
    topics TEXT,             -- JSON: extracted topics/tags
    embed_excluded BOOLEAN NOT NULL DEFAULT FALSE,  -- set when path matched [no-embed]
    embedding_model TEXT,    -- e.g. "bge-m3-v1"; NULL if no embedding stored
    is_binary BOOLEAN NOT NULL DEFAULT FALSE,
    first_seen TIMESTAMP NOT NULL,
    byte_size INTEGER
);

-- vector ANN via sqlite-vec virtual table (separate, joined by content_hash)
CREATE VIRTUAL TABLE document_vectors USING vec0(
    content_hash TEXT PRIMARY KEY,
    embedding FLOAT[1024]
);

-- BM25 over title + topics + summary (and content for small files) via FTS5
CREATE VIRTUAL TABLE document_fts USING fts5(
    content_hash UNINDEXED,
    title,
    topics,
    summary,
    content
);

-- where tier 1 files live (and lived)
CREATE TABLE paths (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    content_hash TEXT NOT NULL REFERENCES documents(content_hash),
    path TEXT NOT NULL,      -- relative to the allowlisted root
    root TEXT NOT NULL,      -- which allowlisted root this path belongs to
    is_hardlink BOOLEAN NOT NULL DEFAULT FALSE,  -- distinguishes hardlinks from independent copies
    mtime TIMESTAMP NOT NULL,    -- powers mtime+size short-circuit on next scan
    size_bytes INTEGER NOT NULL,
    appeared TIMESTAMP NOT NULL,
    disappeared TIMESTAMP,   -- NULL = still here
    UNIQUE(content_hash, path, appeared)
);
CREATE INDEX idx_paths_path ON paths(path);
CREATE INDEX idx_paths_disappeared ON paths(disappeared) WHERE disappeared IS NULL;

-- lifecycle events (tier 1)
CREATE TABLE events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    event_type TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    path TEXT NOT NULL,
    previous_hash TEXT,
    previous_path TEXT,
    timestamp TIMESTAMP NOT NULL
);
CREATE INDEX idx_events_hash ON events(content_hash);
CREATE INDEX idx_events_path ON events(path);
CREATE INDEX idx_events_ts ON events(timestamp);

-- cataloged directories (tier 2)
CREATE TABLE catalog (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT NOT NULL,
    total_bytes INTEGER NOT NULL,
    file_count INTEGER NOT NULL,
    last_modified TIMESTAMP,
    regenerable BOOLEAN NOT NULL DEFAULT TRUE,
    last_scanned TIMESTAMP NOT NULL,
    UNIQUE(path)
);

-- scan state
CREATE TABLE snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp TIMESTAMP NOT NULL,
    tier1_files_scanned INTEGER,
    tier2_dirs_cataloged INTEGER,
    events_emitted INTEGER,
    duration_ms INTEGER
);
```

### why SQLite (with extensions)

Chitta uses Postgres because it needs pgvector, concurrent MCP access, and FTS with GIN indexes. Smriti needs single-writer (the daemon) and many-readers (whatever MCP clients) on a local DB. SQLite + WAL mode handles that. `sqlite-vec` gives us ANN; `FTS5` is built in for BM25. The footprint stays a single inspectable file.

DB location: `~/.smriti/index.db`.

---

## MCP tools

All prefixed `smriti_`. Designed for agent consumption. Token-efficient. Follows the envelope pattern from chitta.

**Freshness envelope.** Every read tool's response carries an `as_of` timestamp (the `last_scan` time the result was derived from) and an `is_stale` boolean (true if `as_of` is older than a configurable threshold, default 1 hour, or if a scan is currently in progress). The agent can weight stale results accordingly. Without this, callers get paths confidently when they may be hours out of date.

**Note on mcpjungle.** Smriti is fronted by mcpjungle in the manas system. mcpjungle currently does not proxy MCP resource templates or `resources/subscribe` notifications, and Tool Groups do not group-scope resources. Smriti therefore exposes file content via a **tool** (`smriti_read`), not parameterized MCP resources, and emits change notifications via the manas-cli sideband daemon rather than MCP subscriptions.

### smriti_scan

Trigger a scan cycle. Returns summary of changes since last scan.

```
Input:  { paths?: [string] }          -- override: scan only these subtrees
Output: {
    tier1: { created: int, moved: int, updated: int, deleted: int, total: int },
    tier2: { cataloged: int, total: int },
    scan_duration_ms: int
}
```

### smriti_find

Semantic query → matching files with current paths. Tier 1 only.

```
Input:  { query: string, k?: int }
Output: {
    results: [{
        path: string,
        title: string,
        summary: string,
        topics: [string],
        content_hash: string,
        byte_size: int,
        embed_excluded: bool   -- true if hit came from BM25 only (file is in [no-embed])
    }],
    total_indexed: int,
    as_of: timestamp,
    is_stale: bool
}
```

Search has two legs:

1. **BM25** via SQLite FTS5 over title + topics + summary + content. Always available.
2. **Dense embeddings** via BGE-M3 ONNX, stored in the `document_vectors` virtual table (sqlite-vec). Enabled when `SMRITI_MODEL_PATH` is configured. When available, runs both legs and merges via RRF. Files matching `[no-embed]` contribute to the BM25 leg only.

If the model file isn't configured, degrades to BM25 only.

### smriti_get

Direct lookup by content_hash. Distinct from `smriti_find` because semantics differ — this is a get, not a search.

```
Input:  { content_hash: string }
Output: {
    path: string | null,        -- current canonical path, NULL if all paths disappeared
    all_current_paths: [string], -- includes copies and hardlinks
    title: string,
    summary: string,
    topics: [string],
    content_hash: string,
    byte_size: int,
    as_of: timestamp,
    is_stale: bool
}
```

### smriti_read

Read tier-1 file content through smriti's privacy gate. Required because the agent must not bypass `.smritiignore` exclusions by using built-in filesystem tools — files matching the secret-bearing defaults or `[no-embed]` should still be readable here only if they are tier 1 and within an allowlisted root, and the read is logged.

```
Input:  {
    path?: string,            -- one of path or content_hash required
    content_hash?: string,
    line_range?: [int, int]   -- optional: 1-indexed inclusive
}
Output: {
    path: string,
    content_hash: string,
    content: string,          -- text content (or base64 for binary)
    is_binary: bool,
    truncated: bool,          -- true if line_range was applied
    as_of: timestamp,
    is_stale: bool
}
```

Files outside allowlisted roots, files matching ignored patterns, and files in tiers other than tier 1 return an error. The smriti daemon logs every read for auditability. This is the privacy gate — if the agent is told "do not read `.env` files," that policy is enforced here, not at the prompt layer.

### smriti_map

Overview of tracked files and cataloged directories.

```
Input:  { path_prefix?: string, tier?: "indexed" | "cataloged" | "all" }
Output: {
    indexed: [{
        path: string,
        title: string,
        topics: [string],
        last_updated: timestamp
    }],
    cataloged: [{
        path: string,
        total_bytes: int,
        file_count: int,
        regenerable: bool,        -- as declared by [catalog] rule, not verified
        regenerable_verified: false  -- always false in v0.1; honest signal
    }],
    total_indexed: int,
    total_cataloged: int,
    as_of: timestamp,
    is_stale: bool
}
```

### smriti_outline

Structure of a single document (tier 1).

```
Input:  { path: string }               -- or content_hash
Output: {
    path: string,
    title: string,
    summary: string,
    sections: [{
        heading: string,
        level: int,
        line: int
    }],
    topics: [string],
    byte_size: int,
    content_hash: string,
    as_of: timestamp,
    is_stale: bool
}
```

### smriti_history

Lifecycle of a file. What makes smriti more than a static index.

```
Input:  { path: string, since?: timestamp, until?: timestamp }   -- or content_hash
Output: {
    current_path: string,
    content_hash: string,
    events: [{
        event_type: string,
        timestamp: timestamp,
        path: string,
        previous_path?: string,
        previous_hash?: string
    }],
    versions: int,
    as_of: timestamp,
    is_stale: bool
}
```

### smriti_audit

The backup manifest. This is the first killer feature.

```
Input:  {
    min_bytes?: int,            -- only show entries above this size
    sort_by?: "size" | "count"  -- sort cataloged entries
}
Output: {
    tier1: {
        total_files: int,
        total_bytes: int,
        by_extension: { ".md": { files: int, bytes: int }, ... }   -- weak proxy; topic-based audit comes later
    },
    tier2: {
        total_dirs: int,
        total_bytes: int,
        largest: [{ path: string, total_bytes: int, file_count: int, regenerable: bool }]
    },
    ignored_estimate_bytes: int,  -- rough estimate of ignored content (including secret-bearing defaults)
    excluded_from_embedding: {
        files: int,
        bytes: int                 -- how much tier-1 content is search-excluded for privacy
    },
    roots: [string],              -- the allowlisted roots that defined scope
    backup_target_bytes: int,     -- tier1 total = what you'd actually back up
    as_of: timestamp,
    is_stale: bool
}
```

### smriti_manifest

Bulk export of tier-1 paths for backup tooling. Distinct from `smriti_audit` (which summarizes) — this is the list, suitable for piping to rsync or restic.

```
Input:  { format?: "paths" | "ndjson" }       -- default "paths"
Output: {
    format: string,
    entries: [string]              -- absolute paths if format=paths; one JSON-per-line objects if ndjson
                                   -- ndjson form: { path, content_hash, byte_size }
    as_of: timestamp,
    is_stale: bool
}
```

### smriti_health

Health check.

```
Input:  {}
Output: {
    status: "ok" | "degraded",
    db_path: string,
    roots: [string],                -- allowlisted roots being indexed
    total_indexed: int,
    total_cataloged: int,
    last_scan: timestamp,
    scan_in_progress: bool,
    scan_eta_seconds?: int,         -- present when scan_in_progress
    embedder_ok: bool,              -- whether dense embedding leg is available
    embedding_model: string | null,
    version: string
}
```

---

## configuration

Env vars, consistent with chitta-rs pattern.

| Var | Default | Purpose |
|---|---|---|
| `SMRITI_DB_PATH` | `~/.smriti/index.db` | SQLite database location |
| `SMRITI_ROOTS` | *(none — must be set)* | Colon-separated list of allowlisted roots. Smriti indexes nothing if unset. Example: `~/Documents:~/projects:~/notes` |
| `SMRITI_MODEL_PATH` | *(none)* | Path to BGE-M3 ONNX model. Enables dense embeddings. |
| `SMRITI_LISTEN_ADDR` | `unix:~/.smriti/sock` | Daemon listen address. Unix socket by default; `tcp://127.0.0.1:NNNN` for HTTP transport. |
| `SMRITI_STALE_THRESHOLD_SEC` | `3600` | `is_stale` threshold for read envelopes. |

Path filtering is handled by `.smritiignore` files, not env vars. Ignore files are more expressive (glob patterns, nesting, `[catalog]` and `[no-embed]` sections) and version-controllable. Hardened defaults ship at `~/.smriti/ignore`; user overrides go in `~/.smritiignore`.

### transport — daemon, not stdio-per-CC

Smriti runs as a **long-lived background daemon**, not a stdio process spawned per Claude Code instance. Two reasons:

1. **Scan latency.** A real allowlist (a few hundred thousand tier-1 files) takes time to scan. /done cannot block on it; the daemon handles scans asynchronously and tools query the latest snapshot.
2. **Single-writer / many-readers.** Multiple agent sessions, scripts, manas-cli all need read access concurrently. SQLite WAL handles many readers + one writer; the daemon owns the writer.

The daemon exposes MCP over Unix socket (or HTTP, configurable) and is fronted by mcpjungle in the manas system. Startup is launched by manas-cli or systemd user unit; CC clients never spawn smriti directly.

A small CLI surface (`smriti scan`, `smriti audit`, `smriti roots add ...`) exists for human use, talking to the same daemon over the same socket.

---

## move detection algorithm

The core trick. When scanning tier 1 files:

1. Build a map of `{content_hash → [path]}` from the current filesystem.
2. Compare to the previous snapshot's `{content_hash → [path]}`.
3. For each content_hash:
   - Same hash, same path → no change.
   - Same hash, different path, old path gone → **move**.
   - Same hash, different path, old path still exists → **copy**.
   - New hash at existing path → **update** (content changed).
   - New hash at new path → **create**.
   - Old hash at old path, not in new scan → **delete**.

Edge case: file moved AND content changed in the same scan cycle. Looks like a delete + create (different hash, different path).

Approach: fuzzy match — if a deleted path and a created path share the same filename, treat as move+update (emit both events). Often right. When it's wrong, history is still accurate (records delete + create instead of move+update), so no data loss from a false negative.

### hardlinks vs copies

Two filesystem paths can point to the same content for two different reasons:

- **Copy.** Independent files that happen to have the same content. Modifying one does not modify the other.
- **Hardlink.** Same inode under two paths. Modifying one modifies all.

Smriti distinguishes these by checking the inode (`st_ino`) on Unix-like systems. Both record as multiple rows in `paths`, but the hardlink case sets `is_hardlink=true` and emits the `hardlinked` event. Agents reasoning about "if I change this file, what else changes?" need this distinction.

### symlinks

Smriti **does not follow symlinks by default**. Following symlinks under an allowlisted root can escape into `/etc`, `/var`, or other allowlisted roots through user-created links, defeating both the allowlist and the secret-bearing-pattern defaults. Symlinks are recorded as their own filesystem entries (path + link target as data), not as the file they point to. A future config option may allow opt-in following per allowlisted root.

---

## project structure

```
smriti/
├── Cargo.toml
├── src/
│   ├── main.rs           -- CLI entry: daemon | scan | audit | roots | init
│   ├── daemon.rs         -- long-lived process, socket listener, scan scheduler
│   ├── mcp.rs            -- MCP server + tool handlers (talks to daemon state)
│   ├── scanner.rs        -- walk + mtime+size short-circuit + diff + tier classification
│   ├── ignore.rs         -- .smritiignore parsing ([catalog], [no-embed] sections, hardened defaults)
│   ├── hasher.rs         -- blake3 whole-content hashing, body-vs-frontmatter detection for minor_change
│   ├── metadata.rs       -- title/topic/structure extraction
│   ├── embedding.rs      -- BGE-M3 ONNX (optional, gated on model path AND [no-embed] match)
│   ├── search.rs         -- FTS5 BM25 + sqlite-vec dense retrieval, RRF merge
│   ├── privacy.rs        -- allowlist enforcement + read audit log (smriti_read gate)
│   ├── db.rs             -- SQLite operations (incl. sqlite-vec, FTS5)
│   ├── roots.rs          -- allowlisted-roots config management
│   └── config.rs         -- env var parsing
├── migrations/
│   └── 0001_initial.sql
└── tests/
    ├── scan_test.rs
    ├── ignore_test.rs
    ├── move_detection_test.rs
    ├── privacy_test.rs           -- allowlist + read-audit + secret-pattern coverage
    └── mtime_shortcircuit_test.rs
```

~12 source files. The ignore parser, scanner, and privacy gate are the most interesting new code; embedding/search reuse patterns from chitta-rs.

---

## what this enables

### immediate (v0.1)

1. **Backup audit.** `smriti_audit` → "you have 2.3 GB of stuff that matters, and 47 GB of regenerable artifacts. Here's the breakdown." First time Josh can actually see what needs backing up.
2. **Backup manifest.** `smriti_manifest` → list of tier-1 paths suitable for piping into rsync, restic, or borg.
3. **Find anything.** `smriti_find("astrology notes")` → current paths, even if they moved.
4. **Reorganize freely.** Move files around, next scan detects all moves. No broken references anywhere.
5. **What changed?** `smriti_history` → lifecycle of any file. When was it created, where has it been, what versions existed.
6. **Privacy-gated reads.** `smriti_read` becomes the agent's preferred path for file content; built-in filesystem reads bypass smriti's exclusions and should be deprecated in CLAUDE.md guidance.
7. **Agent perception.** `/reflect` and other skills can query smriti instead of walking the filesystem. More reliable, less token burn.
8. **`document_ref` removed from chitta.** Chitta stores *knowledge about* files as observations/decisions. Smriti handles *awareness of* files as artifacts.

### v0.2 — content storage and revert

v0.1 tracks lifecycle but doesn't store content. v0.2 adds the time travel.

**Content blob store.** When a file is updated or deleted, store the previous content:

```sql
CREATE TABLE blobs (
    content_hash TEXT PRIMARY KEY REFERENCES documents(content_hash),
    content BLOB NOT NULL,
    compressed BOOLEAN NOT NULL DEFAULT TRUE  -- zstd compression
);
```

Configurable retention (keep last N versions, or all versions for paths matching a pattern).

**Revert tool.** `smriti_revert`:

```
Input:  { path: string, content_hash: string }
Output: { success: bool, path: string, content_hash: string }
```

The agent asks "what were the previous versions?" via `smriti_history`, picks one, and reverts. The revert is a filesystem write + a new scan event.

This is smriti fully realized — not just remembering, but restoring what was remembered.

---

## resolved

- **Name:** smriti (स्मृति — "that which is remembered"). Loose metaphor; not a strict Vedantic mapping.
- **Scope:** allowlisted roots under `~`, not all of `~`. One index across multiple roots, but never opportunistic.
- **Privacy posture:** default-deny via allowlisted roots; hardened secret-pattern defaults; `[no-embed]` section for sensitive tier-1 content.
- **Rust.** Consistent with chitta-rs. Small binary, fast.
- **Separate repo** from chitta-rs.
- **Blake3** for whole-content hashing. No frontmatter stripping; `minor_change` event covers metadata-only edits.
- **Two tiers:** indexed (semantic, hashed, tracked) and cataloged (existence + size only).
- **.smritiignore** with gitignore-style patterns + `[catalog]` + `[no-embed]` sections.
- **Hardened ignore defaults** ship at `~/.smriti/ignore`, covering known secret-bearing patterns.
- **Large binaries** in tier 1: hash + path tracked, no semantic extraction.
- **smriti_get** for content_hash lookup (split from smriti_find).
- **smriti_read** as the privacy gate for file content. Built-in filesystem reads bypass policy and should be discouraged via CLAUDE.md.
- **smriti_manifest** for bulk tier-1 path export.
- **Daemon transport** (Unix socket by default), not stdio-per-CC. Fronted by mcpjungle.
- **mtime+size short-circuit** in scanner — required, not optimization.
- **SQLite + sqlite-vec + FTS5.** No tantivy, no separate index files.
- **Fuzzy match** for move+edit detection.
- **Hardlinks tracked separately from copies** (via inode check, `is_hardlink` flag, `hardlinked` event).
- **Symlinks not followed by default** — recorded as link entries.
- **BGE-M3 embeddings** as opt-in shared tooling, gated additionally by `[no-embed]`.
- **`smriti_history` supports `since`/`until` time-range filtering.**
- **Freshness envelope** (`as_of`, `is_stale`) on every read tool.
- **First consumer:** backup audit + backup manifest.
- **Aion library use case** handled by grantha (separate tool on top of smriti), not by extending smriti. Smriti provides file awareness; grantha provides document intelligence. See `docs/grantha-sketch.md`.

## open questions

- Should the `catalog` table track whether a cataloged directory has grown/shrunk between scans? Useful for "your Flutter cache grew 2 GB since last week."
- MCP server instructions: what should smriti tell the agent about itself in the server handshake? (Likely: "use smriti_read in preference to built-in file reads; secrets are gated.")
- **Embedding model versioning.** If BGE-M3 is upgraded, every existing embedding silently becomes stale. The `documents.embedding_model` column captures the version, but what's the rebuild policy — wipe and re-embed all on upgrade, or keep both?
- **Multi-machine.** `~/Documents` synced via Syncthing/Dropbox across two machines = two divergent smriti indexes pointing at the same hashes. Out of scope for v0.1; revisit when it's a real workflow.
- **Audit by topic, not extension.** Once embeddings are in, audit-by-topic ("you have 1.2 GB tagged 'astrology'") is the real win. Frame extension audit as a v0.1 stopgap.
- **`/reflect` integration.** What exactly does `/reflect` ask smriti for? Probably: "files changed since last reflect" + topic clustering. Spec when `/reflect` is designed.
- **Read audit log retention.** `smriti_read` logs every read for auditability. How long is the log kept? Where? Probably in the same DB; need a retention policy.
- **Watcher recovery.** When inotify is added, what's the on-disk state if the watcher process dies mid-event? Need a "last clean checkpoint" semantic.
- **Crash mid-scan.** What's the snapshot semantic if the scanner is killed at file 50000 of 200000? Probably: snapshots are atomic on commit; partial scans don't poison the next snapshot.
- **Downstream consumer subscription.** Grantha (and potentially other tools) needs to learn about file events without polling. The daemon model supports this — options include a callback registration API (`smriti_subscribe`), a Unix socket event stream, or a simple table of pending events that consumers poll and ack. Design when grantha gets real, but the daemon architecture shouldn't make it hard.
