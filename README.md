# smriti

*smriti* (स्मृति — "that which is remembered") is the **filesystem perception
layer** for agents and downstream tools. It maintains a content-addressed,
allowlist-gated, audited index of what files exist under your chosen roots,
where they live, and how they have changed over time.

It is *not* a disk-usage analyzer (use [gdu](https://github.com/dundee/gdu) for
that), not a desktop search engine, and not a document reader. It tracks files
by their BLAKE3 content hash so identical content at different paths is
recognized as the same document, and so moves, copies, hardlinks, and edits
are detected across scans.

smriti is one tier of [manas](https://github.com/josharp/manas) (मनस् — "mind"),
a personal AI operating system. It pairs with **kosha** (the document
*comprehension* tier) to give agents a clean two-step model: *smriti finds the
file; kosha reads what's inside*. See the
[smriti+kosha architecture sketch](docs/smriti-kosha-architecture-sketch.md)
for how the two compose.

## What it does today

- **Content-addressed indexing.** BLAKE3 hashes every tracked file. Identical
  content at different paths shares a single document record.
- **Lifecycle events.** Emits `created`, `updated`, `moved`, `copied`,
  `deleted`, `hardlinked`, and `minor_change` (frontmatter-only edit) events
  to a persisted log.
- **Two-tier classification.** Every path under an allowlisted root is either
  *indexed* (tier 1: hashed, lightweight metadata extracted, FTS-searchable)
  or *cataloged* (tier 2: existence + size only). Secrets and noise are
  ignored entirely.
- **Shallow text search.** BM25 via SQLite FTS5 over title, topics, summary,
  and a capped excerpt of text-file content (default 100 KB,
  `SMRITI_FTS_CONTENT_MAX_BYTES`). Good for *finding the file*. Not a
  substitute for deep document search — see "Reach of search" below.
- **Privacy-first access.** Nothing is indexed until you explicitly add roots.
  Hardened defaults block `.env`, `*.pem`, `*.key`, `id_rsa*`, `*.kdbx`,
  `.ssh/`, `.gnupg/`, browser auth stores, and more. Every file read through
  smriti's MCP interface is checked against the allowlist and logged to a
  read audit table.
- **Backup manifest.** `smriti manifest` exports tier-1 paths for rsync,
  restic, borg, or any tool that takes a file list. An *inclusion-based*
  backup story instead of the usual exclusion-based one.
- **Triage.** `smriti triage` analyzes your index and recommends what to
  reclassify (regenerable build dirs, large media dirs, duplicates). Opens
  recommendations in your `$EDITOR` for you to accept or modify. Best
  understood as **classifier maintenance** — codifying decisions into
  `.smritiignore` so future scans respect them — not disk cleanup.
- **Backup audit.** `smriti backup-audit /mnt/usb` compares a root against
  your other roots to find redundant, unique, and stale files.
- **MCP server.** `smriti serve` runs a streamable HTTP MCP server for editor
  and AI agent integration. All queries go through the privacy gate.

## Reach of search — be honest

smriti's search is a *file-finder*, not a *passage-finder*.

| File type | What smriti indexes | What you can search on |
|---|---|---|
| Markdown / plain text (< 100 KB) | Title, summary, topics, content | All of it via FTS |
| Markdown / plain text (> 100 KB) | Title, summary, topics, first 100 KB of content | Title/topics/summary + the excerpt |
| Source code | Filename, size, mime | Path, name, extension. Use [sutra](https://github.com/josharp/manas) for code search. |
| PDFs, epubs, docx, scanned books | **Filename, size, mime only** | Path, name, extension — **not** the document text |
| Images, audio, video | Filename, size, mime | Path, name, extension |

For deep search inside binary documents (PDFs, epubs), use **kosha**. smriti
will tell you the file exists at `~/library/foo.pdf`; kosha will tell you what
page 47 says.

## What it doesn't do

- **No deep document extraction.** PDFs and other binary formats are detected
  as binary and indexed by filename + size only. Text extraction, OCR,
  page-level retrieval, and citations live in kosha.
- **No file watcher yet.** smriti scans on demand. An inotify/fanotify watcher
  is on the roadmap.
- **No cloud sync.** Everything is local. The SQLite database lives on your
  machine.
- **No content storage.** smriti indexes metadata and hashes; it does not copy
  or store file contents. Files stay where they are.
- **No deep semantic search.** An optional dense-embedding feature flag exists
  (BGE-M3 over the same shallow material as FTS), but for real semantic
  retrieval over document content, kosha is the answer.
- **No code intelligence.** That's [sutra](https://github.com/josharp/manas).
- **No long-term memory of decisions or conversations.** That's chitta.

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
| `smriti find --path <glob> [--limit N]` | Search by path pattern (e.g., `"*.iso"`, `"~/Downloads/**"`) |
| `smriti find --ext <ext> [--limit N]` | Search by file extension (e.g., `.iso`) |
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

The near-term direction is sharpening smriti's role as the *perception and
access* tier of manas, and building the seam that lets **kosha** (document
comprehension) sit cleanly on top of it. See
[docs/smriti-kosha-architecture-sketch.md](docs/smriti-kosha-architecture-sketch.md)
for the architectural framing and
[docs/smriti-next-steps.md](docs/smriti-next-steps.md) for the work that flows
from it.

Headline items:

- **Scan event subscriptions.** Expose the existing event log as a stream
  consumers can poll (or, later, subscribe to) so that kosha and other
  downstream tools react to file changes without re-walking the filesystem.
  This is the load-bearing seam for the two-tier architecture.
- **`smriti watch`.** Inotify/fanotify watcher for incremental updates instead
  of full rescans. Designed to share an event shape with the subscription
  stream above.
- **systemd user service.** Run `smriti serve` and the watcher as persistent
  services so the perception layer is always available to agents.
- **Honest framing of the search story.** Document the FTS reach plainly
  (text files only, capped excerpt) and stop implying smriti can search inside
  PDFs. Decide whether to retire the smriti-side dense embedding feature flag
  in favor of kosha owning deep semantic search.

Why these and not other things: smriti's value is *perception that is cheap,
complete, and safe*. Cheap so it can cover a whole home directory; complete so
consumers don't reimplement filesystem walks; safe so agents can be given
access without leaking secrets. Anything that strengthens those three is in
scope. Deep document understanding, page-level retrieval, multimodal
embeddings, and citation machinery are deliberately out of scope — those live
in kosha.

Speculative / not committed:

- **Content blob store + revert.** Store file versions for rollback. Likely
  collides with kosha's storage if not designed carefully; needs more thought.
- **Hybrid search in CLI/MCP.** `search_hybrid` (BM25 + dense, RRF merge)
  exists in the codebase but isn't wired to commands. May be retired in favor
  of kosha if the dense path moves there entirely.

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
