-- Watcher infrastructure: scan request queue and heartbeat.

CREATE TABLE IF NOT EXISTS scan_requests (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    requested_at    TIMESTAMP NOT NULL,
    kind            TEXT NOT NULL CHECK (kind IN ('full', 'partial', 'path')),
    root            TEXT,
    status          TEXT NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending', 'running', 'complete', 'failed')),
    scan_run_id     INTEGER REFERENCES scan_runs(id),
    started_at      TIMESTAMP,
    completed_at    TIMESTAMP,
    error           TEXT
);

CREATE INDEX IF NOT EXISTS idx_scan_requests_status
    ON scan_requests(status, requested_at);

CREATE TABLE IF NOT EXISTS watcher_heartbeat (
    id                      INTEGER PRIMARY KEY CHECK (id = 1),
    pid                     INTEGER NOT NULL,
    started_at              TIMESTAMP NOT NULL,
    updated_at              TIMESTAMP NOT NULL,
    state                   TEXT NOT NULL DEFAULT 'starting'
                            CHECK (state IN ('starting', 'watching', 'scanning', 'stopping', 'reconciling', 'stopped')),
    watch_count             INTEGER NOT NULL DEFAULT 0,
    pending_events          INTEGER NOT NULL DEFAULT 0,
    last_event_processed_at TIMESTAMP,
    last_full_scan_at       TIMESTAMP,
    last_full_scan_duration_ms INTEGER
);
