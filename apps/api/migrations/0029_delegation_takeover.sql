ALTER TABLE delegations ADD COLUMN takeover_turn_id BLOB REFERENCES turns (id);
CREATE INDEX delegations_takeover ON delegations (takeover_turn_id) WHERE takeover_turn_id IS NOT NULL;

DROP TRIGGER turns_delegation_child;
CREATE TRIGGER turns_delegation_child AFTER UPDATE OF status ON turns
WHEN OLD.status <> NEW.status AND NEW.status IN ('completed', 'interrupted', 'failed', 'uncertain') BEGIN
    UPDATE delegations SET
        status = CASE NEW.status WHEN 'completed' THEN 'completed' WHEN 'interrupted' THEN 'cancelled' ELSE 'failed' END,
        result = (SELECT group_concat(text, char(10) || char(10)) FROM
                  (SELECT text FROM items WHERE turn_id = NEW.id AND kind = 'assistant' ORDER BY seq)),
        error = CASE WHEN NEW.status = 'completed' THEN NULL ELSE coalesce(NEW.error_code || ': ' || NEW.error_message, NEW.status) END,
        finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE ((child_turn_id = NEW.id AND takeover_turn_id IS NULL) OR takeover_turn_id = NEW.id)
      AND status NOT IN ('completed', 'failed', 'cancelled');
    UPDATE turns SET status = 'queued', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE id IN (SELECT root_turn_id FROM delegations
                 WHERE (child_turn_id = NEW.id AND takeover_turn_id IS NULL) OR takeover_turn_id = NEW.id)
      AND status = 'waiting'
      AND NOT EXISTS (SELECT 1 FROM turn_holds h WHERE h.turn_id = turns.id AND h.pending);
END;
