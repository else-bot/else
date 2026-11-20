-- A bot's watcher: else.bot runs a short check in the bot's computer every
-- every_minutes until the check reports the condition met, it keeps
-- failing, the owner or bot stops it, or watch_until passes. No turn waits
-- on it. 'waking' has ended and still has to queue the bot's wake turn in
-- conversation_id; 'ended' has queued it (wake_turn_id) or needs none.
CREATE TABLE watchers (
    id BLOB NOT NULL PRIMARY KEY,
    bot_id BLOB NOT NULL REFERENCES bots (id),
    conversation_id BLOB NOT NULL REFERENCES conversations (id),
    created_turn_id BLOB NOT NULL REFERENCES turns (id),
    purpose TEXT NOT NULL CHECK (length(purpose) BETWEEN 1 AND 300 AND purpose = trim(purpose)),
    check_command TEXT NOT NULL
        CHECK (trim(check_command) <> '' AND length(CAST(check_command AS BLOB)) <= 8000),
    prompt TEXT NOT NULL CHECK (trim(prompt) <> '' AND length(CAST(prompt AS BLOB)) <= 8000),
    every_minutes INTEGER NOT NULL CHECK (every_minutes BETWEEN 5 AND 1440),
    watch_until TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'waiting'
        CHECK (status IN ('waiting', 'checking', 'waking', 'ended')),
    outcome TEXT CHECK (outcome IN ('met', 'failed', 'expired', 'stopped')),
    next_check_at TEXT,
    check_job_id BLOB REFERENCES guest_jobs (id),
    checks INTEGER NOT NULL DEFAULT 0,
    failures INTEGER NOT NULL DEFAULT 0,
    last_status TEXT,
    last_output TEXT,
    last_checked_at TEXT,
    wake_turn_id BLOB UNIQUE REFERENCES turns (id),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    ended_at TEXT,
    CHECK ((status IN ('waking', 'ended')) = (outcome IS NOT NULL)),
    CHECK ((status IN ('waking', 'ended')) = (ended_at IS NOT NULL)),
    CHECK ((status = 'waiting') = (next_check_at IS NOT NULL)),
    CHECK ((status = 'checking') = (check_job_id IS NOT NULL)),
    CHECK (wake_turn_id IS NULL OR status = 'ended')
) STRICT;

CREATE INDEX watchers_active ON watchers (bot_id) WHERE status IN ('waiting', 'checking');
CREATE INDEX watchers_due ON watchers (next_check_at) WHERE status = 'waiting';
CREATE INDEX watchers_checking ON watchers (check_job_id) WHERE status = 'checking';
CREATE INDEX watchers_waking ON watchers (ended_at) WHERE status = 'waking';
CREATE INDEX watchers_created_turn ON watchers (created_turn_id);
