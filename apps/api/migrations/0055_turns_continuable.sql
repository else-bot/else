-- One definition of when a waiting turn continues, shared by the triggers
-- that requeue it and by `turns::continue_waiting`: nothing but its
-- delegated tasks holds it, and either none of those is pending or a batch
-- of them has settled (the `delegations::READY` condition).
--
-- turns_delegation_child previously required the settled batch even when
-- no task was pending. The child that just ended leaves its delegation
-- ended and undelivered, so the rules differ only for a root whose tasks
-- had all ended and been delivered already: it now continues instead of
-- staying waiting with nothing to wait for, as continue_waiting does.
CREATE VIEW turns_continuable AS
    SELECT t.id FROM turns t
    WHERE t.status = 'waiting'
      AND (SELECT coalesce(sum(h.pending), 0) FROM turn_holds h WHERE h.turn_id = t.id)
          = (SELECT count(*) FROM delegations p
             WHERE p.root_turn_id = t.id AND p.status IN ('queued', 'running'))
      AND (NOT EXISTS (SELECT 1 FROM delegations p
                       WHERE p.root_turn_id = t.id AND p.status IN ('queued', 'running'))
           OR EXISTS (SELECT 1 FROM delegations d
                      WHERE d.root_turn_id = t.id AND d.delivered_attempt IS NULL
                        AND d.status IN ('completed', 'failed', 'cancelled')
                        AND NOT EXISTS (SELECT 1 FROM delegations p
                                        WHERE p.root_turn_id = d.root_turn_id
                                          AND p.status IN ('queued', 'running')
                                          AND d.parent_attempt > 0
                                          AND p.parent_attempt = d.parent_attempt)));

DROP TRIGGER guest_jobs_settle_waits;
CREATE TRIGGER guest_jobs_settle_waits AFTER UPDATE OF status ON guest_jobs
WHEN OLD.status <> NEW.status AND NEW.status IN ('completed', 'failed', 'cancelled', 'lost') BEGIN
    UPDATE computer_job_waits SET status = 'settled',
        settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE job_id = NEW.id AND status = 'pending';
    UPDATE turns SET status = 'queued', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE id IN (SELECT turn_id FROM computer_job_waits
                 WHERE job_id = NEW.id AND status = 'settled' AND delivered_attempt IS NULL)
      AND EXISTS (SELECT 1 FROM turns_continuable c WHERE c.id = turns.id);
END;

DROP TRIGGER turns_delegation_child;
CREATE TRIGGER turns_delegation_child AFTER UPDATE OF status ON turns
WHEN OLD.status <> NEW.status AND NEW.status IN ('completed', 'interrupted', 'failed', 'uncertain') BEGIN
    UPDATE delegations SET
        status = CASE NEW.status WHEN 'completed' THEN 'completed' WHEN 'interrupted' THEN 'cancelled' ELSE 'failed' END,
        result = (SELECT text FROM items WHERE turn_id = NEW.id AND kind = 'assistant'
                  ORDER BY seq DESC LIMIT 1),
        error = CASE WHEN NEW.status = 'completed' THEN NULL ELSE coalesce(NEW.error_code || ': ' || NEW.error_message, NEW.status) END,
        finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE ((child_turn_id = NEW.id AND takeover_turn_id IS NULL) OR takeover_turn_id = NEW.id)
      AND status NOT IN ('completed', 'failed', 'cancelled');
    UPDATE turns SET status = 'queued', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE id IN (SELECT root_turn_id FROM delegations
                 WHERE (child_turn_id = NEW.id AND takeover_turn_id IS NULL) OR takeover_turn_id = NEW.id)
      AND EXISTS (SELECT 1 FROM turns_continuable c WHERE c.id = turns.id);
END;
