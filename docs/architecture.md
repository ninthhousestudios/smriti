# Architecture

## Overview

smriti is a single Rust binary that operates in two modes: CLI commands for
direct use, and a daemon mode that serves the Model Context Protocol (MCP)
over stdio. Both modes share the same core libraries and a single SQLite
database.

```
┌─────────────────────────────────────────────────────┐
│                    smriti binary                     │
│                                                     │
│  ┌──────────┐    ┌──────────┐    ┌──────────────┐   │
│  │  CLI      │    │  MCP     │    │  Scanner     │   │
│  │  (clap)   │    │  daemon  │    │              │   │
│  │           │    │  (rmcp)  │    │  walk → diff │   │
│  └─────┬─────┘    └─────┬────┘    │  → classify  │   │
│        │                │         │  → hash      │   │
│        │                │         │  → emit      │   │
│        │                │         └──────┬───────┘   │
│        │                │                │           │
│  ┌─────┴────────────────┴────────────────┴───────┐   │
│  │              Core libraries                    │   │
│  │  db · search · privacy · metadata · ignore     │   │
│  │  config · embedding(opt) · roots               │   │
│  └─────────────────────┬─────────────────────────┘   │
│                        │                             │
│  ┌─────────────────────┴─────────────────────────┐   │
│  │           SQLite (WAL mode)                    │   │
│  │  documents · paths · events · catalog          │   │
│  │  document_fts · document_vectors(opt)          │   │
│  │  read_audit · scan_runs                        │   │
│  └───────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────┘
```

## Modules

### main.rs — CLI entry point

Parses commands via clap and dispatches to the appropriate handler. Each command
opens a fresh database connection, does its work, and exits. No persistent
state between invocations.

### scanner.rs — the scan engine

The largest module (~1600 lines). Two implementations:

- **`scan_legacy`** — wraps the entire scan in a single SQLite transaction.
  Simple but produces massive WAL files on large directories (300MB+ for 50k
  files) and holds a write lock for the full duration.
- **`scan_batched`** — commits every N files (default 500). Each batch is its
  own transaction. Produces smaller WAL pressure, allows concurrent readers
  between batches, and updates `scan_runs` with progress. Gated behind
  `SMRITI_SCAN_BATCHED=1` while validating.

Both implementations follow the same pipeline:

1. **Load previous state** — read all live paths and their hashes from the
   database into memory.
