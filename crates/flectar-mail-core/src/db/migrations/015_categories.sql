-- Siftmail M2: user-managed categories with deterministic rules.
-- Rules may only apply a category (optionally star / mark read when the user
-- ticks it on the rule). AI output never writes here; see ai_suggestions.
CREATE TABLE IF NOT EXISTS categories (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  color TEXT,
  show_as_tab INTEGER NOT NULL DEFAULT 1,
  priority INTEGER NOT NULL DEFAULT 100,
  enabled INTEGER NOT NULL DEFAULT 1,
  label_id INTEGER REFERENCES labels(id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS category_rules (
  id INTEGER PRIMARY KEY,
  category_id TEXT NOT NULL REFERENCES categories(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  priority INTEGER NOT NULL DEFAULT 100,
  enabled INTEGER NOT NULL DEFAULT 1,
  match_mode TEXT NOT NULL CHECK (match_mode IN ('all', 'any')),
  also_star INTEGER NOT NULL DEFAULT 0,
  also_mark_read INTEGER NOT NULL DEFAULT 0,
  created_from_suggestion INTEGER,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS rule_conditions (
  id INTEGER PRIMARY KEY,
  rule_id INTEGER NOT NULL REFERENCES category_rules(id) ON DELETE CASCADE,
  field TEXT NOT NULL CHECK (field IN ('from_address', 'from_domain', 'from_name', 'to', 'subject', 'body')),
  op TEXT NOT NULL CHECK (op IN ('equals', 'contains', 'wildcard', 'regex', 'exists')),
  value TEXT NOT NULL,
  case_sensitive INTEGER NOT NULL DEFAULT 0,
  negate INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS rule_conditions_rule_idx ON rule_conditions(rule_id);
CREATE TABLE IF NOT EXISTS thread_categories (
  thread_id INTEGER PRIMARY KEY REFERENCES threads(id) ON DELETE CASCADE,
  category_id TEXT REFERENCES categories(id) ON DELETE SET NULL,
  source TEXT NOT NULL CHECK (source IN ('rule', 'user')),
  rule_id INTEGER,
  updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS thread_categories_category_idx ON thread_categories(category_id);

INSERT OR IGNORE INTO categories (id, name, description, priority, created_at, updated_at) VALUES
  ('bills', '账单', 'Invoices, receipts and payment notices', 10, 0, 0),
  ('subscriptions', '订阅', 'Newsletters and subscriptions', 20, 0, 0),
  ('notifications', '通知', 'Automated service notifications', 30, 0, 0),
  ('vps_alerts', 'VPS警告', 'Server / VPS provider alerts', 5, 0, 0),
  ('app_status', '应用运行状态', 'Application health and status reports', 15, 0, 0);

-- Example rules, disabled by default (one click to enable).
INSERT INTO category_rules (id, category_id, name, priority, enabled, match_mode, created_at)
SELECT 1, 'bills', '主题含发票/账单', 100, 0, 'any', 0
WHERE NOT EXISTS (SELECT 1 FROM category_rules WHERE id = 1);
INSERT INTO rule_conditions (rule_id, field, op, value)
SELECT 1, 'subject', 'regex', '(?i)(invoice|receipt|发票|账单)'
WHERE NOT EXISTS (SELECT 1 FROM rule_conditions WHERE rule_id = 1);
INSERT INTO category_rules (id, category_id, name, priority, enabled, match_mode, created_at)
SELECT 2, 'vps_alerts', 'VPS 服务商告警', 100, 0, 'any', 0
WHERE NOT EXISTS (SELECT 1 FROM category_rules WHERE id = 2);
INSERT INTO rule_conditions (rule_id, field, op, value)
SELECT 2, 'from_address', 'wildcard', '*@*.hetzner.com'
WHERE NOT EXISTS (SELECT 1 FROM rule_conditions WHERE rule_id = 2);

-- Category suggestions reuse ai_suggestions (rule_id = 'category').
ALTER TABLE ai_suggestions ADD COLUMN category_id TEXT;
ALTER TABLE ai_suggestions ADD COLUMN confidence REAL;
ALTER TABLE ai_suggestions ADD COLUMN rationale TEXT NOT NULL DEFAULT '';
