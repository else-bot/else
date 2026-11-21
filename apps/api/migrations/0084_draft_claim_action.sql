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
