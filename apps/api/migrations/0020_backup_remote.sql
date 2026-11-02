-- Remote replication of backup sets (POC-023), tracked per set and apart
-- from the local outcome: `completed` only after every object was verified
-- by readback and the completion record was published and read back.
CREATE TABLE backup_remote (
    set_id text PRIMARY KEY CHECK (set_id <> '' AND octet_length(set_id) <= 64),
    state text NOT NULL CHECK (state IN ('completed', 'failed')),
    error text CHECK (octet_length(error) <= 2000),
    verified_at timestamptz,
    updated_at timestamptz NOT NULL DEFAULT now()
);
