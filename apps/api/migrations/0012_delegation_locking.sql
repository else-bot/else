-- POC-016 review: settle a child against its root under the root turn's
-- row lock. The parent's finish takes the same lock before deciding to
-- park, so a child that ends meanwhile can never leave the root waiting.
-- Lock order: root turn, then delegation.
CREATE OR REPLACE FUNCTION delegation_child_ended() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE d delegations%ROWTYPE;
DECLARE root_id uuid;
BEGIN
    SELECT root_turn_id INTO root_id FROM delegations WHERE child_turn_id = NEW.id;
    IF NOT FOUND THEN
        RETURN NEW;
    END IF;
    PERFORM 1 FROM turns WHERE id = root_id FOR UPDATE;
    SELECT * INTO d FROM delegations WHERE child_turn_id = NEW.id FOR UPDATE;
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
