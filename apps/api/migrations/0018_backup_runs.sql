-- Backup runs (POC-021). Local and remote outcomes are separate: a local
-- success never clears a remote failure. Coverage states what a set holds.
CREATE TABLE backup_runs (
    id uuid PRIMARY KEY,
    set_id text NOT NULL CHECK (set_id <> '' AND octet_length(set_id) <= 64),
    started_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz,
    local_status text NOT NULL DEFAULT 'running'
        CHECK (local_status IN ('running', 'completed', 'failed')),
    remote_status text CHECK (remote_status IN ('pending', 'running', 'completed', 'failed')),
    error text CHECK (octet_length(error) <= 2000),
    ciphertext_bytes bigint,
    coverage jsonb
);
CREATE INDEX backup_runs_started ON backup_runs (started_at);
