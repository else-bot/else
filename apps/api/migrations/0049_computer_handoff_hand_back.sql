-- A computer handoff now holds its turn, like a question: the bot ends its
-- attempt and the turn waits until the owner hands the computer back or
-- declines. Taking over only puts the owner in control ('taken'); the turn
-- continues once they press Hand back ('handed_back'). A request the owner
-- declines is 'dismissed', one a newer owner message or request replaces is
-- 'skipped', and one whose turn ended first is 'cancelled'. A 'taken'
-- request outlives its turn: the owner still holds the computer until they
-- hand it back.
--
-- Earlier rows never held a turn. They keep attempt 0 and count as already
-- delivered; an old 'taken' row was never confirmed, so it becomes
-- 'cancelled' rather than claiming the owner finished.
CREATE TABLE computer_handoffs_new (
    id BLOB NOT NULL PRIMARY KEY,
    bot_id BLOB NOT NULL REFERENCES bots (id),
    turn_id BLOB NOT NULL REFERENCES turns (id),
    attempt INTEGER NOT NULL,
    reason TEXT NOT NULL CHECK (trim(reason) <> '' AND length(CAST(reason AS BLOB)) <= 200),
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'taken', 'handed_back', 'dismissed', 'skipped', 'cancelled')),
    delivered_attempt INTEGER,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    taken_at TEXT,
    settled_at TEXT,
    CHECK ((status IN ('taken', 'handed_back')) = (taken_at IS NOT NULL))
) STRICT;

INSERT INTO computer_handoffs_new
    (id, bot_id, turn_id, attempt, reason, status, delivered_attempt, created_at, taken_at, settled_at)
SELECT id, bot_id, turn_id, 0, reason,
       CASE status WHEN 'taken' THEN 'cancelled' ELSE status END,
       0, created_at, NULL, settled_at
FROM computer_handoffs;

DROP TABLE computer_handoffs;
ALTER TABLE computer_handoffs_new RENAME TO computer_handoffs;

CREATE INDEX computer_handoffs_turn ON computer_handoffs (turn_id);
CREATE UNIQUE INDEX computer_handoffs_open ON computer_handoffs (bot_id)
    WHERE status IN ('pending', 'taken');

-- Whatever ends the turn withdraws a request the owner has not acted on.
CREATE TRIGGER turns_end_computer_handoffs AFTER UPDATE OF status ON turns
WHEN OLD.status <> NEW.status AND NEW.status IN ('cancelling', 'completed', 'interrupted', 'failed', 'uncertain') BEGIN
    UPDATE computer_handoffs SET status = 'cancelled',
        settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE turn_id = NEW.id AND status = 'pending' AND attempt > 0;
END;

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
    FROM skill_installs
    UNION ALL
    SELECT turn_id, attempt, status IN ('pending', 'taken'),
           delivered_attempt IS NULL AND status IN ('handed_back', 'dismissed', 'skipped')
    FROM computer_handoffs WHERE attempt > 0;
