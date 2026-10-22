-- Siftmail: AI translations (plan §2.2).
CREATE TABLE IF NOT EXISTS translations (
  message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
  target_lang TEXT NOT NULL,
  model TEXT NOT NULL,
  source_hash TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('partial', 'done', 'failed')),
  html TEXT,
  text TEXT,
  chunks_done INTEGER NOT NULL,
  chunks_total INTEGER NOT NULL,
  failed_blocks INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (message_id, target_lang, model)
);
CREATE TABLE IF NOT EXISTS translation_chunks (
  hash TEXT PRIMARY KEY,
  output TEXT NOT NULL,
  used_at INTEGER NOT NULL
);
