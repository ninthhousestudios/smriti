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
- **Triage** — `smriti triage` analyzes your index and recommends what to
  reclassify (regenerable build dirs, large media dirs, duplicates). Opens
  recommendations in your `$EDITOR` for you to accept or modify.
- **Backup audit** — `smriti backup-audit /mnt/usb` compares a root against
  your other roots to find redundant, unique, and stale files.
- **MCP server** — `smriti serve` runs a streamable HTTP MCP server for editor
  and AI agent integration. All queries go through a privacy gate with a read
  audit log.

## What it doesn't do

- **No file watcher (yet)** — smriti scans on demand. There is no inotify
  watcher that reacts to changes in real time (planned).
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

# Let smriti recommend what to reclassify
smriti triage

# Export a file list for your backup tool
smriti manifest | rsync -av --files-from=- / /mnt/backup/

# Start the MCP server
smriti serve
```

## Commands

| Command | Description |
|---------|-------------|
| `smriti init` | Create the database and config directory |
| `smriti scan [--paths <path>...] [-j N]` | Scan allowlisted roots (or specific subtrees) |
| `smriti scan-status` | Show status of the most recent scan |
| `smriti find <query> [-k <n>]` | Full-text search (default k=10) |
| `smriti find --path <glob>` | Search by path pattern (e.g., `"*.iso"`, `"~/Downloads/**"`) |
| `smriti find --ext <ext>` | Search by file extension (e.g., `.iso`) |
| `smriti get <content_hash>` | Look up a document by its BLAKE3 hash |
| `smriti history <path>` | Show lifecycle events for a file |
| `smriti audit` | Backup audit summary (top 5 extensions, top 5 tier-2) |
| `smriti audit --full` | Full audit with all extensions and tier-2 entries |
| `smriti audit --ext <ext>` | List all files with a specific extension |
| `smriti audit --tier2` | Show only tier-2 catalog entries |
| `smriti manifest [--format paths\|ndjson]` | Export tier-1 paths for backup tools |
| `smriti triage` | Analyze index, recommend reclassifications in `$EDITOR` |
| `smriti backup-audit <root>` | Compare a root against others for redundancy |
| `smriti roots add/remove/enable/disable/list` | Manage allowlisted roots |
| `smriti prune [--older-than <duration>]` | Clean up old events (default: 30 days) |
| `smriti health` | Database status, roots, last scan |
| `smriti serve [--port N] [--host H] [--stdio]` | Run the MCP server (HTTP default, port 7333) |

## Root management

Roots are the directories smriti is allowed to index. Nothing is scanned until
you explicitly add a root.

```bash
smriti roots add ~/Documents
smriti roots add /mnt/usb-backup

# Temporarily exclude a root (e.g., unmounted USB) without losing index data
smriti roots disable /mnt/usb-backup

# Re-enable when the drive is plugged back in
smriti roots enable /mnt/usb-backup

smriti roots list
# [enabled]  /home/josh/Documents
# [disabled] /mnt/usb-backup
```

Disabled roots are skipped during scans but their index data is preserved.
Search still returns results from disabled roots (marked as stale).

## Triage

`smriti triage` analyzes your index and opens recommendations in `$EDITOR`:

```
# ACTION    PATH                                     SIZE        REASON
catalog     ~/code/bigproject/target/                 12.3 GB     cargo build output
catalog     ~/code/webapp/node_modules/               4.1 GB      npm dependency cache
keep        ~/Music/                                  89.2 GB     large dir, 98% audio

# DUPLICATES — same content at multiple paths
# ACTION    PATH                                     SIZE        DUPLICATE OF
keep        ~/Desktop/report-v2.pdf                   14 MB      ~/Documents/report-v2.pdf
```

Edit the ACTION column (`catalog`, `ignore`, or `keep`), save, and smriti
applies the changes to your `.smritiignore`. Heuristics detect:

- Regenerable build directories (`target/`, `node_modules/`, `.cache/`, etc.)
- Large homogeneous media directories (>90% same type, >1 GB)
- XDG cache and trash directories
- Content duplicates (same BLAKE3 hash at multiple paths)

## Backup audit (USB / removable drives)

Compare a backup root against your live roots:

```bash
# Plug in the USB, add and scan it
smriti roots add /mnt/usb-backup
smriti scan

# See what's redundant, unique, or stale
smriti backup-audit /mnt/usb-backup

# When done, disable it before unplugging
smriti roots disable /mnt/usb-backup
```

The audit classifies files as:
- **Redundant** — same content hash exists on a live root (safe to delete)
- **Unique** — exists only on the backup (keep or decide)
- **Stale** — same relative path elsewhere with newer content

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

`smriti serve` starts a streamable HTTP MCP server on `127.0.0.1:7333`:

```bash
smriti serve                              # default: HTTP on port 7333
smriti serve --port 8080 --host 0.0.0.0   # custom bind
smriti serve --stdio                      # stdio transport (subprocess mode)
```

### Tools

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

### Client configuration

**Claude Code** (`~/.claude/settings.json`):
```json
"smriti": {
  "type": "http",
  "url": "http://127.0.0.1:7333/mcp"
}
```

**Gemini** (`~/.gemini/settings.json`):
```json
"smriti": {
  "url": "http://127.0.0.1:7333/mcp"
}
```

**OpenCode** (`~/.config/opencode/opencode.json`):
```json
"smriti": {
  "type": "remote",
  "url": "http://127.0.0.1:7333/mcp",
  "enabled": true
}
```

## Current state (v0.2.3)

**Working:**
- Full scanner with mtime+size short-circuit, move/copy/hardlink detection
- Parallel hashing via rayon (`-j` flag for thread control)
- Two-tier classification with `.smritiignore` support
- BLAKE3 content hashing with body-hash for minor change detection
- FTS5 BM25 search
- All CLI commands and MCP tools listed above
- Privacy gate with read audit logging
- Streamable HTTP MCP server
- Root enable/disable for removable media
- Triage command with editor-based UX
- Backup audit for comparing roots

**Feature-gated:**
- Dense embeddings via BGE-M3 ONNX (`--features embedding` + `SMRITI_MODEL_PATH`)

## Planned

- **`smriti watch`** — inotify/fanotify for incremental updates instead of
  full rescans
- **systemd user service** — run the server and watcher as persistent services
- **Hybrid search in CLI/MCP** — `search_hybrid` (BM25 + dense, RRF merge)
  exists in the codebase but isn't wired to commands yet
- **Content blob store + revert** — store file versions, enable rollback
- **Downstream subscriptions** — let other tools (grantha, agents) subscribe to
  scan events

## Installing from source

Requires Rust 1.75+ (for `async fn` in traits). If you don't have Rust:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Clone and install:

```bash
git clone https://github.com/josharp/manas.git
cd manas/smriti
cargo install --path .
```

This builds in release mode and installs the `smriti` binary to `~/.cargo/bin/`.
Make sure `~/.cargo/bin` is in your `PATH`.

To rebuild after pulling changes:

```bash
cd manas/smriti
cargo install --path .
```

With dense embeddings (requires downloading the BGE-M3 ONNX model separately):

```bash
cargo install --path . --features embedding
```

## License

TBD
