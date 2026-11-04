CREATE TABLE routines (
    id BLOB NOT NULL PRIMARY KEY,
    bot_id BLOB NOT NULL REFERENCES bots (id),
    conversation_id BLOB NOT NULL REFERENCES conversations (id),
    name text NOT NULL CHECK (length(name) BETWEEN 1 AND 80 AND name = trim(name)),
    prompt text NOT NULL CHECK (trim(prompt) <> '' AND length(CAST(prompt AS BLOB)) <= 8000),
    cron text NOT NULL CHECK (cron <> '' AND length(CAST(cron AS BLOB)) <= 100),
    timezone text NOT NULL CHECK (timezone <> '' AND length(CAST(timezone AS BLOB)) <= 64),
    enabled INTEGER NOT NULL DEFAULT true,
    next_due_at TEXT,
    created_turn_id BLOB NOT NULL REFERENCES turns (id),
    created_name text NOT NULL,
    request_hash BLOB NOT NULL CHECK (length(CAST(request_hash AS BLOB)) = 32),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    deleted_at TEXT,
    UNIQUE (created_turn_id, created_name),
    CHECK (next_due_at IS NULL OR (enabled AND deleted_at IS NULL))
) STRICT;

CREATE UNIQUE INDEX routines_name ON routines (bot_id, name) WHERE deleted_at IS NULL;
CREATE INDEX routines_due ON routines (next_due_at) WHERE next_due_at IS NOT NULL;

CREATE TABLE routine_runs (
    id BLOB NOT NULL PRIMARY KEY,
    routine_id BLOB NOT NULL REFERENCES routines (id),
    due_at TEXT NOT NULL,
    name text NOT NULL,
    turn_id BLOB UNIQUE REFERENCES turns (id),
    skip_reason text CHECK (skip_reason IN ('overlap', 'queue_full', 'provider_unavailable')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    UNIQUE (routine_id, due_at),
    CHECK ((turn_id IS NULL) <> (skip_reason IS NULL))
) STRICT;

CREATE INDEX routine_runs_routine ON routine_runs (routine_id, due_at);

-- A run's prompt is a hidden 'routine' item: rebuild items to allow the
-- kind. The delegation trigger reads items, so it is dropped around the
-- rebuild and recreated exactly as 0002_secret_requests defined it.
DROP TRIGGER turns_delegation_child;

CREATE TABLE items_new (
    id BLOB NOT NULL PRIMARY KEY,
    conversation_id BLOB NOT NULL REFERENCES conversations (id),
    seq INTEGER NOT NULL CHECK (seq >= 1),
    turn_id BLOB NOT NULL REFERENCES turns (id),
    kind text NOT NULL CHECK (kind IN ('user', 'assistant', 'status', 'routine')),
    text text NOT NULL,
    provider_id text,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    UNIQUE (conversation_id, seq)
) STRICT;

INSERT INTO items_new (id, conversation_id, seq, turn_id, kind, text, provider_id, created_at)
SELECT id, conversation_id, seq, turn_id, kind, text, provider_id, created_at FROM items;

DROP TABLE items;
ALTER TABLE items_new RENAME TO items;
CREATE INDEX items_turn ON items (turn_id);

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
