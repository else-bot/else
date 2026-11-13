-- A bot's wait on one of its background computer jobs (`computer_job` with
-- wait). The attempt ends and its turn waits with no provider process. The
-- job's own end settles the wait, whatever ended it: an exit, a cancel, the
-- job deadline, or a lost computer. One wait per attempt; a continuation
-- that waits again gets a new row. Ending the turn first withdraws it.
CREATE TABLE computer_job_waits (
    id BLOB PRIMARY KEY NOT NULL,
    bot_id BLOB NOT NULL REFERENCES bots (id),
    turn_id BLOB NOT NULL REFERENCES turns (id),
    attempt INTEGER NOT NULL,
    job_id BLOB NOT NULL REFERENCES guest_jobs (id),
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'settled', 'cancelled')),
    delivered_attempt INTEGER,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    settled_at TEXT,
    UNIQUE (turn_id, attempt)
) STRICT;

CREATE INDEX computer_job_waits_job ON computer_job_waits (job_id) WHERE status = 'pending';

-- Settle each wait on the job and queue its waiting turn as a continuation
-- when nothing else holds it, by the rule `turns::continue_waiting` applies.
CREATE TRIGGER guest_jobs_settle_waits AFTER UPDATE OF status ON guest_jobs
WHEN OLD.status <> NEW.status AND NEW.status IN ('completed', 'failed', 'cancelled', 'lost') BEGIN
    UPDATE computer_job_waits SET status = 'settled',
        settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE job_id = NEW.id AND status = 'pending';
    UPDATE turns SET status = 'queued', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE status = 'waiting'
      AND id IN (SELECT turn_id FROM computer_job_waits
                 WHERE job_id = NEW.id AND status = 'settled' AND delivered_attempt IS NULL)
      AND (SELECT coalesce(sum(h.pending), 0) FROM turn_holds h WHERE h.turn_id = turns.id)
          = (SELECT count(*) FROM delegations p
             WHERE p.root_turn_id = turns.id AND p.status IN ('queued', 'running'))
      AND (NOT EXISTS (SELECT 1 FROM delegations p
                       WHERE p.root_turn_id = turns.id AND p.status IN ('queued', 'running'))
           OR EXISTS (SELECT 1 FROM delegations d
                      WHERE d.root_turn_id = turns.id AND d.delivered_attempt IS NULL
                        AND d.status IN ('completed', 'failed', 'cancelled')
                        AND NOT EXISTS (SELECT 1 FROM delegations p
                                        WHERE p.root_turn_id = d.root_turn_id
                                          AND p.status IN ('queued', 'running')
                                          AND d.parent_attempt > 0
                                          AND p.parent_attempt = d.parent_attempt)));
END;

CREATE TRIGGER turns_end_computer_job_waits AFTER UPDATE OF status ON turns
WHEN OLD.status <> NEW.status AND NEW.status IN ('cancelling', 'completed', 'interrupted', 'failed', 'uncertain') BEGIN
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
           delivered_attempt IS NULL AND status IN ('finished', 'failed', 'expired')
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
    SELECT turn_id, attempt, status = 'pending', delivered_attempt IS NULL AND status = 'settled'
    FROM computer_job_waits;
