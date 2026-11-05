CREATE TABLE backup_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    revision INTEGER NOT NULL,
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    interval_secs INTEGER NOT NULL CHECK (interval_secs BETWEEN 0 AND 2592000),
    keep_count INTEGER NOT NULL CHECK (keep_count BETWEEN 1 AND 1000),
    destination TEXT,
    secret_salt BLOB,
    secret_cipher BLOB,
    CHECK ((destination IS NULL AND secret_salt IS NULL AND secret_cipher IS NULL)
        OR (destination IS NOT NULL AND secret_salt IS NOT NULL AND secret_cipher IS NOT NULL))
);
