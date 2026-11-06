CREATE TABLE bot_permissions_new (
    bot_id BLOB NOT NULL REFERENCES bots (id),
    scope TEXT NOT NULL CHECK (scope = 'destructive' OR scope GLOB 'mcp:*' OR scope GLOB 'task:*'),
    granted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    question_id BLOB REFERENCES owner_questions (id),
    PRIMARY KEY (bot_id, scope)
) STRICT;

INSERT INTO bot_permissions_new SELECT * FROM bot_permissions;
DROP TABLE bot_permissions;
ALTER TABLE bot_permissions_new RENAME TO bot_permissions;
