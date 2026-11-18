-- A routine grant names the effect it allows. Every grant saved before
-- allowed destructive actions; "Always allow for this routine" on an
-- external card now saves an external grant beside it.
CREATE TABLE routine_grants_new (
    routine_id BLOB NOT NULL REFERENCES routines (id),
    kind TEXT NOT NULL CHECK (kind IN ('destructive', 'external')),
    prompt_revision INTEGER NOT NULL,
    question_id BLOB NOT NULL REFERENCES owner_questions (id),
    granted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    PRIMARY KEY (routine_id, kind)
) STRICT;
INSERT INTO routine_grants_new (routine_id, kind, prompt_revision, question_id, granted_at)
SELECT routine_id, 'destructive', prompt_revision, question_id, granted_at FROM routine_grants;
DROP TABLE routine_grants;
ALTER TABLE routine_grants_new RENAME TO routine_grants;

-- A computer tool grant allows one computer tool's external calls.
CREATE TABLE bot_permissions_new (
    bot_id BLOB NOT NULL REFERENCES bots (id),
    scope TEXT NOT NULL CHECK (scope = 'destructive' OR scope GLOB 'mcp:*' OR scope GLOB 'task:*'
        OR scope GLOB 'computer:*'),
    granted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    question_id BLOB REFERENCES owner_questions (id),
    PRIMARY KEY (bot_id, scope)
) STRICT;
INSERT INTO bot_permissions_new SELECT * FROM bot_permissions;
DROP TABLE bot_permissions;
ALTER TABLE bot_permissions_new RENAME TO bot_permissions;

-- The owner message that answered a card by saying only "Always allow".
-- Its words saved that card's grant and authorize no other one.
ALTER TABLE owner_questions ADD COLUMN typed_turn_id BLOB REFERENCES turns (id);
