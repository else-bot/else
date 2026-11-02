-- Asynchronous one-hop delegation (POC-016). See
-- docs/research/prompts-and-delegation.md#one-hop-delegation.

-- A bot keeps one owner conversation; delegated work runs in separate task
-- conversations so it never appears in the owner's chat with that bot.
ALTER TABLE conversations
    ADD COLUMN kind text NOT NULL DEFAULT 'owner' CHECK (kind IN ('owner', 'task'));
ALTER TABLE conversations DROP CONSTRAINT conversations_bot_id_key;
CREATE UNIQUE INDEX conversations_one_owner ON conversations (bot_id) WHERE kind = 'owner';

-- A root owner turn parks as `waiting` while its children work: no provider
-- attempt is active, yet the owner task is still in progress. `attempt`
-- counts provider attempts of the turn (a continuation is a new attempt).
ALTER TABLE turns DROP CONSTRAINT turns_status_check;
ALTER TABLE turns ADD CONSTRAINT turns_status_check CHECK (status IN (
    'queued', 'preparing', 'sending', 'running', 'cancelling', 'waiting',
    'completed', 'interrupted', 'failed', 'uncertain'));
ALTER TABLE turns ADD COLUMN attempt integer NOT NULL DEFAULT 0 CHECK (attempt >= 0);

-- The delegated task text enters the target's memory as model-authored.
ALTER TABLE memory_entry DROP CONSTRAINT memory_entry_kind_check;
ALTER TABLE memory_entry ADD CONSTRAINT memory_entry_kind_check CHECK (kind IN (
    'owner_message', 'assistant_message', 'owner_correction', 'tool_activity', 'delegated_task'));

CREATE TABLE delegations (
    id uuid PRIMARY KEY,
    -- The owner turn of the delegating (main) bot, and its attempt.
    root_turn_id uuid NOT NULL REFERENCES turns (id),
    parent_attempt integer NOT NULL,
    parent_bot_id uuid NOT NULL REFERENCES bots (id),
    target_bot_id uuid NOT NULL REFERENCES bots (id),
    task text NOT NULL CHECK (btrim(task) <> '' AND octet_length(task) <= 8000),
    -- A retried tool call with the same task returns the same child.
    task_hash bytea NOT NULL CHECK (octet_length(task_hash) = 32),
    conversation_id uuid NOT NULL REFERENCES conversations (id),
    child_turn_id uuid NOT NULL UNIQUE REFERENCES turns (id),
    status text NOT NULL DEFAULT 'queued'
        CHECK (status IN ('queued', 'running', 'completed', 'failed', 'cancelled')),
    result text,
    error text,
    -- The parent attempt that received the outcome.
    delivered_attempt integer,
    created_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz,
    UNIQUE (root_turn_id, task_hash),
    CHECK (parent_bot_id <> target_bot_id)
);
CREATE INDEX delegations_root ON delegations (root_turn_id);

-- A child turn's terminal status settles its delegation and requeues a
-- waiting root as a continuation, whichever code path ended the child
-- (completion, stop, deletion, restart reconciliation).
CREATE FUNCTION delegation_child_ended() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE d delegations%ROWTYPE;
BEGIN
    SELECT * INTO d FROM delegations WHERE child_turn_id = NEW.id FOR UPDATE;
    IF NOT FOUND THEN
        RETURN NEW;
    END IF;
    IF NEW.status IN ('preparing', 'sending', 'running') AND d.status = 'queued' THEN
        UPDATE delegations SET status = 'running' WHERE id = d.id;
        RETURN NEW;
    END IF;
    IF NEW.status NOT IN ('completed', 'interrupted', 'failed', 'uncertain')
       OR d.status IN ('completed', 'failed', 'cancelled') THEN
        RETURN NEW;
    END IF;
    UPDATE delegations SET
        status = CASE NEW.status WHEN 'completed' THEN 'completed'
                                 WHEN 'interrupted' THEN 'cancelled' ELSE 'failed' END,
        result = (SELECT string_agg(i.text, E'\n\n' ORDER BY i.seq) FROM items i
                  WHERE i.turn_id = NEW.id AND i.kind = 'assistant'),
        error = CASE WHEN NEW.status = 'completed' THEN NULL
                     ELSE coalesce(NEW.error_code || ': ' || NEW.error_message, NEW.status) END,
        finished_at = now()
    WHERE id = d.id;
    UPDATE turns SET status = 'queued', updated_at = now()
    WHERE id = d.root_turn_id AND status = 'waiting';
    RETURN NEW;
END $$;
CREATE TRIGGER turns_delegation_child AFTER UPDATE OF status ON turns
    FOR EACH ROW WHEN (OLD.status IS DISTINCT FROM NEW.status)
    EXECUTE FUNCTION delegation_child_ended();
