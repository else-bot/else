-- The owner can stop one wait on its own. A `stopped` wait is an outcome
-- the bot receives, unlike `cancelled`, which withdraws a wait with its turn
-- and is never delivered.
--
-- The owner's Stop ends only the attempt that is generating: it moves the
-- turn to `cancelling` with error code `stopped`, and its pending waits stay
-- so the turn can park on them. Every other move to `cancelling` (a task
-- cancelled by its sender, a turn taken over by the owner, bot deletion)
-- still withdraws them.
--
-- Triggers and the turn_holds view name both tables in their bodies; the
-- legacy rename keeps them pointing at the rebuilt tables.
PRAGMA legacy_alter_table = ON;

CREATE TABLE mcp_waits_new (
    id BLOB PRIMARY KEY NOT NULL,
    bot_id BLOB NOT NULL REFERENCES bots (id),
    turn_id BLOB NOT NULL REFERENCES turns (id),
    attempt INTEGER NOT NULL,
    mcp_id TEXT NOT NULL,
    tool TEXT NOT NULL,
    arguments TEXT NOT NULL CHECK (json_valid(arguments)),
    status_pointer TEXT NOT NULL,
    running_values TEXT NOT NULL CHECK (json_valid(running_values)),
    purpose TEXT NOT NULL,
    interval_seconds INTEGER NOT NULL CHECK (interval_seconds BETWEEN 10 AND 3600),
    deadline_at TEXT NOT NULL,
    next_check_at TEXT NOT NULL,
    checks INTEGER NOT NULL DEFAULT 1,
    failures INTEGER NOT NULL DEFAULT 0,
    last_result TEXT,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'finished', 'failed', 'expired', 'cancelled', 'stopped')),
    delivered_attempt INTEGER,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    settled_at TEXT
) STRICT;

INSERT INTO mcp_waits_new SELECT * FROM mcp_waits;
DROP TABLE mcp_waits;
ALTER TABLE mcp_waits_new RENAME TO mcp_waits;

CREATE UNIQUE INDEX mcp_waits_attempt ON mcp_waits (turn_id, attempt) WHERE attempt > 0;
CREATE INDEX mcp_waits_due ON mcp_waits (next_check_at) WHERE status = 'pending';

CREATE TABLE computer_job_waits_new (
    id BLOB PRIMARY KEY NOT NULL,
    bot_id BLOB NOT NULL REFERENCES bots (id),
    turn_id BLOB NOT NULL REFERENCES turns (id),
    attempt INTEGER NOT NULL,
    job_id BLOB NOT NULL REFERENCES guest_jobs (id),
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'settled', 'cancelled', 'stopped')),
    delivered_attempt INTEGER,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    settled_at TEXT
) STRICT;

INSERT INTO computer_job_waits_new SELECT * FROM computer_job_waits;
DROP TABLE computer_job_waits;
ALTER TABLE computer_job_waits_new RENAME TO computer_job_waits;

PRAGMA legacy_alter_table = OFF;

CREATE UNIQUE INDEX computer_job_waits_attempt ON computer_job_waits (turn_id, attempt)
    WHERE attempt > 0;
CREATE INDEX computer_job_waits_job ON computer_job_waits (job_id) WHERE status = 'pending';

DROP TRIGGER turns_end_mcp_waits;
CREATE TRIGGER turns_end_mcp_waits AFTER UPDATE OF status ON turns
WHEN OLD.status <> NEW.status
 AND NEW.status IN ('cancelling', 'completed', 'interrupted', 'failed', 'uncertain')
 AND NOT (NEW.status = 'cancelling' AND NEW.error_code IS 'stopped') BEGIN
    UPDATE mcp_waits SET status = 'cancelled',
        settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE turn_id = NEW.id AND status = 'pending';
END;

DROP TRIGGER turns_end_computer_job_waits;
CREATE TRIGGER turns_end_computer_job_waits AFTER UPDATE OF status ON turns
WHEN OLD.status <> NEW.status
 AND NEW.status IN ('cancelling', 'completed', 'interrupted', 'failed', 'uncertain')
 AND NOT (NEW.status = 'cancelling' AND NEW.error_code IS 'stopped') BEGIN
    UPDATE computer_job_waits SET status = 'cancelled',
        settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE turn_id = NEW.id AND status = 'pending';
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
           delivered_attempt IS NULL AND status IN ('finished', 'failed', 'expired', 'stopped')
    FROM mcp_waits
    UNION ALL
    SELECT turn_id, attempt, status = 'pending',
           delivered_attempt IS NULL AND status IN ('installed', 'declined')
    FROM skill_installs
    UNION ALL
    SELECT turn_id, attempt, status IN ('pending', 'taken'),
           delivered_attempt IS NULL AND status IN ('handed_back', 'dismissed', 'skipped')
    FROM computer_handoffs WHERE attempt > 0
    UNION ALL
    SELECT turn_id, attempt, status = 'pending',
           delivered_attempt IS NULL AND status IN ('settled', 'stopped')
    FROM computer_job_waits;
