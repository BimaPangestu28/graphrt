-- graphrt telemetry collector schema (Cloudflare D1 / SQLite).
--   wrangler d1 execute graphrt-telemetry --remote --file=schema.sql
--
-- No IP addresses, timestamps finer than a UTC day, or request headers are stored.

-- One row per install per UTC day on which it reported: the basis of
-- weekly-active-installs. Days are 'YYYY-MM-DD'.
CREATE TABLE IF NOT EXISTS reports (
  install_id   TEXT NOT NULL CHECK (length(install_id) = 32),
  received_day TEXT NOT NULL,
  version      TEXT NOT NULL,
  os           TEXT NOT NULL,
  arch         TEXT NOT NULL,
  build        TEXT NOT NULL CHECK (build IN ('cli', 'docker')),
  features     TEXT NOT NULL, -- JSON array of cargo feature names
  PRIMARY KEY (install_id, received_day)
);

CREATE INDEX IF NOT EXISTS reports_by_day ON reports (received_day);

-- Usage per install and UTC day, as aggregated by the CLI.
CREATE TABLE IF NOT EXISTS daily_counts (
  install_id     TEXT NOT NULL CHECK (length(install_id) = 32),
  day            TEXT NOT NULL,
  runs           INTEGER NOT NULL CHECK (runs >= 0),
  serve_requests INTEGER NOT NULL CHECK (serve_requests >= 0),
  PRIMARY KEY (install_id, day)
);

CREATE INDEX IF NOT EXISTS daily_counts_by_day ON daily_counts (day);
