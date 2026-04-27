CREATE TABLE documents (
    content_hash TEXT PRIMARY KEY,
    body_hash TEXT,
    title TEXT,
    summary TEXT,
    structure TEXT,
    topics TEXT,
    embed_excluded BOOLEAN NOT NULL DEFAULT FALSE,
    embedding_model TEXT,
    is_binary BOOLEAN NOT NULL DEFAULT FALSE,
    first_seen TIMESTAMP NOT NULL,
    byte_size INTEGER
);

CREATE VIRTUAL TABLE document_vectors USING vec0(
    content_hash TEXT PRIMARY KEY,
    embedding FLOAT[1024]
);

CREATE VIRTUAL TABLE document_fts USING fts5(
    content_hash UNINDEXED,
    title,
    topics,
    summary,
    content
);

CREATE TABLE paths (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    content_hash TEXT NOT NULL REFERENCES documents(content_hash),
    path TEXT NOT NULL,
    root TEXT NOT NULL,
    is_hardlink BOOLEAN NOT NULL DEFAULT FALSE,
    mtime TIMESTAMP NOT NULL,
    size_bytes INTEGER NOT NULL,
    appeared TIMESTAMP NOT NULL,
    disappeared TIMESTAMP,
    UNIQUE(content_hash, path, appeared)
);
CREATE INDEX idx_paths_path ON paths(path);
CREATE INDEX idx_paths_disappeared ON paths(disappeared) WHERE disappeared IS NULL;

CREATE TABLE events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    event_type TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    path TEXT NOT NULL,
    previous_hash TEXT,
    previous_path TEXT,
    timestamp TIMESTAMP NOT NULL,
    file_extension TEXT,
    mime_type TEXT
);
CREATE INDEX idx_events_hash ON events(content_hash);
CREATE INDEX idx_events_path ON events(path);
CREATE INDEX idx_events_ts ON events(timestamp);

CREATE TABLE catalog (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT NOT NULL,
    total_bytes INTEGER NOT NULL,
    file_count INTEGER NOT NULL,
    last_modified TIMESTAMP,
    regenerable BOOLEAN NOT NULL DEFAULT TRUE,
    last_scanned TIMESTAMP NOT NULL,
    previous_total_bytes INTEGER,
    previous_file_count INTEGER,
    UNIQUE(path)
);

CREATE TABLE snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp TIMESTAMP NOT NULL,
    tier1_files_scanned INTEGER,
    tier2_dirs_cataloged INTEGER,
    events_emitted INTEGER,
    duration_ms INTEGER
);

CREATE TABLE read_audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT NOT NULL,
    content_hash TEXT,
    timestamp TIMESTAMP NOT NULL,
    caller TEXT
);
CREATE INDEX idx_read_audit_ts ON read_audit(timestamp);
