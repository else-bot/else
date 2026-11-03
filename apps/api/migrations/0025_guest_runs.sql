-- POC-026 review: no stored fingerprints of secret values (a database
-- reader could brute-force short ones); the API decrypts a job's secrets
-- by name when it redacts its output.
ALTER TABLE guest_jobs DROP COLUMN redact;

-- Foreground commands while they run, so Stop work and deletion settle
-- only once they ended, and an API that restarted ends the ones it left.
CREATE TABLE guest_runs (
    id uuid PRIMARY KEY,
    bot_id uuid NOT NULL REFERENCES bots (id),
    turn_id uuid REFERENCES turns (id),
    runtime_name text NOT NULL,
    started_at timestamptz NOT NULL DEFAULT now(),
    ended_at timestamptz,
    end_reason text CHECK (octet_length(end_reason) <= 200)
);
CREATE INDEX guest_runs_active ON guest_runs (bot_id) WHERE ended_at IS NULL;
