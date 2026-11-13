-- A run whose check job runs has neither a turn nor a skip reason.
ALTER TABLE routines ADD COLUMN check_command TEXT CHECK (
    check_command IS NULL
    OR (trim(check_command) <> '' AND length(CAST(check_command AS BLOB)) <= 8000));

CREATE TABLE routine_runs_new (
    id BLOB NOT NULL PRIMARY KEY,
    routine_id BLOB NOT NULL REFERENCES routines (id),
    due_at TEXT NOT NULL,
    name text NOT NULL,
    turn_id BLOB UNIQUE REFERENCES turns (id),
    skip_reason text CHECK (skip_reason IN (
        'overlap', 'queue_full', 'provider_unavailable',
        'nothing_new', 'check_cancelled', 'check_unavailable')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    prompt_revision INTEGER,
    retry_of BLOB REFERENCES routine_runs_new (id),
    reviewed INTEGER NOT NULL DEFAULT false,
    check_job_id BLOB REFERENCES guest_jobs (id),
    UNIQUE (routine_id, due_at),
    CHECK (turn_id IS NULL OR skip_reason IS NULL),
    CHECK (turn_id IS NOT NULL OR skip_reason IS NOT NULL OR check_job_id IS NOT NULL),
    CHECK (skip_reason NOT IN ('nothing_new', 'check_cancelled') OR check_job_id IS NOT NULL)
) STRICT;

INSERT INTO routine_runs_new (id, routine_id, due_at, name, turn_id, skip_reason, created_at,
                              prompt_revision, retry_of, reviewed)
SELECT id, routine_id, due_at, name, turn_id, skip_reason, created_at,
       prompt_revision, retry_of, reviewed
FROM routine_runs;

DROP TABLE routine_runs;
ALTER TABLE routine_runs_new RENAME TO routine_runs;

CREATE INDEX routine_runs_routine ON routine_runs (routine_id, due_at);
CREATE INDEX routine_runs_unreviewed ON routine_runs (turn_id)
    WHERE NOT reviewed AND turn_id IS NOT NULL;
CREATE INDEX routine_runs_checking ON routine_runs (check_job_id)
    WHERE turn_id IS NULL AND skip_reason IS NULL;
