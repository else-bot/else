-- A webhook is a bot's saved task that an event wakes it for. An event
-- sets requested_at (later events coalesce into it); the scheduler runs
-- the wake once the pace allows, recording not_before while it waits.
-- Only the trigger token's digest is stored.
CREATE TABLE webhooks (
    id BLOB NOT NULL PRIMARY KEY,
    bot_id BLOB NOT NULL REFERENCES bots (id),
    conversation_id BLOB NOT NULL REFERENCES conversations (id),
    name text NOT NULL CHECK (length(name) BETWEEN 1 AND 80 AND name = trim(name)),
    prompt text NOT NULL CHECK (trim(prompt) <> '' AND length(CAST(prompt AS BLOB)) <= 8000),
    token_digest BLOB NOT NULL CHECK (length(token_digest) = 32),
    min_interval_minutes INTEGER NOT NULL CHECK (min_interval_minutes BETWEEN 1 AND 1440),
    daily_limit INTEGER NOT NULL CHECK (daily_limit BETWEEN 1 AND 1440),
    enabled INTEGER NOT NULL DEFAULT true,
    requested_at TEXT,
    not_before TEXT,
    created_turn_id BLOB NOT NULL REFERENCES turns (id),
    created_name text NOT NULL,
    request_hash BLOB NOT NULL CHECK (length(request_hash) = 32),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    deleted_at TEXT,
    UNIQUE (created_turn_id, created_name),
    CHECK (requested_at IS NULL OR (enabled AND deleted_at IS NULL)),
    CHECK (not_before IS NULL OR requested_at IS NOT NULL)
) STRICT;

CREATE UNIQUE INDEX webhooks_name ON webhooks (bot_id, name) WHERE deleted_at IS NULL;
CREATE INDEX webhooks_requested ON webhooks (requested_at) WHERE requested_at IS NOT NULL;

-- Each wake the scheduler queued. They count toward the pace. A wake that
-- cannot queue yet stays requested and is retried.
CREATE TABLE webhook_runs (
    id BLOB NOT NULL PRIMARY KEY,
    webhook_id BLOB NOT NULL REFERENCES webhooks (id),
    name text NOT NULL,
    requested_at TEXT NOT NULL,
    woke_at TEXT NOT NULL,
    turn_id BLOB NOT NULL UNIQUE REFERENCES turns (id)
) STRICT;

CREATE INDEX webhook_runs_webhook ON webhook_runs (webhook_id, woke_at);

-- A background job may fire one of its bot's webhooks when it exits 0.
ALTER TABLE guest_jobs ADD COLUMN webhook_id BLOB REFERENCES webhooks (id);

CREATE TRIGGER guest_jobs_fire_webhook AFTER UPDATE OF status ON guest_jobs
WHEN OLD.status <> 'completed' AND NEW.status = 'completed' AND NEW.webhook_id IS NOT NULL BEGIN
    UPDATE webhooks
    SET requested_at = coalesce(requested_at, strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))
    WHERE id = NEW.webhook_id AND bot_id = NEW.bot_id AND enabled AND deleted_at IS NULL;
END;