2. **Walk** — traverse each root with `walkdir`, respecting symlink policy
   (don't follow).
3. **Classify** — at each path, consult the ignore stack (hardened defaults +
   per-directory `.smritiignore` files) to determine tier.
4. **Short-circuit** — if mtime and size haven't changed, skip rehashing.
5. **Hash** — BLAKE3 the file content. For text files, also compute a body
   hash (content minus frontmatter) to detect minor changes.
6. **Diff** — compare against previous state to determine the event type
   (created, updated, moved, copied, deleted, hardlinked, minor_change).
7. **Persist** — write documents, paths, events, and catalog entries.
8. **Finalize** — mark disappeared paths, detect moves/copies across the full
   scan, clean up orphaned FTS entries.

### db.rs — database lifecycle

Opens SQLite in WAL mode with a 5-second busy timeout. Runs migrations on
every open (idempotent). Provides `checkpoint_wal()` for pre-scan WAL
truncation (prevents SIGBUS from stale WAL frames after crashes).

### ignore.rs — .smritiignore parser

Parses gitignore-syntax files into three compiled matchers (via the `ignore`
crate):

- **ignored** — skip entirely
- **cataloged** — tier 2 only
- **no_embed** — tier 1 but suppress dense embedding

Hardened defaults are compiled into the binary from `ignore_defaults.txt`.
Per-directory files are discovered during the walk and pushed/popped onto an
`IgnoreStack` as the scanner descends and ascends the tree.

### privacy.rs — the privacy gate

`PrivacyGate` enforces two rules before any file read:

1. The path must be under an allowlisted root.
2. The path must not match any ignore rules.

Every successful read is logged to the `read_audit` table with timestamp,
content hash, and the requesting agent. This is the intended file access
layer for AI agents — they should use `smriti_read` instead of direct
filesystem reads.

### search.rs — BM25 and hybrid search

- **`search_fts`** — BM25 ranking via SQLite FTS5. Searches title, topics,
  summary, and content columns.
- **`search_hybrid`** — runs both BM25 and dense vector search (cosine
  similarity via `sqlite-vec`), then merges results with Reciprocal Rank
  Fusion. Requires the `embedding` feature.
- **`health`** — assembles the status report (doc count, last scan, embedder
  availability, roots).

### metadata.rs — file metadata extraction

Extracts title (first heading or filename), summary (first paragraph), topics
(headings list), and document structure from file content. Detects binary
files. Computes file extension and MIME type.

### embedding.rs — dense embeddings (optional)

BGE-M3 ONNX model for 1024-dimensional dense embeddings. Gated behind the
`embedding` Cargo feature. Provides an `Embedder` that tokenizes and runs
inference, and `upsert_embedding` / `search_dense` for the `document_vectors`
virtual table.

### mcp.rs — MCP server

Implements the `rmcp` `ServerHandler` trait. Each MCP tool maps to a core
library function with the same name pattern. All responses include a freshness
envelope (`as_of` timestamp, `is_stale` boolean) so clients know how current
the data is.

### config.rs — configuration

`Config::from_env()` reads all `SMRITI_*` environment variables with sensible
defaults. `dotenvy` loads `.env` files automatically. No config file format —
environment variables are the only interface.

### roots.rs — root management

Roots are stored in `~/.smriti/roots` as one path per line. CLI commands
`roots add/remove/list` manage this file. `SMRITI_ROOTS` env var can override.
`load_roots` merges both sources and validates that paths exist.

## Database schema

```
documents
├── content_hash TEXT PK    -- BLAKE3 of file content
├── body_hash TEXT          -- BLAKE3 of content minus frontmatter (nullable)
├── title TEXT
├── summary TEXT
├── topics TEXT             -- JSON array
├── structure TEXT          -- JSON array of {heading, level, line}
├── is_binary BOOLEAN
├── embed_excluded BOOLEAN
├── byte_size INTEGER
└── first_seen TEXT

paths
├── id INTEGER PK
├── content_hash TEXT FK    -- → documents.content_hash
├── path TEXT
├── root TEXT
├── is_hardlink BOOLEAN
├── mtime INTEGER
├── size_bytes INTEGER
├── appeared TEXT
├── disappeared TEXT        -- NULL while file exists
└── last_seen_scan INTEGER  -- scan generation FK → scan_runs.id

events
├── id INTEGER PK
├── event_type TEXT         -- created|updated|moved|copied|deleted|hardlinked|minor_change
├── content_hash TEXT
├── path TEXT
├── timestamp TEXT
├── file_extension TEXT
├── mime_type TEXT
└── scan_id INTEGER

catalog
├── path TEXT PK
├── total_bytes INTEGER
├── file_count INTEGER
├── regenerable BOOLEAN
└── last_scanned TEXT

scan_runs
├── id INTEGER PK
├── started_at TEXT
├── finished_at TEXT
├── status TEXT             -- running|completed|failed
├── files_seen INTEGER
└── error TEXT

document_fts (FTS5 virtual table)
├── content_hash
├── title
├── topics
├── summary
└── content

document_vectors (sqlite-vec virtual table, optional)
├── content_hash TEXT
└── embedding FLOAT[1024]

read_audit
├── id INTEGER PK
├── timestamp TEXT
├── content_hash TEXT
├── path TEXT
└── agent TEXT
```

## Data flow: scan

```
roots.list
    │
    ▼
 WalkDir ──► IgnoreStack.classify() ──► Ignored (skip)
                  │            │
                  │            └──► Cataloged ──► catalog table
                  │
                  ▼
              Tier 1
                  │
        mtime+size changed?
           │           │
           no          yes
           │           │
     short-circuit   BLAKE3 hash
           │           │
           └─────┬─────┘
                 │
           diff vs prev_paths
                 │
        ┌────────┼────────┐
        ▼        ▼        ▼
    new path  same path  path gone
        │     new hash       │
        │        │           │
  ┌─────┴──┐     │     ┌────┴────┐
  │ hash   │  updated/  │ hash    │
  │ seen?  │  minor    │ seen    │
  │        │  change   │ elsewhere│
  no  yes  │           │         │
  │    │   │          no    yes  │
  ▼    ▼   ▼          ▼     ▼    │
created copy         deleted moved
```

## Data flow: search

```
query string
    │
    ├──► FTS5 BM25 ──► ranked results
    │                        │
    │                   (always active)
    │
    └──► BGE-M3 embed ──► sqlite-vec cosine ──► ranked results
              │                                       │
         (optional,                              (optional)
          feature-gated)                              │
                                                      │
                              Reciprocal Rank Fusion ◄┘
                                        │
                                   merged results
```

## Data flow: privacy gate (smriti_read)

```
read request (path or content_hash)
    │
    ├──► resolve path (if hash given, look up in paths table)
    │
    ├──► check: path under an allowlisted root?
    │         no → deny
    │
    ├──► check: path matches ignore rules?
    │         yes → deny
    │
    ├──► read file from disk
    │
    ├──► log to read_audit
    │
    └──► return content + freshness envelope
```

## Concurrency model

smriti is single-writer. The scanner holds write transactions (one per batch
in batched mode, one for the full scan in legacy mode). Read-only commands
(`health`, `scan-status`, `find`, `get`) open separate connections and read
via WAL's snapshot isolation — they see committed state without blocking the
scanner.

A 5-second `busy_timeout` is set on every connection so that transient lock
contention (e.g., a reader opening during a batch commit) retries instead of
failing immediately.

`PRAGMA wal_checkpoint(TRUNCATE)` runs only before scans to collapse any
stale WAL frames from prior crashes. It is not run on read-only connections
to avoid contending with an active scan.

## Feature flags

| Feature | Cargo flag | What it adds |
|---------|-----------|--------------|
| Dense embeddings | `--features embedding` | BGE-M3 ONNX, `document_vectors` table, `search_hybrid` |
| HTTP transport | `--features http` | Axum-based HTTP server for the daemon |

Default build: BM25 search only, MCP over stdio.
