-- A delegating turn wakes once any batch of its tasks has fully settled,
-- instead of only after every task. A batch is the tasks one provider
-- attempt sent (one individual task, or a council); a task carried over
-- from an earlier owner message (attempt 0) is a batch of its own. Other
-- holds (secret requests, questions, drafts, social setups, MCP waits)
-- still keep the turn waiting.
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
      AND (SELECT coalesce(sum(h.pending), 0) FROM turn_holds h WHERE h.turn_id = turns.id)
          = (SELECT count(*) FROM delegations p
             WHERE p.root_turn_id = turns.id AND p.status IN ('queued', 'running'))
      AND EXISTS (SELECT 1 FROM delegations d
                  WHERE d.root_turn_id = turns.id AND d.delivered_attempt IS NULL
                    AND d.status IN ('completed', 'failed', 'cancelled')
                    AND NOT EXISTS (SELECT 1 FROM delegations p
                                    WHERE p.root_turn_id = d.root_turn_id
                                      AND p.status IN ('queued', 'running')
                                      AND d.parent_attempt > 0
                                      AND p.parent_attempt = d.parent_attempt));
END;
