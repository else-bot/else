-- A bot's request for credentials the owner types into the conversation.
-- Only metadata is stored here: the values go straight into task_secrets,
-- encrypted, in the same transaction that marks the request stored.
CREATE TABLE secret_requests (
    id BLOB NOT NULL PRIMARY KEY,
    bot_id BLOB NOT NULL REFERENCES bots (id),
    turn_id BLOB NOT NULL REFERENCES turns (id),
    attempt INTEGER NOT NULL,
    name text NOT NULL CHECK (length(name) BETWEEN 1 AND 40 AND substr(name, 1, 1) GLOB '[a-z]' AND name NOT GLOB '*[^a-z0-9_-]*'),
    reason text NOT NULL CHECK (trim(reason) <> '' AND length(CAST(reason AS BLOB)) <= 500),
    env_names TEXT NOT NULL CHECK (json_valid(env_names)),
    status text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'stored', 'cancelled')),
    delivered_attempt INTEGER,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    settled_at TEXT,
    UNIQUE (turn_id, name)
) STRICT;

CREATE INDEX secret_requests_turn ON secret_requests (turn_id);
CREATE UNIQUE INDEX secret_requests_pending ON secret_requests (turn_id) WHERE status = 'pending';

-- A request outlives no turn: whatever ends the turn (Stop, deletion,
-- failure, restart reconciliation) withdraws what is still pending.
CREATE TRIGGER turns_end_secret_requests AFTER UPDATE OF status ON turns
WHEN OLD.status <> NEW.status AND NEW.status IN ('cancelling', 'completed', 'interrupted', 'failed', 'uncertain') BEGIN
    UPDATE secret_requests SET status = 'cancelled',
        settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE turn_id = NEW.id AND status = 'pending';
END;

-- A root waiting on the owner's secret too stays waiting when a child ends.
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
      AND NOT EXISTS (SELECT 1 FROM delegations d WHERE d.root_turn_id = turns.id AND d.status IN ('queued', 'running'))
      AND NOT EXISTS (SELECT 1 FROM secret_requests r WHERE r.turn_id = turns.id AND r.status = 'pending');
END;
