-- The Vault: task secrets gain what an owner and a bot need to find the
-- right login (a description and the website it signs in to) and a record
-- of which bot used each one, when, and how. Values stay encrypted in
-- ciphertext; nothing here holds or derives from a value.
ALTER TABLE task_secrets ADD COLUMN description TEXT NOT NULL DEFAULT ''
    CHECK (length(CAST(description AS BLOB)) <= 200);
ALTER TABLE task_secrets ADD COLUMN website TEXT
    CHECK (website IS NULL OR (length(website) BETWEEN 1 AND 253 AND website NOT GLOB '*[^a-z0-9.-]*'));

ALTER TABLE secret_requests ADD COLUMN description TEXT
    CHECK (description IS NULL OR length(CAST(description AS BLOB)) <= 200);
ALTER TABLE secret_requests ADD COLUMN website TEXT
    CHECK (website IS NULL OR (length(website) BETWEEN 1 AND 253 AND website NOT GLOB '*[^a-z0-9.-]*'));

-- Bots are kept by id only, so a removed bot's uses stay readable.
CREATE TABLE secret_uses (
    id INTEGER PRIMARY KEY,
    secret TEXT NOT NULL REFERENCES task_secrets (name) ON DELETE CASCADE ON UPDATE CASCADE,
    bot_id BLOB NOT NULL,
    turn_id BLOB,
    use TEXT NOT NULL CHECK (use IN ('command', 'job', 'browser')),
    used_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))
) STRICT;

CREATE INDEX secret_uses_latest ON secret_uses (secret, id DESC);
