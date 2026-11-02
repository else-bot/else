-- On-demand bot computers (POC-017). See docs/sandboxes.md and
-- docs/research/execution-lifecycle.md.

-- One persistent micro-VM per bot, created on first permitted use. The
-- runtime name is the bot's stable identity in Microsandbox; its disk
-- survives stop, restart, and soft deletion.
CREATE TABLE bot_computers (
    bot_id uuid PRIMARY KEY REFERENCES bots (id),
    runtime_name text NOT NULL UNIQUE CHECK (runtime_name <> '' AND octet_length(runtime_name) <= 128),
    image text NOT NULL,
    cpus integer NOT NULL CHECK (cpus BETWEEN 1 AND 64),
    memory_mib integer NOT NULL CHECK (memory_mib BETWEEN 256 AND 262144),
    -- Owner intent: `auto` starts on demand; `stopped` never wakes
    -- implicitly until the owner resumes it.
    desired_state text NOT NULL DEFAULT 'auto' CHECK (desired_state IN ('auto', 'stopped')),
    -- What the API last established about the runtime. `missing` means a
    -- recorded computer was not found; it is never silently recreated.
    observed_state text NOT NULL DEFAULT 'absent' CHECK (observed_state IN (
        'absent', 'creating', 'starting', 'running', 'pausing', 'paused', 'resuming',
        'stopping', 'stopped', 'missing', 'error')),
    -- Bumped by every start, pause, resume, and stop decision; a slow
    -- transition whose generation moved on cannot record its result.
    generation bigint NOT NULL DEFAULT 0,
    last_used_at timestamptz,
    last_error text CHECK (octet_length(last_error) <= 2000),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Holders of a computer (provider turns, guest jobs, desktop sessions). A
-- computer is idle only when no lease is active; a lease is renewed while
-- its holder works and expires otherwise.
CREATE TABLE computer_leases (
    id uuid PRIMARY KEY,
    bot_id uuid NOT NULL REFERENCES bot_computers (bot_id),
    holder text NOT NULL CHECK (holder <> '' AND octet_length(holder) <= 200),
    generation bigint NOT NULL,
    acquired_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    released_at timestamptz,
    -- Why it ended: released, expired (after reconciliation), revoked.
    end_reason text
);
CREATE UNIQUE INDEX computer_leases_one_active_holder
    ON computer_leases (bot_id, holder) WHERE released_at IS NULL;
CREATE INDEX computer_leases_active ON computer_leases (bot_id) WHERE released_at IS NULL;
