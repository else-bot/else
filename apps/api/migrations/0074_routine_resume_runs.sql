-- A run the owner asked for from a paused routine's Retry switches the
-- routine back on when it succeeds. An automatic retry never does.
ALTER TABLE routine_runs ADD COLUMN resumes INTEGER NOT NULL DEFAULT false
    CHECK (resumes IN (0, 1) AND NOT (resumes AND retry_of IS NOT NULL));
