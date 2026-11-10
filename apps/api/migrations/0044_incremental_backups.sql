-- Remote sets share content-addressed chunks, so a set's size no longer
-- says what a run uploaded or what the bucket stores. Each completed set
-- records the bytes its run uploaded, and each destination records the
-- bytes its bucket held after the last retention.
ALTER TABLE backup_remote ADD COLUMN uploaded_bytes INTEGER CHECK (uploaded_bytes >= 0);

CREATE TABLE backup_bucket_usage (
    destination text NOT NULL PRIMARY KEY,
    stored_bytes INTEGER NOT NULL CHECK (stored_bytes >= 0),
    measured_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))
) STRICT;
