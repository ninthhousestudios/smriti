# smriti

*smriti* (स्मृति — "that which is remembered") is a content-addressed filesystem
indexer. It tracks files by content identity (BLAKE3 hash), not by path, so it
detects renames, copies, moves, and edits across scans.

The first-order problem it solves: separating what you *created* from what tools
*generated* on a large home directory, so backup tools can target only what
matters.

smriti is the filesystem perception subsystem of
[manas](https://github.com/josharp/manas) (मनस् — "mind"), a personal AI
operating system.

## What it does

- **Content-addressed indexing** — BLAKE3 hashes every tracked file. Identical
  content at different paths shares a single document record.
- **Lifecycle events** — emits `created`, `updated`, `moved`, `copied`,
  `deleted`, `hardlinked`, and `minor_change` (frontmatter-only edit) events.
- **Two-tier classification** — every path under an allowlisted root is either
  *indexed* (tier 1: hashed, metadata extracted, FTS-searchable) or *cataloged*
  (tier 2: existence + size only). Secrets and noise are ignored entirely.
- **Full-text search** — BM25 via SQLite FTS5 over title, topics, summary, and
  file content.
- **Privacy-first** — nothing is indexed until you explicitly add roots.
  Hardened defaults block `.env`, `*.pem`, `*.key`, `id_rsa*`, `*.kdbx`,
  `.ssh/`, `.gnupg/`, browser auth stores, and more.
- **Backup manifest** — `smriti manifest` exports tier-1 paths for rsync,
  restic, borg, or any tool that takes a file list.
- **MCP server** — `smriti daemon` runs a Model Context Protocol server over
  stdio for editor and AI agent integration. All queries go through a privacy
  gate with a read audit log.

## What it doesn't do

- **No daemon/watcher (yet)** — smriti scans on demand. There is no file
  watcher or background service that reacts to changes in real time.
- **No cloud sync** — everything is local. The SQLite database lives on your
  machine.
- **No content storage** — smriti indexes metadata and hashes; it does not copy
  or deduplicate file contents. Files stay where they are.
- **No semantic/vector search by default** — dense embeddings (BGE-M3) are
  behind a feature flag and require a model download. Out of the box, search is
  BM25 only.

## Install

```bash
cargo install --path .
```

With dense embeddings:

```bash
cargo install --path . --features embedding
```

## Quick start

```bash
# Initialize the database
smriti init

# Add directories to index
smriti roots add ~/Documents
smriti roots add ~/projects

# Run a scan
smriti scan

# Search
smriti find "rust error handling"

# Check what you should back up
smriti audit

# Export a file list for your backup tool
smriti manifest | rsync -av --files-from=- / /mnt/backup/
```

## Commands

| Command | Description |
|---------|-------------|
| `smriti init` | Create the database and config directory |
| `smriti scan [--paths <path>...]` | Scan allowlisted roots (or specific subtrees) |
| `smriti scan-status` | Show status of the most recent scan |
| `smriti find <query> [-k <n>]` | Full-text search (default k=10) |
| `smriti get <content_hash>` | Look up a document by its BLAKE3 hash |
| `smriti history <path>` | Show lifecycle events for a file |
| `smriti audit [--min-bytes <n>]` | Backup audit: tier 1 vs tier 2 breakdown |
| `smriti manifest [--format paths\|ndjson]` | Export tier-1 paths for backup tools |
| `smriti roots add/remove/list` | Manage allowlisted roots |
| `smriti prune [--older-than <duration>]` | Clean up old events (default: 30 days) |
| `smriti health` | Database status, roots, last scan |
| `smriti daemon` | Run the MCP server over stdio |

## Configuration

All configuration is via environment variables. A `.env` file in the working
directory is loaded automatically.

| Variable | Default | Purpose |
|----------|---------|---------|
| `SMRITI_DB_PATH` | `~/.smriti/index.db` | Database location |
| `SMRITI_ROOTS` | *(none)* | Colon-separated roots, e.g. `~/Documents:~/notes` |
| `SMRITI_FTS_CONTENT_MAX_BYTES` | `102400` | Max bytes of content indexed per file |
| `SMRITI_MAX_METADATA_BYTES` | `524288000` | Files larger than this skip metadata extraction |
| `SMRITI_SCAN_BATCH_SIZE` | `500` | Files per batch commit |
| `SMRITI_STALE_THRESHOLD_SEC` | `3600` | Freshness threshold for MCP responses |
| `SMRITI_AUDIT_RETENTION_DAYS` | `30` | Read audit log retention |
| `SMRITI_MODEL_PATH` | *(none)* | Path to BGE-M3 ONNX model (enables dense search) |

## .smritiignore

`.smritiignore` files use gitignore syntax with three sections:

```gitignore
# Default section — fully ignored
*.log
my-secrets/

[catalog]
# Tier 2 — tracked but not content-indexed
**/build/
**/dist/

[no-embed]
# Tier 1 but skip dense embedding
**/*.min.js
```

Place them in any directory under a root. They apply to that subtree, just like
`.gitignore`. Hardened defaults are compiled into the binary and always active.

## MCP server

`smriti daemon` exposes these tools over MCP stdio:

| Tool | Description |
|------|-------------|
| `smriti_scan` | Trigger a scan, returns change summary |
| `smriti_find` | Full-text search |
| `smriti_get` | Document lookup by hash |
| `smriti_read` | Read a file through the privacy gate (audited) |
| `smriti_map` | Overview of tracked files and cataloged dirs |
| `smriti_outline` | Document structure: title, headings, topics |
| `smriti_history` | Lifecycle events for a path |
| `smriti_audit` | Backup audit report |
| `smriti_manifest` | Bulk tier-1 path export |
| `smriti_health` | Status check |

Every response includes a freshness envelope (`as_of`, `is_stale`).
`smriti_read` is the intended file access point for AI agents — it enforces
allowlist rules and logs every access.

## Current state (v0.2.0)

**Working:**
- Full scanner with mtime+size short-circuit, move/copy/hardlink detection
- Two-tier classification with `.smritiignore` support
- BLAKE3 content hashing with body-hash for minor change detection
- FTS5 BM25 search
- All CLI commands and MCP tools listed above
- Privacy gate with read audit logging
- Batched scanner with parallel hashing via rayon

**Feature-gated:**
- Dense embeddings via BGE-M3 ONNX (`--features embedding` + `SMRITI_MODEL_PATH`)
- HTTP transport for the daemon (`--features http`)

## Planned / potential

- **Parallel walk** — parallelize the directory walk (currently single-threaded via walkdir)
- **`smriti watch`** — inotify/fanotify for incremental updates instead of
  full rescans
- **systemd user service** — run the daemon as a persistent service
- **Hybrid search in CLI/MCP** — `search_hybrid` (BM25 + dense, RRF merge)
  exists in the codebase but isn't wired to commands yet
- **Content blob store + revert** — store file versions, enable rollback
- **Downstream subscriptions** — let other tools (grantha, agents) subscribe to
  scan events

## License

TBD
