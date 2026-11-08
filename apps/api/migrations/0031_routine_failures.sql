-- Consecutive failed occurrences of a routine since its last success, and
-- why else.bot paused it, if it did.
ALTER TABLE routines ADD COLUMN failed_runs INTEGER NOT NULL DEFAULT 0 CHECK (failed_runs >= 0);
ALTER TABLE routines ADD COLUMN paused_reason TEXT
    CHECK (paused_reason IS NULL OR paused_reason IN ('needs_action', 'repeated'));

-- An automatic retry names the run it repeats. A run is reviewed once its
-- turn has ended and its outcome was handled.
ALTER TABLE routine_runs ADD COLUMN retry_of BLOB REFERENCES routine_runs (id);
ALTER TABLE routine_runs ADD COLUMN reviewed INTEGER NOT NULL DEFAULT false;

-- Runs that ended before this release are history: never retried or reported.
UPDATE routine_runs SET reviewed = true;

-- A routine whose last run already failed shows that failure until it is
-- dismissed, re-enabled, or a run succeeds.
UPDATE routines SET failed_runs = 1
WHERE EXISTS (
    SELECT 1 FROM routine_runs rr JOIN turns t ON t.id = rr.turn_id
    WHERE rr.id = (SELECT x.id FROM routine_runs x WHERE x.routine_id = routines.id
                   ORDER BY x.due_at DESC LIMIT 1)
      AND t.status IN ('failed', 'uncertain'));

CREATE INDEX routine_runs_unreviewed ON routine_runs (turn_id)
    WHERE NOT reviewed AND turn_id IS NOT NULL;
