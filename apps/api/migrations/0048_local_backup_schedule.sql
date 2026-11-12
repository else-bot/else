-- The owner picks when the daily local backup runs, in the server's time
-- zone (minutes after midnight), and how many local sets to keep. Each run
-- records how long it paused new work, which the next pause shows as its
-- expected length. See backup::settings::LocalSchedule.

ALTER TABLE backup_settings ADD COLUMN local_enabled INTEGER NOT NULL DEFAULT 1
    CHECK (local_enabled IN (0, 1));
ALTER TABLE backup_settings ADD COLUMN local_minute INTEGER NOT NULL DEFAULT 180
    CHECK (local_minute BETWEEN 0 AND 1439);
ALTER TABLE backup_settings ADD COLUMN local_keep INTEGER NOT NULL DEFAULT 7
    CHECK (local_keep BETWEEN 1 AND 1000);

ALTER TABLE backup_runs ADD COLUMN paused_ms INTEGER CHECK (paused_ms IS NULL OR paused_ms >= 0);
