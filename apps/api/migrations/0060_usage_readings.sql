-- Subscription limit readings providers report while a turn runs: how much
-- of a rolling window's cap the owner's account has used, from any client.
-- Append-only; readings older than the longest window are pruned on insert.
-- report_id groups the windows of one report, so the windows can be
-- compared reading for reading.
CREATE TABLE usage_readings (
    id INTEGER PRIMARY KEY,
    report_id BLOB NOT NULL,
    provider_id TEXT NOT NULL,
    window_minutes INTEGER NOT NULL CHECK (window_minutes > 0),
    used_percent REAL NOT NULL CHECK (used_percent >= 0),
    resets_at TEXT NOT NULL,
    observed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))
) STRICT;

CREATE INDEX usage_readings_observed ON usage_readings (observed_at);
