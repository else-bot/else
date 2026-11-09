-- An owner message sent with a pending draft open hands that draft to its
-- request. When the request ends without the bot revising or confirming it,
-- and the request that drafted it cannot take the Approve button any more,
-- the owner's reply moved on: the draft closes.
CREATE INDEX outgoing_drafts_context_turn ON outgoing_drafts (context_turn_id)
    WHERE context_turn_id IS NOT NULL;

CREATE TRIGGER turns_end_reply_drafts AFTER UPDATE OF status ON turns
WHEN OLD.status <> NEW.status AND NEW.status IN ('cancelling', 'completed', 'interrupted', 'failed', 'uncertain') BEGIN
    UPDATE outgoing_drafts SET status = 'discarded',
        result_detail = 'Your reply moved on without sending it.',
        updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
        settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE context_turn_id = NEW.id AND status = 'pending'
      AND turn_id IN (SELECT id FROM turns
                      WHERE status NOT IN ('queued', 'preparing', 'sending', 'running', 'waiting'));
END;

UPDATE outgoing_drafts SET status = 'discarded',
    result_detail = 'Your reply moved on without sending it.',
    updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
    settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
WHERE status = 'pending'
  AND context_turn_id IN (SELECT id FROM turns
                          WHERE status IN ('cancelling', 'completed', 'interrupted', 'failed', 'uncertain'))
  AND turn_id IN (SELECT id FROM turns
                  WHERE status NOT IN ('queued', 'preparing', 'sending', 'running', 'waiting'));
