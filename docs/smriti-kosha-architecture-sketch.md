# smriti + kosha — architecture sketch

Status: sketch
Date: 2026-04-29
Context: clarifies the split of responsibilities between smriti and kosha after rethinking smriti's purpose. Companion to `architecture.md` (smriti internals) and `../../kosha/docs/kosha-sketch.md` (kosha internals). This doc is the *interface* between them.

## the question this answers

When the disk-cleanup framing fell away (gdu solves that better), the question was: what is smriti actually *for*? And once kosha exists, where does the line fall?

This doc states the split: smriti is the **perception and access tier**; kosha is the **comprehension tier**. They compose, and neither is redundant.

## the split

```
                  ┌─────────────────────────────────────────────┐
                  │            agents (CC, opencode, …)         │
                  └────────┬─────────────────────────┬──────────┘
                           │                         │
                "find files about X"        "find passages about X"
                           │                         │
                           v                         v
       ┌───────────────────────────┐   ┌───────────────────────────┐
       │   smriti — perception     │   │   kosha — comprehension   │
       │                           │   │                           │
       │   what files exist        │   │   what's inside them      │
       │   where, when, identity   │   │   page-level retrieval    │
       │   gated read access       │   │   citations + snippets    │
       │   lifecycle events        │   │   text + image embeddings │
       └────────────┬──────────────┘   └─────────────┬─────────────┘
                    │                                │
                    │  scan events ("new PDF at X")  │
                    └────────────────────────────────┘
                                       │
                                       v
                            ┌─────────────────────┐
                            │  filesystem (~/...) │
                            └─────────────────────┘
```

**One sentence:** smriti finds the file; kosha reads what's inside.

## smriti — goals and intentions

### what it is

The filesystem perception layer. A content-addressed, allowlist-gated, audited index of *what exists in your filesystem*, *where it lives*, and *how it has changed over time*.

### what it owns

- **Existence and identity.** Every tracked file has a BLAKE3 content hash. Two files with the same content are recognized as the same document, regardless of path. Moves, copies, hardlinks, and edits are detected across scans.
- **Location and lifecycle.** Every path has an appearance time, a disappearance time, and a stream of lifecycle events (`created`, `updated`, `moved`, `copied`, `deleted`, `hardlinked`, `minor_change`).
- **Classification.** Two tiers — *indexed* (tier 1, hashed + lightweight metadata extracted) and *cataloged* (tier 2, existence + size only). Anything sensitive is ignored. Hardened defaults block secrets; per-tree `.smritiignore` files refine.
- **Lightweight metadata.** Title, summary, topics, structure for text files. For binary files, just filename + size + mime.
- **Shallow FTS.** BM25 over title, summary, topics, and a capped slice of text-file content (default 100 KB). This is good for "find the file about X," not "find the passage about X."
- **The privacy gate.** A single read entry point (`smriti_read`) that enforces allowlist policy and writes a read audit log. Intended as the access layer for any agent or downstream tool.
- **A backup manifest.** The positive list of tier-1 paths, exportable to rsync/restic/borg.

### what it deliberately does *not* own

- Deep content extraction from binary formats (PDF, epub, docx). smriti detects them and indexes filename + size only.
- Page-level or chunk-level retrieval. smriti's unit is the file.
- Citation machinery. smriti can return a path; it cannot return "page 47 of book X."
- Embeddings of full document content. The current optional dense embedding only covers the same shallow material as FTS (title/summary/topics + capped content excerpt).
- Long-term semantic memory of conversations or decisions. That's chitta.
- Code structure. That's sutra.

### why this shape

A perception layer needs to be *cheap, complete, and safe*. Cheap so it can cover the whole home directory. Complete so consumers don't have to walk the filesystem themselves. Safe so agents can be given access without leaking secrets. Pushing comprehension out keeps perception fast and lets the comprehension tier evolve independently (different models, different chunking, different storage costs).

## kosha — goals and intentions

### what it is

The document comprehension layer. A page-aware, embedding-backed index of *what's inside* the documents that smriti has discovered. Detailed in `../../kosha/docs/kosha-sketch.md`; summarized here for the interface story.

### what it owns

- **Decomposition.** PDFs and epubs broken into pages and sections.
- **Extraction.** Text where a text layer exists. Page-image rendering for scan-only material.
- **Embeddings.** Page-level vectors via Qwen3-VL-Embedding-2B (multimodal — text and image share one space). Per-page FTS for pages with text layers.
- **Citations.** Stable book ids that survive re-scans and edition changes. Page-anchored snippets that chitta can store as durable references.
- **Per-document state.** Extraction status, embedding status, page count, format.

### what it deliberately does *not* own

- Knowing what files exist on disk. It subscribes to smriti for that.
- Reading files directly from the filesystem. It reads via `smriti_read`, going through the privacy gate.
- Tracking moves and renames. smriti's content hash already collapses these; kosha keys off the hash.
- Whole-corpus text search outside its ingested set. Ask smriti for the file; kosha only indexes what was explicitly ingested.

