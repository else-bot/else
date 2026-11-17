-- Every pending wait holds its turn like a delegated task, whichever
-- attempt parked it. Before, only waits carried from an earlier owner
-- message (attempt 0) did; a wait the turn parked itself blocked it, so a
-- carried wait that settled stayed undelivered until the turn's own wait
-- ended too.
--
-- Each hold now names its kind, so `turns_continuable` and the attempt end
-- in `turn_sink.rs` read one definition: an `owner` hold (a question, draft,
-- secret, social setup, skill install, or handoff) blocks the turn while it
-- is pending; a `task` or `wait` holds it without blocking. A waiting turn
-- continues once no owner hold is pending and either nothing is pending, an
-- outcome that is not a task's awaits delivery, or a batch of tasks is
-- ready (the `delegations::READY` rule).
DROP VIEW turns_continuable;
DROP VIEW carried_waits;
DROP VIEW turn_holds;

CREATE VIEW turn_holds AS
    SELECT root_turn_id AS turn_id, parent_attempt AS attempt, 'task' AS kind,
           status IN ('queued', 'running') AS pending,
           delivered_attempt IS NULL AND status IN ('completed', 'failed', 'cancelled') AS unseen
    FROM delegations
    UNION ALL
    SELECT turn_id, attempt, 'owner', status = 'pending', delivered_attempt IS NULL AND status <> 'pending'
    FROM secret_requests
    UNION ALL
    SELECT turn_id, attempt, 'owner', status = 'pending', delivered_attempt IS NULL AND status <> 'pending'
    FROM owner_questions WHERE json_type(action, '$.permission') IS NULL
    UNION ALL
    SELECT turn_id, attempt, 'owner', status = 'pending',
           delivered_attempt IS NULL AND status IN ('approved', 'discarded')
    FROM outgoing_drafts WHERE approval_source IS NOT 'message' AND approval_source IS NOT 'standing'
    UNION ALL
    SELECT turn_id, attempt, 'owner', status = 'pending', delivered_attempt IS NULL AND status <> 'pending'
    FROM social_setups
    UNION ALL
    SELECT turn_id, attempt, 'wait', status = 'pending',
           delivered_attempt IS NULL AND status IN ('finished', 'failed', 'expired', 'stopped')
    FROM mcp_waits
    UNION ALL
    SELECT turn_id, attempt, 'owner', status = 'pending',
           delivered_attempt IS NULL AND status IN ('installed', 'declined')
    FROM skill_installs
    UNION ALL
    SELECT turn_id, attempt, 'owner', status IN ('pending', 'taken'),
           delivered_attempt IS NULL AND status IN ('handed_back', 'dismissed', 'skipped')
    FROM computer_handoffs WHERE attempt > 0
    UNION ALL
    SELECT turn_id, attempt, 'wait', status = 'pending',
           delivered_attempt IS NULL AND status IN ('settled', 'stopped')
    FROM computer_job_waits;

CREATE VIEW turns_continuable AS
    SELECT t.id FROM turns t
    WHERE t.status = 'waiting'
      AND NOT EXISTS (SELECT 1 FROM turn_holds h
                      WHERE h.turn_id = t.id AND h.pending AND h.kind = 'owner')
      AND (NOT EXISTS (SELECT 1 FROM turn_holds h WHERE h.turn_id = t.id AND h.pending)
           OR EXISTS (SELECT 1 FROM turn_holds h
                      WHERE h.turn_id = t.id AND h.unseen AND h.kind <> 'task')
           OR EXISTS (SELECT 1 FROM delegations d
                      WHERE d.root_turn_id = t.id AND d.delivered_attempt IS NULL
                        AND d.status IN ('completed', 'failed', 'cancelled')
                        AND NOT EXISTS (SELECT 1 FROM delegations p
                                        WHERE p.root_turn_id = d.root_turn_id
                                          AND p.status IN ('queued', 'running')
                                          AND d.parent_attempt > 0
                                          AND p.parent_attempt = d.parent_attempt)));

-- A turn the old rule stranded, with a wait's outcome undelivered while
-- another of its waits runs, continues now instead of at its next outcome.
UPDATE turns SET status = 'queued', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
WHERE id IN (SELECT c.id FROM turns_continuable c)
  AND EXISTS (SELECT 1 FROM turn_holds h WHERE h.turn_id = turns.id AND h.unseen AND h.kind = 'wait');
