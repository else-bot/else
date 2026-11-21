-- The action a connected send's claim allocated. A retry of the claim names
-- it, and settling the draft never changes it, unlike `delivery`, which a
-- Sent-folder search or an earlier uncertain send rewrites to name the send
-- the receipt describes. A new approval clears it.
ALTER TABLE outgoing_drafts ADD COLUMN claim_action_id BLOB;

UPDATE outgoing_drafts
SET claim_action_id = unhex(replace(json_extract(delivery, '$.action_id'), '-', ''))
WHERE json_extract(delivery, '$.via') IN ('mail', 'social');

-- Every action a draft's connected send claimed. A connector admits a new
-- action named here only while its draft is still sending under that claim,
-- so a copy of a send still queued after the draft settled never sends.
CREATE TABLE draft_claims (
    account_id BLOB NOT NULL,
    action_id BLOB NOT NULL,
    draft_id BLOB NOT NULL REFERENCES outgoing_drafts (id),
    PRIMARY KEY (account_id, action_id)
) STRICT;

INSERT OR IGNORE INTO draft_claims (account_id, action_id, draft_id)
SELECT unhex(replace(json_extract(delivery, '$.account_id'), '-', '')), claim_action_id, id
FROM outgoing_drafts
WHERE claim_action_id IS NOT NULL;

-- A connected send its connector never admitted fails when its request ends:
-- admission refuses it from then on, so nothing can send it.
DROP TRIGGER turns_end_draft_sends;

CREATE TRIGGER turns_end_draft_sends AFTER UPDATE OF status ON turns
WHEN OLD.status <> NEW.status AND NEW.status IN ('cancelling', 'completed', 'interrupted', 'failed', 'uncertain') BEGIN
    UPDATE outgoing_drafts SET status = 'failed',
        result_detail = 'The request ended before the mail connector sent it. Nothing was sent.',
        updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
        settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE send_turn_id = NEW.id AND status = 'sending' AND json_extract(delivery, '$.via') = 'mail'
      AND NOT EXISTS (SELECT 1 FROM connector_actions a
                      WHERE lower(hex(a.account_id)) = replace(json_extract(delivery, '$.account_id'), '-', '')
                        AND lower(hex(a.action_id)) = replace(json_extract(delivery, '$.action_id'), '-', ''));
    UPDATE outgoing_drafts SET status = 'failed',
        result_detail = 'The request ended before the connection started sending it. Nothing was sent.',
        updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
        settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE send_turn_id = NEW.id AND status = 'sending' AND json_extract(delivery, '$.via') = 'social'
      AND NOT EXISTS (SELECT 1 FROM social_actions a
                      WHERE lower(hex(a.connection_id)) = replace(json_extract(delivery, '$.account_id'), '-', '')
                        AND lower(hex(a.action_id)) = replace(json_extract(delivery, '$.action_id'), '-', ''));
    UPDATE outgoing_drafts SET status = 'uncertain',
        result_detail = 'The request ended before the bot reported whether it was sent.',
        updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
        settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE send_turn_id = NEW.id AND status = 'sending';
    UPDATE outgoing_drafts SET status = 'failed',
        result_detail = 'The request ended before the bot sent it. Nothing was sent.',
        updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
        settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE send_turn_id = NEW.id AND status = 'approved';
END;
