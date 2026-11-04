-- A bot's question to the owner, answered by choosing one of its options
-- in the conversation: a plain choice, or approval of one destructive or
-- external action. Everything here is meant to be shown; secret requests
-- stay in their own table.
CREATE TABLE owner_questions (
    id BLOB NOT NULL PRIMARY KEY,
    bot_id BLOB NOT NULL REFERENCES bots (id),
    turn_id BLOB NOT NULL REFERENCES turns (id),
    attempt INTEGER NOT NULL,
    kind text NOT NULL CHECK (kind IN ('choice', 'destructive', 'external')),
    title text NOT NULL CHECK (trim(title) <> '' AND length(CAST(title AS BLOB)) <= 500),
    context text CHECK (context IS NULL OR length(CAST(context AS BLOB)) <= 4000),
    choices TEXT NOT NULL CHECK (json_valid(choices) AND json_type(choices) = 'array'),
    action TEXT CHECK (action IS NULL OR json_valid(action)),
    action_hash BLOB CHECK (action_hash IS NULL OR length(CAST(action_hash AS BLOB)) = 32),
    status text NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'answered', 'skipped', 'cancelled')),
    answer text,
    consumed_attempt INTEGER,
    delivered_attempt INTEGER,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    settled_at TEXT,
    CHECK ((status = 'answered') = (answer IS NOT NULL)),
    CHECK ((kind = 'choice') = (action IS NULL))
) STRICT;

CREATE INDEX owner_questions_turn ON owner_questions (turn_id);
CREATE UNIQUE INDEX owner_questions_pending ON owner_questions (turn_id) WHERE status = 'pending';

-- Standing grants the owner gave a bot. Only destructive actions can be
-- allowed in advance; external ones are approved one at a time.
CREATE TABLE bot_permissions (
    bot_id BLOB NOT NULL REFERENCES bots (id),
    scope text NOT NULL CHECK (scope IN ('destructive')),
    granted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    question_id BLOB REFERENCES owner_questions (id),
    PRIMARY KEY (bot_id, scope)
) STRICT;

CREATE VIEW turn_holds AS
    SELECT root_turn_id AS turn_id, parent_attempt AS attempt,
           status IN ('queued', 'running') AS pending,
           delivered_attempt IS NULL AND status IN ('completed', 'failed', 'cancelled') AS unseen
    FROM delegations
    UNION ALL
    SELECT turn_id, attempt, status = 'pending', delivered_attempt IS NULL AND status <> 'pending'
    FROM secret_requests
    UNION ALL
    SELECT turn_id, attempt, status = 'pending', delivered_attempt IS NULL AND status <> 'pending'
    FROM owner_questions;

CREATE TRIGGER turns_end_owner_questions AFTER UPDATE OF status ON turns
WHEN OLD.status <> NEW.status AND NEW.status IN ('cancelling', 'completed', 'interrupted', 'failed', 'uncertain') BEGIN
    UPDATE owner_questions SET status = 'cancelled',
        settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE turn_id = NEW.id AND status = 'pending';
END;

DROP TRIGGER turns_delegation_child;
CREATE TRIGGER turns_delegation_child AFTER UPDATE OF status ON turns
WHEN OLD.status <> NEW.status AND NEW.status IN ('completed', 'interrupted', 'failed', 'uncertain') BEGIN
    UPDATE delegations SET
        status = CASE NEW.status WHEN 'completed' THEN 'completed' WHEN 'interrupted' THEN 'cancelled' ELSE 'failed' END,
        result = (SELECT group_concat(text, char(10) || char(10)) FROM
                  (SELECT text FROM items WHERE turn_id = NEW.id AND kind = 'assistant' ORDER BY seq)),
        error = CASE WHEN NEW.status = 'completed' THEN NULL ELSE coalesce(NEW.error_code || ': ' || NEW.error_message, NEW.status) END,
        finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE child_turn_id = NEW.id AND status NOT IN ('completed', 'failed', 'cancelled');
    UPDATE turns SET status = 'queued', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE id IN (SELECT root_turn_id FROM delegations WHERE child_turn_id = NEW.id) AND status = 'waiting'
      AND NOT EXISTS (SELECT 1 FROM turn_holds h WHERE h.turn_id = turns.id AND h.pending);
END;
