-- Files the owner attaches to messages. The bytes live here, so ordinary
-- database backups cover them; a message refers to them by ID in its parts.
CREATE TABLE attachments (
    id BLOB NOT NULL PRIMARY KEY,
    bot_id BLOB NOT NULL REFERENCES bots (id),
    name text NOT NULL CHECK (name <> '' AND length(CAST(name AS BLOB)) <= 255),
    media_type text NOT NULL CHECK (media_type <> '' AND length(media_type) <= 127),
    size INTEGER NOT NULL CHECK (size >= 0),
    sha256 BLOB NOT NULL CHECK (length(sha256) = 32),
    data BLOB NOT NULL CHECK (length(data) = size),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))
) STRICT;

CREATE INDEX attachments_bot ON attachments (bot_id);

-- An owner message's ordered text and file parts; NULL for a plain message.
ALTER TABLE items ADD COLUMN parts TEXT
    CHECK (parts IS NULL OR (kind = 'user' AND json_valid(parts) AND json_type(parts) = 'array'));
