-- Add 'stopping' and 'reconciling' states to watcher_heartbeat.
-- SQLite requires table recreation to alter CHECK constraints.

CREATE TABLE IF NOT EXISTS watcher_heartbeat_new (
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

INSERT OR IGNORE INTO watcher_heartbeat_new
    SELECT * FROM watcher_heartbeat;

DROP TABLE IF EXISTS watcher_heartbeat;

ALTER TABLE watcher_heartbeat_new RENAME TO watcher_heartbeat;
