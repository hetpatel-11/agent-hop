-- D1 schema for the agent-hop telemetry ingest (telemetry/worker.js).
-- One row per event. No IP address, no queries, no file paths -- only the
-- aggregate fields the client sends.
CREATE TABLE IF NOT EXISTS events (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  device_id   TEXT,      -- anonymous, random per-install UUID
  session_id  TEXT,      -- random per-process id (no cross-run linkage)
  app_version TEXT,
  os          TEXT,
  arch        TEXT,
  country     TEXT,      -- coarse geo from Cloudflare; IP is never stored
  event       TEXT,      -- e.g. "app_launched"
  event_time  TEXT,      -- client-side RFC3339 timestamp
  props       TEXT,      -- JSON of remaining event properties
  received_at TEXT       -- server-side RFC3339 timestamp
);

CREATE INDEX IF NOT EXISTS idx_events_event ON events (event);
CREATE INDEX IF NOT EXISTS idx_events_device ON events (device_id);
CREATE INDEX IF NOT EXISTS idx_events_received ON events (received_at);

-- Explicit user-submitted notes from `ah feedback`. Separate from aggregate
-- events so we can read them without mixing in usage pings.
CREATE TABLE IF NOT EXISTS feedback (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  device_id   TEXT,
  app_version TEXT,
  os          TEXT,
  arch        TEXT,
  country     TEXT,
  message     TEXT NOT NULL,
  received_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_feedback_received ON feedback (received_at);
