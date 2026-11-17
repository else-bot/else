-- Every mail action that sent, or tried to send, a connected draft. The
-- action's receipt in connector_actions holds its outcome, phase and reason.
-- `sent_check` records a search of the Sent folder for an uncertain send
-- before the draft may be sent again.
CREATE TABLE draft_mail_sends (
    account_id BLOB NOT NULL,
    action_id BLOB NOT NULL,
    draft_id BLOB NOT NULL REFERENCES outgoing_drafts (id),
    -- NULL for sends made before drafts carried a stable Message-ID.
    message_id TEXT CHECK (message_id IS NULL OR length(message_id) BETWEEN 3 AND 255),
    sent_check TEXT CHECK (sent_check IN ('found', 'absent')),
    checked_at TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    PRIMARY KEY (account_id, action_id),
    CHECK ((sent_check IS NULL) = (checked_at IS NULL))
) STRICT;

CREATE INDEX draft_mail_sends_draft ON draft_mail_sends (draft_id, created_at);

INSERT OR IGNORE INTO draft_mail_sends (account_id, action_id, draft_id, created_at)
SELECT unhex(replace(json_extract(delivery, '$.account_id'), '-', '')),
       unhex(replace(json_extract(delivery, '$.action_id'), '-', '')),
       id, updated_at
FROM outgoing_drafts
WHERE json_extract(delivery, '$.via') = 'mail';

DROP TRIGGER connector_actions_settle_drafts;

CREATE TRIGGER connector_actions_settle_drafts AFTER UPDATE OF status ON connector_actions
WHEN OLD.status <> NEW.status AND NEW.status <> 'dispatching' BEGIN
    UPDATE outgoing_drafts SET
        status = CASE NEW.status WHEN 'succeeded' THEN 'sent' ELSE NEW.status END,
        result_detail = CASE NEW.status
            WHEN 'succeeded' THEN 'The mail server accepted it from the connected account.'
            WHEN 'failed' THEN 'Nothing was sent: '
                || substr(coalesce(json_extract(NEW.result, '$.reason'), 'the mail server refused it'), 1, 600)
                || '.'
            ELSE 'It may have been sent: '
                || substr(coalesce(json_extract(NEW.result, '$.reason'), 'the mail connector could not confirm delivery'), 1, 600)
                || '. Before it is sent again, its Sent folder is searched for it.'
        END,
        updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
        settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE status IN ('sending', 'uncertain') AND json_extract(delivery, '$.via') = 'mail'
      AND replace(json_extract(delivery, '$.account_id'), '-', '') = lower(hex(NEW.account_id))
      AND replace(json_extract(delivery, '$.action_id'), '-', '') = lower(hex(NEW.action_id));
END;

-- A connected email that fails while an earlier send of it may have gone
-- out stays uncertain, naming that send, whatever failed it: a refused
-- retry, a changed attachment, or the end of its request. An uncertain
-- draft cannot be revised into a second message.
CREATE TRIGGER outgoing_drafts_keep_uncertain AFTER UPDATE OF status ON outgoing_drafts
WHEN NEW.status = 'failed' AND EXISTS (
    SELECT 1 FROM draft_mail_sends s JOIN connector_actions a
      ON a.account_id = s.account_id AND a.action_id = s.action_id
    WHERE s.draft_id = NEW.id AND a.status IN ('uncertain', 'dispatching'))
BEGIN
    UPDATE outgoing_drafts SET status = 'uncertain',
        delivery = (
            SELECT json_object('via', 'mail',
                'account_id', lower(substr(hex(s.account_id), 1, 8) || '-' || substr(hex(s.account_id), 9, 4) || '-'
                    || substr(hex(s.account_id), 13, 4) || '-' || substr(hex(s.account_id), 17, 4) || '-'
                    || substr(hex(s.account_id), 21)),
                'action_id', lower(substr(hex(s.action_id), 1, 8) || '-' || substr(hex(s.action_id), 9, 4) || '-'
                    || substr(hex(s.action_id), 13, 4) || '-' || substr(hex(s.action_id), 17, 4) || '-'
                    || substr(hex(s.action_id), 21)))
            FROM draft_mail_sends s JOIN connector_actions a
              ON a.account_id = s.account_id AND a.action_id = s.action_id
            WHERE s.draft_id = NEW.id AND a.status IN ('uncertain', 'dispatching')
            ORDER BY s.created_at DESC, s.rowid DESC LIMIT 1),
        result_detail = substr(coalesce(NEW.result_detail || ' ', '')
            || 'An earlier send of it may have gone out.', 1, 2000)
    WHERE id = NEW.id;
END;
