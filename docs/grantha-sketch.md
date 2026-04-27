# grantha — sketch

Status: sketch
Date: 2026-04-26
Context: aion phase 5 needs library perception; smriti handles filesystem, grantha handles document intelligence on top

## what it is

Grantha (ग्रन्थ — "text, literary work") is a document intelligence layer that sits on top of smriti. Smriti knows you have `brihat-parashara-hora-shastra.pdf` and where it lives. Grantha knows what's on page 47.

It decomposes structured documents (PDFs, epubs) into pages and sections, extracts what content it can, stores page images for future multimodal embedding, and exposes per-page semantic search and citation tools over MCP.

## what it is not

- Not a filesystem tracker. Smriti does that. Grantha subscribes to smriti for file events.
- Not a general-purpose search engine. It searches *within* documents that smriti already knows about.
- Not an OCR pipeline. v1 extracts text layers where they exist and stores page images where they don't. OCR and multimodal embedding are upgrade paths, not launch requirements.

## the hard problem

Most astrology reference material is scanned classical texts — no text layer, mixed scripts (Sanskrit, transliterated, English commentary), complex layouts with tables and diagrams.

Text extraction works for born-digital PDFs. For scans, the options today are all bad: OCR is slow and error-prone on mixed scripts; cloud multimodal APIs work but violate local-first; local multimodal embedding models (EmbedGemma 2) exist but are unproven and large.

**v1 strategy:** extract what you can (text layers), store page images for everything, and treat scan-only pages as "cataloged but not yet searchable." When good local multimodal embedding arrives, the upgrade is swapping the embedding pipeline — not redesigning the architecture.

**The design decision that matters now** is in the shared embedder service, not here: its interface should accept text *or images* and return vectors. Model-agnostic. Then grantha sends text today and page images tomorrow, same API.

## how it relates to the ecosystem

```
smriti (filesystem perception)
  │ "new PDF appeared at ~/library/classics/bphs.pdf"
  │ "file moved from ~/Downloads/"
  v
grantha (document intelligence)
  │ decompose into pages, extract text, store images
  │ embed via shared embedder service
  │ expose citation + search tools
  v
chitta (memory)
  "research note about Saturn-Mars conjunction,
   cites book:bphs page 47"
```

- **smriti → grantha:** smriti tracks files, grantha processes them. Grantha registers interest in configured file types (`.pdf`, `.epub`) within smriti roots. When smriti detects a new or changed PDF, grantha re-processes it.
- **grantha → chitta:** chitta memories reference grantha documents via `book:<id>` tags and `metadata.page`. Grantha provides the citation; chitta stores the relationship.
- **grantha → embedder service:** grantha sends text (v1) or page images (v2) to the shared embedder. Doesn't load models itself.

## storage

SQLite, same single-file story as smriti. Separate DB (`~/.grantha/index.db` or colocated with smriti).

```sql
-- one row per document (linked to smriti by content_hash)
CREATE TABLE books (
    id TEXT PRIMARY KEY,             -- stable book id (slug or derived)
    content_hash TEXT NOT NULL,      -- smriti's content hash for this version
    title TEXT,
    author TEXT,
    format TEXT NOT NULL,            -- pdf, epub
    page_count INTEGER,
    has_text_layer BOOLEAN,
    extraction_status TEXT,          -- complete, partial, images_only
    first_indexed TIMESTAMP NOT NULL,
    last_indexed TIMESTAMP NOT NULL
);

-- one row per page
CREATE TABLE pages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    book_id TEXT NOT NULL REFERENCES books(id),
    page_number INTEGER NOT NULL,
    text_content TEXT,               -- extracted text, NULL if scan-only
    image_path TEXT,                 -- stored page image for future multimodal
    has_text BOOLEAN NOT NULL,
    UNIQUE(book_id, page_number)
);

-- per-page embeddings (text or multimodal, depending on era)
CREATE VIRTUAL TABLE page_vectors USING vec0(
    page_id INTEGER PRIMARY KEY,
    embedding FLOAT[1024]
);

-- BM25 over extracted text
CREATE VIRTUAL TABLE page_fts USING fts5(
    page_id UNINDEXED,
    book_id UNINDEXED,
    text_content
);
```

## MCP tools (sketch)

- **grantha_search** — semantic query across all books, returns page-level results with snippets
- **grantha_read** — read a page or page range from a book
- **grantha_cite** — returns a citable reference (book id + page + snippet) formatted for chitta metadata
- **grantha_books** — list indexed books with extraction status
- **grantha_outline** — table of contents / structure of a book (where extractable)

## book identity

A book needs a stable `id` that survives re-scans, file moves, and new editions. Options:

- **ISBN** where available (embedded in PDF metadata or epub)
- **Derived slug** from title + author (e.g. `bphs-parashara`) as fallback
- **Content hash** is *not* the id — it changes with the file version

The `book:<id>` tag in chitta references this id. If a new scan of the same book appears (different file, different hash), grantha links it to the same book id; chitta citations stay valid.

This needs thought. For now, assume human-assignable slugs with auto-suggestion from metadata.

## what it depends on

| dependency | status | needed for |
|---|---|---|
| smriti v0.1 | not started | file awareness, change events |
| shared embedder service | not started | embeddings (text now, images later) |
| chitta engine/server split | planned | citation storage via tags |
| PDF text extraction lib | available (poppler, pdf-extract, lopdf) | v1 text layer extraction |
| local multimodal embeddings | not available | v2 scan understanding |

## timeline

Not urgent. Aion phase 5 is the consumer, and phases 1–4 come first. The sketch exists so:

1. The embedder service interface gets designed with image input in mind
2. Smriti's architecture doesn't accidentally make grantha harder
3. The concept is captured before the conversation context is lost

## open questions

- **Page image storage.** Stored as files on disk (referenced by path) or as blobs in SQLite? Files are simpler but add a second thing to back up.
- **Chunking strategy.** Pages are a natural unit for PDFs. For epubs, chapters/sections might be better. Or both: pages for citation anchoring, overlapping chunks for embedding quality.
- **Book id assignment.** Auto-slug from metadata vs. human-assigned vs. hybrid. Wrong answer here means broken citations.
- **Scope.** PDFs only first? Or epubs from day one? Epub extraction is much cleaner.
- **Relation to smriti tiers.** Are grantha-processed files still tier 1 in smriti, or does grantha's processing make them something more? Probably: tier 1 in smriti (file-level), richer in grantha (page-level). Two layers of understanding.
