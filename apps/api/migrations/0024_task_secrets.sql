-- POC-026: owner-provided credentials for a bot's guest scripts. Each
-- secret is a set of environment variables, encrypted with a key derived
-- from SECRET_KEY and a per-secret salt; only the variable names are
-- stored in the clear. Values reach the guest only as environment
-- variables of the command or job that names the secret.
CREATE TABLE task_secrets (
    bot_id uuid NOT NULL REFERENCES bots (id),
    name text NOT NULL CHECK (name ~ '^[a-z][a-z0-9_-]{0,39}$'),
    env_names text[] NOT NULL,
    salt bytea NOT NULL CHECK (octet_length(salt) = 32),
    ciphertext bytea NOT NULL CHECK (octet_length(ciphertext) <= 262144),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (bot_id, name)
);

-- What a job's output must not show: for each secret value its byte length
-- and SHA-256, never the value. Output is redacted before it is stored.
ALTER TABLE guest_jobs ADD COLUMN redact jsonb NOT NULL DEFAULT '[]';
ALTER TABLE guest_jobs ADD COLUMN secrets text[] NOT NULL DEFAULT '{}';
