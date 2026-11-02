-- Guest tools and the desktop (POC-018).

-- The host loopback port where a desktop computer's noVNC is published,
-- fixed when the computer is created.
ALTER TABLE bot_computers
    ADD COLUMN desktop_port integer CHECK (desktop_port BETWEEN 1024 AND 65535);
CREATE UNIQUE INDEX bot_computers_desktop_port ON bot_computers (desktop_port)
    WHERE desktop_port IS NOT NULL;

-- Background commands in a bot's computer. A running job holds its own
-- lease (`job:<id>`), so it outlives the provider turn that started it.
CREATE TABLE guest_jobs (
    id uuid PRIMARY KEY,
    bot_id uuid NOT NULL REFERENCES bot_computers (bot_id),
    turn_id uuid REFERENCES turns (id),
    command text NOT NULL CHECK (command <> '' AND octet_length(command) <= 8000),
    status text NOT NULL DEFAULT 'starting'
        CHECK (status IN ('starting', 'running', 'completed', 'failed', 'cancelled', 'lost')),
    exit_code integer,
    lease_id uuid REFERENCES computer_leases (id),
    deadline timestamptz NOT NULL,
    output_tail text CHECK (octet_length(output_tail) <= 8000),
    error text CHECK (octet_length(error) <= 2000),
    started_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz
);
CREATE INDEX guest_jobs_running ON guest_jobs (bot_id) WHERE status IN ('starting', 'running');

-- Files copied out of a guest into the host artifact store.
CREATE TABLE artifacts (
    id uuid PRIMARY KEY,
    bot_id uuid NOT NULL REFERENCES bots (id),
    turn_id uuid REFERENCES turns (id),
    guest_path text NOT NULL CHECK (octet_length(guest_path) <= 1024),
    size_bytes bigint NOT NULL CHECK (size_bytes >= 0),
    sha256 bytea NOT NULL CHECK (octet_length(sha256) = 32),
    created_at timestamptz NOT NULL DEFAULT now()
);
