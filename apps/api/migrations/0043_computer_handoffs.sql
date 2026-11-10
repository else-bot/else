-- A bot's request that the owner take over its computer. It never holds a
-- turn: the bot ends its reply, and the request stays pending until the owner
-- takes over, dismisses it, sends a newer message, or the bot asks again.
CREATE TABLE computer_handoffs (
    id BLOB NOT NULL PRIMARY KEY,
    bot_id BLOB NOT NULL REFERENCES bots (id),
    turn_id BLOB NOT NULL REFERENCES turns (id),
    reason TEXT NOT NULL CHECK (trim(reason) <> '' AND length(CAST(reason AS BLOB)) <= 200),
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'taken', 'dismissed', 'skipped')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    settled_at TEXT
) STRICT;

CREATE INDEX computer_handoffs_turn ON computer_handoffs (turn_id);
CREATE UNIQUE INDEX computer_handoffs_pending ON computer_handoffs (bot_id) WHERE status = 'pending';
