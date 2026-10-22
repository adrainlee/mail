-- Siftmail M5: shared outbound proxies; accounts reference one via
-- settings_json.connection.proxyId. Passwords live in the credential store.
CREATE TABLE IF NOT EXISTS proxies (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL DEFAULT '',
  kind TEXT NOT NULL CHECK (kind IN ('socks5', 'http')),
  host TEXT NOT NULL,
  port INTEGER NOT NULL CHECK (port BETWEEN 1 AND 65535),
  username TEXT NOT NULL DEFAULT '',
  remote_dns INTEGER NOT NULL DEFAULT 1
);
