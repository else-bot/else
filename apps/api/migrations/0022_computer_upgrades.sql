-- POC-025: explicit image upgrades of existing computers. An upgrade copies
-- the old computer's user data into a new computer created from the new
-- image and then switches the bot to it; the old computer is kept, stopped,
-- so the upgrade can be rolled back. A `started` row whose process died
-- names a replacement nobody uses; the next attempt removes it.
CREATE TABLE computer_upgrades (
    id uuid PRIMARY KEY,
    bot_id uuid NOT NULL REFERENCES bot_computers (bot_id),
    from_runtime_name text NOT NULL,
    from_image text NOT NULL,
    to_runtime_name text NOT NULL UNIQUE,
    to_image text NOT NULL,
    state text NOT NULL CHECK (state IN ('started', 'completed', 'failed', 'rolled_back')),
    data_bytes bigint,
    data_sha256 text,
    error text CHECK (octet_length(error) <= 2000),
    created_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz
);
CREATE INDEX computer_upgrades_bot ON computer_upgrades (bot_id, created_at DESC);
