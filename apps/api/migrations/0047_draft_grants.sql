-- "Approve always" on a draft card: later drafts of the same kind (bot,
-- platform, sending account, destination) are approved when proposed, with
-- approval_source 'standing'. See app::outgoing::DraftKind.

-- Triggers and the turn_holds view name outgoing_drafts in their bodies; the
-- legacy rename keeps them pointing at the rebuilt table.
PRAGMA legacy_alter_table = ON;

CREATE TABLE outgoing_drafts_new (
    id BLOB NOT NULL PRIMARY KEY,
    lineage_id BLOB NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 1),
    bot_id BLOB NOT NULL REFERENCES bots (id),
    turn_id BLOB NOT NULL REFERENCES turns (id),
    attempt INTEGER NOT NULL,
    -- The typed content (app::outgoing::Content), tagged by channel.
    content TEXT NOT NULL CHECK (json_valid(content) AND json_type(content) = 'object'
        AND json_extract(content, '$.channel') IN ('email', 'message')
        AND json_type(content, '$.body') = 'text'
        AND trim(json_extract(content, '$.body')) <> ''
        AND length(CAST(content AS BLOB)) <= 200000),
    version INTEGER NOT NULL DEFAULT 1 CHECK (version >= 1),
    status text NOT NULL DEFAULT 'pending' CHECK (status IN
        ('pending', 'approved', 'sending', 'sent', 'failed', 'uncertain', 'discarded', 'superseded')),
    approval_source text CHECK (approval_source IN ('button', 'message', 'standing')),
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
    UNIQUE (lineage_id, revision),
    CHECK ((approval_source IS NULL) = (approved_version IS NULL)),
    CHECK ((approval_source = 'message') = (approved_item_id IS NOT NULL)),
    CHECK (status NOT IN ('approved', 'sending', 'sent') OR (approval_source IS NOT NULL AND send_turn_id IS NOT NULL)),
    CHECK (status NOT IN ('sending', 'sent', 'uncertain') OR delivery IS NOT NULL),
    CHECK ((status = 'superseded') = (superseded_by IS NOT NULL))
) STRICT;

INSERT INTO outgoing_drafts_new SELECT * FROM outgoing_drafts;
DROP TABLE outgoing_drafts;
ALTER TABLE outgoing_drafts_new RENAME TO outgoing_drafts;

PRAGMA legacy_alter_table = OFF;

CREATE INDEX outgoing_drafts_turn ON outgoing_drafts (turn_id);
CREATE INDEX outgoing_drafts_send_turn ON outgoing_drafts (send_turn_id) WHERE send_turn_id IS NOT NULL;
CREATE UNIQUE INDEX outgoing_drafts_live ON outgoing_drafts (lineage_id) WHERE status <> 'superseded';
CREATE INDEX outgoing_drafts_context_turn ON outgoing_drafts (context_turn_id)
    WHERE context_turn_id IS NOT NULL;

CREATE TRIGGER outgoing_drafts_fixed_content BEFORE UPDATE OF content ON outgoing_drafts
WHEN json_remove(OLD.content, '$.subject', '$.body') IS NOT json_remove(NEW.content, '$.subject', '$.body')
BEGIN
    SELECT RAISE(ABORT, 'only the body and an email subject of a draft can change');
END;

-- One row per kind the owner approved always; `draft_id` is the card that
-- granted it. Deleting the row stops it.
CREATE TABLE draft_grants (
    bot_id BLOB NOT NULL REFERENCES bots (id),
    kind TEXT NOT NULL CHECK (json_valid(kind) AND json_type(kind) = 'object'),
    draft_id BLOB NOT NULL REFERENCES outgoing_drafts (id),
    granted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    PRIMARY KEY (bot_id, kind)
) STRICT;

-- A standing approval waits on no one, so it holds no attempt: the bot may
-- go on to draft and send more in the same request.
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
    FROM owner_questions WHERE json_type(action, '$.permission') IS NULL
    UNION ALL
    SELECT turn_id, attempt, status = 'pending',
           delivered_attempt IS NULL AND status IN ('approved', 'discarded')
    FROM outgoing_drafts WHERE approval_source IS NOT 'message' AND approval_source IS NOT 'standing'
    UNION ALL
    SELECT turn_id, attempt, status = 'pending', delivered_attempt IS NULL AND status <> 'pending'
    FROM social_setups
    UNION ALL
    SELECT turn_id, attempt, status = 'pending',
           delivered_attempt IS NULL AND status IN ('finished', 'failed', 'expired')
    FROM mcp_waits
    UNION ALL
    SELECT turn_id, attempt, status = 'pending',
           delivered_attempt IS NULL AND status IN ('installed', 'declined')
    FROM skill_installs;
