-- Task secrets belong to the installation: every bot on this server can name
-- one. Where several bots stored the same name, the most recently updated
-- copy keeps it and each other copy is kept under name-<first 8 hex of its
-- bot id>, so no stored credential is lost.
CREATE TABLE task_secrets_shared (
    name text NOT NULL PRIMARY KEY CHECK (length(name) BETWEEN 1 AND 40 AND substr(name, 1, 1) GLOB '[a-z]' AND name NOT GLOB '*[^a-z0-9_-]*'),
    env_names TEXT NOT NULL CHECK (json_valid(env_names)),
    salt BLOB NOT NULL CHECK (length(CAST(salt AS BLOB)) = 32),
    ciphertext BLOB NOT NULL CHECK (length(CAST(ciphertext AS BLOB)) <= 262144),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))
) STRICT;

INSERT INTO task_secrets_shared (name, env_names, salt, ciphertext, created_at, updated_at)
SELECT CASE WHEN rank = 1 THEN name
            ELSE substr(name, 1, 31) || '-' || lower(substr(hex(bot_id), 1, 8)) END,
       env_names, salt, ciphertext, created_at, updated_at
FROM (SELECT *, row_number() OVER (PARTITION BY name ORDER BY updated_at DESC, bot_id) AS rank
      FROM task_secrets);

DROP TABLE task_secrets;
ALTER TABLE task_secrets_shared RENAME TO task_secrets;
