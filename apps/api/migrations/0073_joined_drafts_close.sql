-- A draft the owner sent with a message closes once both its reply and that
-- message have ended, in either order. A message that joined the reply can
-- end first (the owner stopped it), so the reply's own end closes the draft
-- too, instead of leaving a card nothing can approve.
DROP TRIGGER turns_end_reply_drafts;
CREATE TRIGGER turns_end_reply_drafts AFTER UPDATE OF status ON turns
WHEN OLD.status <> NEW.status AND NEW.status IN ('cancelling', 'completed', 'interrupted', 'failed', 'uncertain') BEGIN
    UPDATE outgoing_drafts SET status = 'discarded',
        result_detail = 'Your reply moved on without sending it.',
        updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
        settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE status = 'pending'
      AND ((context_turn_id = NEW.id
            AND turn_id IN (SELECT id FROM turns
                            WHERE status NOT IN ('queued', 'preparing', 'sending', 'running', 'waiting')))
           OR (turn_id = NEW.id AND NEW.status <> 'cancelling'
               AND context_turn_id IN (SELECT id FROM turns
                                       WHERE id <> NEW.id
                                         AND status NOT IN ('queued', 'preparing', 'sending', 'running', 'waiting', 'cancelling'))));
END;
