-- See docs/decisions/0003-email-drafts.md.
CREATE TABLE email_drafts (
    id BLOB NOT NULL PRIMARY KEY,
    email_id BLOB NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 1),
    bot_id BLOB NOT NULL REFERENCES bots (id),
    turn_id BLOB NOT NULL REFERENCES turns (id),
    attempt INTEGER NOT NULL,
    sender text NOT NULL CHECK (length(CAST(sender AS BLOB)) BETWEEN 3 AND 320),
    recipients TEXT NOT NULL CHECK (json_valid(recipients) AND json_type(recipients) = 'object'),
    subject text NOT NULL CHECK (length(CAST(subject AS BLOB)) <= 998),
    body text NOT NULL CHECK (trim(body) <> '' AND length(CAST(body AS BLOB)) <= 100000),
    version INTEGER NOT NULL DEFAULT 1 CHECK (version >= 1),
    status text NOT NULL DEFAULT 'pending' CHECK (status IN
        ('pending', 'approved', 'sending', 'sent', 'failed', 'uncertain', 'discarded', 'superseded')),
    approval_source text CHECK (approval_source IN ('button', 'message')),
    approved_version INTEGER,
    approved_item_id BLOB REFERENCES items (id),
    approved_at TEXT,
    send_turn_id BLOB REFERENCES turns (id),
    delivery TEXT CHECK (delivery IS NULL OR json_valid(delivery)),
    result_detail text CHECK (result_detail IS NULL OR length(CAST(result_detail AS BLOB)) <= 2000),
    context_turn_id BLOB REFERENCES turns (id),
    context_version INTEGER,
    superseded_by BLOB,
    delivered_attempt INTEGER,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    settled_at TEXT,
    UNIQUE (email_id, revision),
    CHECK ((approval_source IS NULL) = (approved_version IS NULL)),
    CHECK ((approval_source = 'message') = (approved_item_id IS NOT NULL)),
    CHECK (status NOT IN ('approved', 'sending', 'sent') OR (approval_source IS NOT NULL AND send_turn_id IS NOT NULL)),
    CHECK (status NOT IN ('sending', 'sent', 'uncertain') OR delivery IS NOT NULL),
    CHECK ((status = 'superseded') = (superseded_by IS NOT NULL))
) STRICT;

CREATE INDEX email_drafts_turn ON email_drafts (turn_id);
CREATE INDEX email_drafts_send_turn ON email_drafts (send_turn_id) WHERE send_turn_id IS NOT NULL;
CREATE UNIQUE INDEX email_drafts_live ON email_drafts (email_id) WHERE status <> 'superseded';

DROP VIEW turn_holds;
CREATE VIEW turn_holds AS
    SELECT root_turn_id AS turn_id, parent_attempt AS attempt,
           status IN ('queued', 'running') AS pending,
           delivered_attempt IS NULL AND status IN ('completed', 'failed', 'cancelled') AS unseen
    FROM delegations
    UNION ALL
    SELECT turn_id, attempt, status = 'pending', delivered_attempt IS NULL AND status <> 'pending'
    FROM secret_requests
    UNION ALL
    SELECT turn_id, attempt, status = 'pending', delivered_attempt IS NULL AND status <> 'pending'
    FROM owner_questions
    UNION ALL
    SELECT turn_id, attempt, status = 'pending',
           delivered_attempt IS NULL AND status IN ('approved', 'discarded')
    FROM email_drafts WHERE approval_source IS NOT 'message';

CREATE TRIGGER turns_end_email_sends AFTER UPDATE OF status ON turns
WHEN OLD.status <> NEW.status AND NEW.status IN ('cancelling', 'completed', 'interrupted', 'failed', 'uncertain') BEGIN
    UPDATE email_drafts SET status = 'uncertain',
        result_detail = 'The request ended before the bot reported whether the email was sent.',
        updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
        settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE send_turn_id = NEW.id AND status = 'sending';
    UPDATE email_drafts SET status = 'failed',
        result_detail = 'The request ended before the bot sent the email. Nothing was sent.',
        updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
        settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE send_turn_id = NEW.id AND status = 'approved';
END;