## composition — how they fit together

### typical agent query flows

**Discovery only:** "Where on my filesystem do I have notes about Bayesian inference?"

```
agent  ──► smriti_find "Bayesian inference"
              ── returns: paths + titles + topics for text files
              ── (PDFs appear by filename only)
```

**Discovery → comprehension:** "What does the Brihat Parashara say about Saturn-Mars conjunctions?"

```
agent  ──► smriti_find "Brihat Parashara"
              ── returns: ~/library/classics/bphs.pdf
       ──► kosha_search "Saturn Mars conjunction" book:bphs
              ── returns: page 47 snippet + citation
       ──► kosha_read book:bphs page:47
              ── returns: full page text or image description
```

**Direct comprehension (when agent already knows the book):**

```
agent  ──► kosha_search "samadhi" book:patanjali-yoga-sutras
              ── returns: page-level matches with citations
```

### the privacy gate is the seam

kosha does not stat or read files directly. When kosha needs to ingest a new PDF, it calls `smriti_read` (or a forthcoming bulk equivalent) by path or content hash. Reasons:

- One audit point. Every byte read by every downstream tool flows through one logged interface.
- One policy point. `.smritiignore` and root allowlists apply to kosha automatically.
- One identity point. kosha gets the BLAKE3 hash from smriti, so its `books.content_hash` is consistent with the rest of the ecosystem.

This is why the gate exists. Without consumers like kosha, the gate looks like overhead. With them, it's the load-bearing piece.

### scan events as the activation signal

Today kosha doesn't know when smriti has discovered a new PDF. It would have to poll. The intended pattern:

```
smriti scan completes
   │
   ├── emits events: {created, updated, moved, ...} with content_hash + path + tier
   │
   └── subscribers (kosha) filter for the file types they care about
          │
          └── kosha enqueues ingestion for new/changed PDFs
```

This is `Downstream subscriptions` from smriti's planned section. It is the integration point that makes the two-tier architecture actually work end-to-end without polling.

## what each side knows about the other

**smriti knows nothing about kosha.** It emits events and serves reads. It does not maintain a list of subscribers or a per-tool ingestion state. This keeps smriti's responsibilities small and lets it be replaced or extended without touching kosha.

**kosha knows two things about smriti:**

1. The MCP endpoint to read files through (`smriti_read`).
2. The event stream / subscription endpoint to listen on.

It also stores `content_hash` against each book record so it can correlate with smriti queries. That's the only schema-level coupling.

## boundary cases and how they resolve

| case | who handles it | why |
|---|---|---|
| Markdown notes (`.md`) | smriti FTS is sufficient for most queries | Notes are text and usually small; smriti already indexes content up to 100 KB. kosha is overkill. |
| Source code | sutra, not smriti or kosha | Code intelligence is a separate, structural problem. |
| Plain `.txt` files | smriti FTS, optionally kosha | If they're long-form prose, kosha may want them too. The default is "smriti only." |
| PDFs and epubs | smriti for existence, kosha for content | smriti returns "this PDF exists at /path"; kosha returns "page 47 says X." |
| Images (`.jpg`, `.png`) | smriti for existence; kosha (future) for visual embeddings if ever needed | Currently neither does visual search. |
| `~/.smriti/index.db` | itself, ignored by default | smriti's own DB lives under hardened defaults. |

## why this is better than smriti doing it all

A naive expansion of smriti would add PDF extraction, page tables, and per-page embeddings into the same database and module structure. Reasons not to:

- **Scan latency.** Embedding a single page image takes ~4 minutes on CPU. A 500-page PDF would block a smriti scan for ~35 hours. Smriti scans need to stay minutes-scale on a typical home directory.
- **Storage shape.** Per-page rows + 2048-dim vectors + page images don't belong next to per-file metadata. Different access patterns, different growth profile, different backup considerations.
- **Tooling churn.** Document extraction is a moving target — better OCR, better layout parsers, better multimodal models. That iteration shouldn't perturb the perception tier.
- **Deployment.** kosha may eventually need a GPU (RunPod batch). smriti must remain a single local Rust binary with no external dependencies.

Splitting keeps smriti boring (good) and lets kosha be ambitious (also good).

## why this is better than kosha doing it all

A naive expansion of kosha would have it walk the filesystem itself, extract text, and skip the smriti layer entirely. Reasons not to:

- **No gate.** Kosha would need its own allowlist + audit machinery, duplicating what already exists.
- **No identity.** Without smriti's content hashing, the same book at two paths becomes two separate ingestions.
- **No reuse.** Other tools (chitta cross-references, future agents) want filesystem perception too. Building it once in smriti pays off across the ecosystem.
- **Coupling to formats.** Kosha would have to track every text file, code file, and note file just to know "is this worth ingesting?" — that's smriti's job.

## what this doc is not

This is the *architecture* sketch, not the *implementation* plan. The next-steps doc (`smriti-next-steps.md`) covers what smriti needs to do to support this story concretely: the event stream, the bulk read path, the planned-vs-current divergence in semantic search framing, etc.
