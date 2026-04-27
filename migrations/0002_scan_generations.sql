-- Scan generations: batched commits with per-scan tracking.
--
-- Each scan invocation gets a row in scan_runs. Status transitions:
--   running -> complete | failed
-- The last_seen_scan column on paths stamps which scan last observed a file,
-- replacing the disappear/un-disappear pattern with a generation diff.

CREATE TABLE IF NOT EXISTS scan_runs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at  TIMESTAMP NOT NULL,
    finished_at TIMESTAMP,
    status      TEXT NOT NULL CHECK (status IN ('running', 'complete', 'failed')),
    files_seen  INTEGER NOT NULL DEFAULT 0,
    error       TEXT
);

-- Stamp every active paths row with the scan that last observed it.
-- NULL means "pre-generation-tracking" (legacy rows).
ALTER TABLE paths ADD COLUMN last_seen_scan INTEGER
    REFERENCES scan_runs(id);

-- Tag events with their scan run for finalize-phase move/copy upgrades.
ALTER TABLE events ADD COLUMN scan_id INTEGER
    REFERENCES scan_runs(id);

CREATE INDEX IF NOT EXISTS idx_paths_last_seen ON paths(last_seen_scan)
    WHERE disappeared IS NULL;

CREATE INDEX IF NOT EXISTS idx_events_scan_id ON events(scan_id);

-- Backfill: sentinel 0 means "existed before generation tracking".
UPDATE paths SET last_seen_scan = 0 WHERE last_seen_scan IS NULL;
