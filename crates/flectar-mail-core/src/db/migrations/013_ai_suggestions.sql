-- Siftmail: AI output is advisory only. A model decision that an AI
-- automation rule matches a thread is stored here as a pending suggestion;
-- nothing in this table is ever executed automatically.
CREATE TABLE IF NOT EXISTS ai_suggestions (
  id INTEGER PRIMARY KEY,
  thread_id INTEGER NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
  rule_id TEXT NOT NULL,
  rule_name TEXT NOT NULL DEFAULT '',
  model TEXT NOT NULL DEFAULT '',
  state TEXT NOT NULL DEFAULT 'pending'
    CHECK (state IN ('pending', 'accepted', 'rejected', 'expired')),
  created_at INTEGER NOT NULL,
  decided_at INTEGER,
  UNIQUE (thread_id, rule_id)
);
CREATE INDEX IF NOT EXISTS ai_suggestions_state_idx ON ai_suggestions(state, created_at);
