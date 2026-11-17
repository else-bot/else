-- An owner message that joins a waiting reply continues it at once, with
-- every hold kept: the bot reads the message beside the work still
-- pending, and parks again if it still needs the owner or a result. The
-- message is new input for the reply, like a settled outcome, so a pending
-- owner card does not hold it back.
DROP VIEW turns_continuable;
CREATE VIEW turns_continuable AS
    SELECT t.id FROM turns t
    WHERE t.status = 'waiting'
      AND (EXISTS (SELECT 1 FROM turn_steers s JOIN turns m ON m.id = s.turn_id
                   WHERE s.into_turn_id = t.id AND s.status = 'pending' AND m.status = 'queued')
           OR (NOT EXISTS (SELECT 1 FROM turn_holds h
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
                                                   AND p.parent_attempt = d.parent_attempt)))));
