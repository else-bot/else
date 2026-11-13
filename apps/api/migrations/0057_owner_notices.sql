-- A bot's note that a task delegated to it ends needing the owner: a
-- decision, or a blocker only the owner can clear. It never holds a turn and
-- outlives it. It stays pending until the owner messages the bot (`replied`)
-- or marks it handled (`dismissed`), or until the attempt that flagged it
-- ends with a card for the owner pending on the turn (`superseded`): the card
-- shows instead. A turn has at most one pending notice; flagging again
-- rewrites it.
CREATE TABLE owner_notices (
    id BLOB NOT NULL PRIMARY KEY,
    bot_id BLOB NOT NULL REFERENCES bots (id),
    turn_id BLOB NOT NULL REFERENCES turns (id),
    kind TEXT NOT NULL CHECK (kind IN ('blocked', 'decision')),
    summary TEXT NOT NULL CHECK (trim(summary) <> '' AND length(CAST(summary AS BLOB)) <= 300),
    tried TEXT CHECK (tried IS NULL OR (trim(tried) <> '' AND length(CAST(tried AS BLOB)) <= 200)),
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'replied', 'dismissed', 'superseded')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    settled_at TEXT,
    CHECK ((status = 'pending') = (settled_at IS NULL))
) STRICT;

CREATE INDEX owner_notices_turn ON owner_notices (turn_id);
CREATE INDEX owner_notices_bot_pending ON owner_notices (bot_id) WHERE status = 'pending';
CREATE UNIQUE INDEX owner_notices_turn_pending ON owner_notices (turn_id) WHERE status = 'pending';
