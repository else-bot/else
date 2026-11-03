-- Each bot's own provider and execution profile. A bot without one uses the
-- installation's default provider and that provider's saved profile.
CREATE TABLE bot_profiles (
    bot_id uuid PRIMARY KEY REFERENCES bots (id),
    provider_id text NOT NULL CHECK (provider_id <> '' AND octet_length(provider_id) <= 64),
    model text NOT NULL CHECK (model <> '' AND octet_length(model) <= 200),
    reasoning_effort text NOT NULL CHECK (octet_length(reasoning_effort) <= 64),
    service_tier text NOT NULL CHECK (octet_length(service_tier) <= 64),
    revision bigint NOT NULL DEFAULT 1,
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- The provider new bots start with (the owner's choice); NULL means the
-- first available one.
ALTER TABLE installation ADD COLUMN default_provider_id text
    CHECK (default_provider_id <> '' AND octet_length(default_provider_id) <= 64);
