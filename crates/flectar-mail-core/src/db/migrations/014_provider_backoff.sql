-- Siftmail M1.5: per-provider hourly backoff survives restarts so a restart
-- never triggers a burst of reconnects against a throttling provider.
CREATE TABLE IF NOT EXISTS provider_backoff (
  provider TEXT PRIMARY KEY,
  level INTEGER NOT NULL DEFAULT 0,
  next_allowed_at INTEGER NOT NULL DEFAULT 0, -- unix ms
  last_error TEXT NOT NULL DEFAULT '',
  updated_at INTEGER NOT NULL DEFAULT 0       -- unix ms
);
