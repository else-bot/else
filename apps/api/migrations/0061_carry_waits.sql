-- A new owner message carries the pending waits of the turns it ends to its
-- own turn, as it carries delegated tasks, so the job's result still wakes
-- the bot. A carried wait gets attempt 0 on its new turn: it is no attempt's
-- own park, and several carried waits share that attempt. One wait per
-- attempt therefore holds only for attempts that parked themselves.
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
        CHECK (status IN ('pending', 'finished', 'failed', 'expired', 'cancelled')),
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
        CHECK (status IN ('pending', 'settled', 'cancelled')),
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
