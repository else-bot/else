-- Installation settings, provider profiles, bots, canonical conversations,
-- and durable request identities. Turn and event tables arrive with the tasks
-- that use them.

CREATE TABLE bots (
    id uuid PRIMARY KEY,
    name text NOT NULL
        CHECK (char_length(name) BETWEEN 1 AND 80 AND name = btrim(name)),
    role text NOT NULL
        CHECK (btrim(role) <> '' AND octet_length(role) <= 8192),
    -- active -> deleting (tombstoned, shutdown pending) -> deleted.
    status text NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'deleting', 'deleted')),
    deleted_at timestamptz,
    selected_provider_id text,
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT bots_deleted_at_matches_status
        CHECK ((status = 'active') = (deleted_at IS NULL))
);

-- One canonical conversation per bot, created in the same transaction.
CREATE TABLE conversations (
    id uuid PRIMARY KEY,
    bot_id uuid NOT NULL UNIQUE REFERENCES bots (id),
    created_at timestamptz NOT NULL DEFAULT now()
);

-- Exactly one row once explicit setup has completed. Ordinary startup reads
-- it and never seeds bots. A null default is a valid, preserved state.
CREATE TABLE installation (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    setup_completed_at timestamptz NOT NULL DEFAULT now(),
    default_bot_id uuid REFERENCES bots (id)
);

-- One saved default execution profile per provider. Owner updates bump the
-- revision; accepted turns snapshot the revision they used.
CREATE TABLE provider_profiles (
    provider_id text PRIMARY KEY CHECK (provider_id <> ''),
    model text NOT NULL CHECK (model <> ''),
    reasoning_effort text NOT NULL CHECK (reasoning_effort <> ''),
    service_tier text NOT NULL CHECK (service_tier <> ''),
    revision bigint NOT NULL DEFAULT 1 CHECK (revision >= 1),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Durable receipts for owner mutations retried under a stable request ID.
-- Turns keep their own request identity in the turn table.
CREATE TABLE request_receipts (
    request_id uuid PRIMARY KEY,
    operation text NOT NULL CHECK (operation IN ('create_bot', 'cancel')),
    bot_id uuid NOT NULL REFERENCES bots (id),
    payload_hash bytea NOT NULL CHECK (octet_length(payload_hash) = 32),
    created_at timestamptz NOT NULL DEFAULT now()
);
