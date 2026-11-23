-- A typed owner message skips a pending secret request, as it skips the
-- bot's questions and computer handoffs: the request becomes 'skipped', its
-- turn continues and learns so, and a late answer is refused.
--
-- Triggers and the turn_holds view name the table in their bodies; the
-- legacy rename keeps them pointing at the rebuilt table.
PRAGMA legacy_alter_table = ON;

CREATE TABLE secret_requests_new (
    id BLOB NOT NULL PRIMARY KEY,
    bot_id BLOB NOT NULL REFERENCES bots (id),
    turn_id BLOB NOT NULL REFERENCES turns (id),
    attempt INTEGER NOT NULL,
    name text NOT NULL CHECK (length(name) BETWEEN 1 AND 40 AND substr(name, 1, 1) GLOB '[a-z]' AND name NOT GLOB '*[^a-z0-9_-]*'),
    reason text NOT NULL CHECK (trim(reason) <> '' AND length(CAST(reason AS BLOB)) <= 500),
    env_names TEXT NOT NULL CHECK (json_valid(env_names)),
    status text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'stored', 'cancelled', 'skipped')),
    delivered_attempt INTEGER,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    settled_at TEXT,
    description TEXT CHECK (description IS NULL OR length(CAST(description AS BLOB)) <= 200),
    website TEXT CHECK (website IS NULL OR (length(website) BETWEEN 1 AND 253 AND website NOT GLOB '*[^a-z0-9.-]*')),
    UNIQUE (turn_id, name)
) STRICT;

INSERT INTO secret_requests_new
    (id, bot_id, turn_id, attempt, name, reason, env_names, status, delivered_attempt,
     created_at, settled_at, description, website)
SELECT id, bot_id, turn_id, attempt, name, reason, env_names, status, delivered_attempt,
       created_at, settled_at, description, website
FROM secret_requests;
DROP TABLE secret_requests;
ALTER TABLE secret_requests_new RENAME TO secret_requests;

PRAGMA legacy_alter_table = OFF;

CREATE INDEX secret_requests_turn ON secret_requests (turn_id);
CREATE UNIQUE INDEX secret_requests_pending ON secret_requests (turn_id) WHERE status = 'pending';
