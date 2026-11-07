-- Task secrets belong to the installation: every bot on this server can name
-- one. Where several bots stored the same name, the most recently updated
-- copy keeps it. Each other copy waits in legacy_task_secrets until the
-- upgrade step in db::migrate_in gives it a free name and points that bot's
-- jobs at it, so no stored credential is lost.
CREATE TABLE task_secrets_shared (
    name text NOT NULL PRIMARY KEY CHECK (length(name) BETWEEN 1 AND 40 AND substr(name, 1, 1) GLOB '[a-z]' AND name NOT GLOB '*[^a-z0-9_-]*'),
    env_names TEXT NOT NULL CHECK (json_valid(env_names)),
    salt BLOB NOT NULL CHECK (length(CAST(salt AS BLOB)) = 32),
    ciphertext BLOB NOT NULL CHECK (length(CAST(ciphertext AS BLOB)) <= 262144),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))
) STRICT;

CREATE TABLE legacy_task_secrets (
    bot_id BLOB NOT NULL,
    name text NOT NULL,
    env_names TEXT NOT NULL,
    salt BLOB NOT NULL,
    ciphertext BLOB NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (bot_id, name)
) STRICT;

CREATE TEMP TABLE ranked_task_secrets AS
SELECT *, row_number() OVER (PARTITION BY name ORDER BY updated_at DESC, bot_id) AS rank
FROM task_secrets;

INSERT INTO task_secrets_shared (name, env_names, salt, ciphertext, created_at, updated_at)
SELECT name, env_names, salt, ciphertext, created_at, updated_at
FROM ranked_task_secrets WHERE rank = 1;

INSERT INTO legacy_task_secrets (bot_id, name, env_names, salt, ciphertext, created_at, updated_at)
SELECT bot_id, name, env_names, salt, ciphertext, created_at, updated_at
FROM ranked_task_secrets WHERE rank > 1;

DROP TABLE ranked_task_secrets;
DROP TABLE task_secrets;
ALTER TABLE task_secrets_shared RENAME TO task_secrets;
