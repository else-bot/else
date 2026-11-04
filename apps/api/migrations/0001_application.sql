-- SQLite baseline: the complete application schema, including all PostgreSQL-era features.
-- UUIDs and digests are BLOBs; JSON and UTC RFC3339 timestamps are TEXT.

CREATE TABLE bots (
    id BLOB NOT NULL PRIMARY KEY,
    name text NOT NULL
        CHECK (length(name) BETWEEN 1 AND 80 AND name = trim(name)),
    role text NOT NULL
        CHECK (trim(role) <> '' AND length(CAST(role AS BLOB)) <= 8192),
    status text NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'deleting', 'deleted')),
    deleted_at TEXT,
    selected_provider_id text,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    cancel_pending INTEGER NOT NULL DEFAULT false,
    last_event_seq INTEGER NOT NULL DEFAULT 0,
    category_id BLOB REFERENCES bot_categories (id),
    CONSTRAINT bots_deleted_at_matches_status
        CHECK ((status = 'active') = (deleted_at IS NULL))
) STRICT;

CREATE TABLE conversations (
    id BLOB NOT NULL PRIMARY KEY,
    bot_id BLOB NOT NULL REFERENCES bots (id),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    last_item_seq INTEGER NOT NULL DEFAULT 0,
    last_turn_no INTEGER NOT NULL DEFAULT 0,
    kind text NOT NULL DEFAULT 'owner' CHECK (kind IN ('owner', 'task'))
) STRICT;

CREATE TABLE installation (
    singleton INTEGER NOT NULL PRIMARY KEY DEFAULT true CHECK (singleton = 1),
    setup_completed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    default_bot_id BLOB REFERENCES bots (id),
    memory_recipe TEXT NOT NULL
    DEFAULT '{"version":1,"node_bytes":512,"view_high":128000,"view_low":64000}',
    default_provider_id text
    CHECK (default_provider_id <> '' AND length(CAST(default_provider_id AS BLOB)) <= 64),
    onboarded_at TEXT,
    CHECK (memory_recipe IS NULL OR json_valid(memory_recipe))
) STRICT;

CREATE TABLE provider_profiles (
    provider_id text NOT NULL PRIMARY KEY CHECK (provider_id <> ''),
    model text NOT NULL CHECK (model <> ''),
    reasoning_effort text NOT NULL CHECK (reasoning_effort <> ''),
    service_tier text NOT NULL CHECK (service_tier <> ''),
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision >= 1),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))
) STRICT;

CREATE TABLE request_receipts (
    request_id BLOB NOT NULL PRIMARY KEY,
    operation text NOT NULL CHECK (operation IN ('create_bot', 'cancel')),
    bot_id BLOB NOT NULL REFERENCES bots (id),
    payload_hash BLOB NOT NULL CHECK (length(CAST(payload_hash AS BLOB)) = 32),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    state text CHECK (state IN ('pending', 'done')),
    boundary_turn_no INTEGER,
    completed_at TEXT,
    CONSTRAINT request_receipts_cancel_fields CHECK (
        (operation = 'cancel') = (state IS NOT NULL AND boundary_turn_no IS NOT NULL)
    )
) STRICT;

CREATE TABLE turns (
    id BLOB NOT NULL PRIMARY KEY,
    bot_id BLOB NOT NULL REFERENCES bots (id),
    conversation_id BLOB NOT NULL REFERENCES conversations (id),
    request_id BLOB NOT NULL,
    request_hash BLOB NOT NULL CHECK (length(CAST(request_hash AS BLOB)) = 32),
    turn_no INTEGER NOT NULL CHECK (turn_no >= 1),
    owner_item_id BLOB NOT NULL UNIQUE,
    provider_id text NOT NULL,
    profile_revision INTEGER,
    profile TEXT,
    status text NOT NULL DEFAULT 'queued',
    error_code text,
    error_message text,
    effect_uncertain INTEGER NOT NULL DEFAULT false,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    started_at TEXT,
    finished_at TEXT,
    provider_session_id BLOB REFERENCES provider_sessions (id),
    native_turn_id text,
    reported_profile TEXT,
    attempt INTEGER NOT NULL DEFAULT 0 CHECK (attempt >= 0),
    UNIQUE (conversation_id, request_id),
    UNIQUE (conversation_id, turn_no),
    CHECK ((profile IS NULL) = (profile_revision IS NULL)),
    CONSTRAINT turns_status_check CHECK (status IN (
    'queued', 'preparing', 'sending', 'running', 'cancelling', 'waiting',
    'completed', 'interrupted', 'failed', 'uncertain')),
    CHECK (profile IS NULL OR json_valid(profile)),
    CHECK (reported_profile IS NULL OR json_valid(reported_profile))
) STRICT;

CREATE TABLE items (
    id BLOB NOT NULL PRIMARY KEY,
    conversation_id BLOB NOT NULL REFERENCES conversations (id),
    seq INTEGER NOT NULL CHECK (seq >= 1),
    turn_id BLOB NOT NULL REFERENCES turns (id),
    kind text NOT NULL CHECK (kind IN ('user', 'assistant', 'status')),
    text text NOT NULL,
    provider_id text,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    UNIQUE (conversation_id, seq)
) STRICT;

CREATE TABLE bot_events (
    bot_id BLOB NOT NULL REFERENCES bots (id),
    seq INTEGER NOT NULL CHECK (seq >= 1),
    kind text NOT NULL
        CHECK (kind IN ('item.upsert', 'item.delta', 'turn.updated', 'bot.updated')),
    data TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    PRIMARY KEY (bot_id, seq),
    CHECK (data IS NULL OR json_valid(data))
) STRICT;

CREATE TABLE provider_sessions (
    id BLOB NOT NULL PRIMARY KEY,
    bot_id BLOB NOT NULL REFERENCES bots (id),
    conversation_id BLOB NOT NULL REFERENCES conversations (id),
    provider_id text NOT NULL,
    native_session_id text NOT NULL,
    fingerprint text NOT NULL,
    status text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'retired')),
    considered_through_turn INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    memory_revision INTEGER
) STRICT;

CREATE TABLE memory_store (
    bot_id BLOB NOT NULL PRIMARY KEY REFERENCES bots (id),
    next_seq INTEGER NOT NULL DEFAULT 0 CHECK (next_seq >= 0),
    generation INTEGER NOT NULL DEFAULT 1,
    published_seq INTEGER NOT NULL DEFAULT -1,
    view_revision INTEGER NOT NULL DEFAULT 0,
    view_nodes TEXT NOT NULL DEFAULT '[]',
    compacting INTEGER NOT NULL DEFAULT false,
    building_generation INTEGER,
    context_nodes TEXT NOT NULL DEFAULT '[]',
    context_compacting INTEGER NOT NULL DEFAULT false,
    CONSTRAINT memory_store_published CHECK (published_seq >= -1 AND published_seq < next_seq),
    CONSTRAINT memory_store_building CHECK (building_generation IS NULL OR building_generation > generation),
    CHECK (view_nodes IS NULL OR json_valid(view_nodes)),
    CHECK (context_nodes IS NULL OR json_valid(context_nodes))
) STRICT;

CREATE TABLE memory_entry (
    bot_id BLOB NOT NULL REFERENCES memory_store (bot_id),
    seq INTEGER NOT NULL CHECK (seq >= 0),
    request_key text NOT NULL CHECK (request_key <> '' AND length(CAST(request_key AS BLOB)) <= 200),
    request_fingerprint BLOB NOT NULL CHECK (length(CAST(request_fingerprint AS BLOB)) = 32),
    kind text NOT NULL,
    authority text NOT NULL CHECK (authority IN ('owner', 'assistant', 'tool', 'external')),
    body text NOT NULL CHECK (length(CAST(body AS BLOB)) <= 64000),
    body_sha256 BLOB NOT NULL CHECK (length(CAST(body_sha256 AS BLOB)) = 32),
    source TEXT NOT NULL,
    part_index INTEGER NOT NULL DEFAULT 0 CHECK (part_index >= 0),
    part_count INTEGER NOT NULL DEFAULT 1 CHECK (part_count >= 1 AND part_index < part_count),
    original_sha256 BLOB NOT NULL CHECK (length(CAST(original_sha256 AS BLOB)) = 32),
    supersedes_seq INTEGER,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    PRIMARY KEY (bot_id, seq),
    UNIQUE (bot_id, request_key, part_index),
    FOREIGN KEY (bot_id, supersedes_seq) REFERENCES memory_entry (bot_id, seq),
    CHECK (supersedes_seq IS NULL OR (kind = 'owner_correction' AND authority = 'owner' AND supersedes_seq < seq)),
    CHECK ((kind = 'owner_correction') = (supersedes_seq IS NOT NULL)),
    CHECK (kind NOT IN ('owner_message', 'owner_correction') OR authority = 'owner'),
    CHECK (authority <> 'owner' OR kind IN ('owner_message', 'owner_correction')),
    CONSTRAINT memory_entry_kind_check CHECK (kind IN (
    'owner_message', 'assistant_message', 'owner_correction', 'tool_activity', 'delegated_task')),
    CHECK (source IS NULL OR json_valid(source))
) STRICT;

CREATE TABLE memory_read_grants (
    reader_bot_id BLOB NOT NULL REFERENCES bots (id),
    target_bot_id BLOB NOT NULL REFERENCES bots (id),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    PRIMARY KEY (reader_bot_id, target_bot_id),
    CHECK (reader_bot_id <> target_bot_id)
) STRICT;

CREATE TABLE memory_generation (
    bot_id BLOB NOT NULL REFERENCES memory_store (bot_id),
    generation INTEGER NOT NULL CHECK (generation >= 1),
    recipe TEXT NOT NULL,
    watermark INTEGER,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    PRIMARY KEY (bot_id, generation),
    CHECK (recipe IS NULL OR json_valid(recipe))
) STRICT;

CREATE TABLE memory_node (
    bot_id BLOB NOT NULL,
    generation INTEGER NOT NULL,
    start_seq INTEGER NOT NULL CHECK (start_seq >= 0),
    span INTEGER NOT NULL CHECK (span > 0 AND span <= (1 << 60)),
    body text NOT NULL CHECK (length(CAST(body AS BLOB)) BETWEEN 1 AND 65536),
    method text NOT NULL CHECK (method IN ('verbatim', 'join', 'model')),
    input_hash BLOB NOT NULL CHECK (length(CAST(input_hash AS BLOB)) = 32),
    audit TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    PRIMARY KEY (bot_id, generation, start_seq, span),
    FOREIGN KEY (bot_id, generation) REFERENCES memory_generation (bot_id, generation),
    CHECK ((span & (span - 1)) = 0 AND start_seq % span = 0),
    CHECK (audit IS NULL OR json_valid(audit))
) STRICT;

CREATE TABLE memory_job (
    bot_id BLOB NOT NULL,
    generation INTEGER NOT NULL,
    start_seq INTEGER NOT NULL,
    span INTEGER NOT NULL,
    available_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    lease_token BLOB,
    lease_until TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    blocked_reason text,
    last_error text CHECK (length(CAST(last_error AS BLOB)) <= 1000),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    failures INTEGER NOT NULL DEFAULT 0 CHECK (failures >= 0),
    PRIMARY KEY (bot_id, generation, start_seq, span),
    FOREIGN KEY (bot_id, generation) REFERENCES memory_generation (bot_id, generation),
    CHECK (span > 0 AND (span & (span - 1)) = 0 AND start_seq >= 0 AND start_seq % span = 0),
    CHECK ((lease_token IS NULL) = (lease_until IS NULL))
) STRICT;

CREATE TABLE delegations (
    id BLOB NOT NULL PRIMARY KEY,
    root_turn_id BLOB NOT NULL REFERENCES turns (id),
    parent_attempt INTEGER NOT NULL,
    parent_bot_id BLOB NOT NULL REFERENCES bots (id),
    target_bot_id BLOB NOT NULL REFERENCES bots (id),
    task text NOT NULL CHECK (trim(task) <> '' AND length(CAST(task AS BLOB)) <= 8000),
    task_hash BLOB NOT NULL CHECK (length(CAST(task_hash AS BLOB)) = 32),
    conversation_id BLOB NOT NULL REFERENCES conversations (id),
    child_turn_id BLOB NOT NULL UNIQUE REFERENCES turns (id),
    status text NOT NULL DEFAULT 'queued'
        CHECK (status IN ('queued', 'running', 'completed', 'failed', 'cancelled')),
    result text,
    error text,
    delivered_attempt INTEGER,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    finished_at TEXT,
    UNIQUE (root_turn_id, task_hash),
    CHECK (parent_bot_id <> target_bot_id)
) STRICT;

CREATE TABLE bot_computers (
    bot_id BLOB NOT NULL PRIMARY KEY REFERENCES bots (id),
    runtime_name text NOT NULL UNIQUE CHECK (runtime_name <> '' AND length(CAST(runtime_name AS BLOB)) <= 128),
    image text NOT NULL,
    cpus INTEGER NOT NULL CHECK (cpus BETWEEN 1 AND 64),
    memory_mib INTEGER NOT NULL CHECK (memory_mib BETWEEN 256 AND 262144),
    desired_state text NOT NULL DEFAULT 'auto' CHECK (desired_state IN ('auto', 'stopped')),
    observed_state text NOT NULL DEFAULT 'absent' CHECK (observed_state IN (
        'absent', 'creating', 'starting', 'running', 'pausing', 'paused', 'resuming',
        'stopping', 'stopped', 'missing', 'error')),
    generation INTEGER NOT NULL DEFAULT 0,
    last_used_at TEXT,
    last_error text CHECK (length(CAST(last_error AS BLOB)) <= 2000),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    desktop_port INTEGER CHECK (desktop_port BETWEEN 1024 AND 65535),
    services_generation INTEGER,
    browser_checkpoint text CHECK (browser_checkpoint IN ('ok', 'failed')),
    network text CHECK (network <> '' AND length(CAST(network AS BLOB)) <= 100)
) STRICT;

CREATE TABLE computer_leases (
    id BLOB NOT NULL PRIMARY KEY,
    bot_id BLOB NOT NULL REFERENCES bot_computers (bot_id),
    holder text NOT NULL CHECK (holder <> '' AND length(CAST(holder AS BLOB)) <= 200),
    generation INTEGER NOT NULL,
    acquired_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    expires_at TEXT NOT NULL,
    released_at TEXT,
    end_reason text
) STRICT;

CREATE TABLE guest_jobs (
    id BLOB NOT NULL PRIMARY KEY,
    bot_id BLOB NOT NULL REFERENCES bot_computers (bot_id),
    turn_id BLOB REFERENCES turns (id),
    command text NOT NULL CHECK (command <> '' AND length(CAST(command AS BLOB)) <= 8000),
    status text NOT NULL DEFAULT 'starting',
    exit_code INTEGER,
    lease_id BLOB REFERENCES computer_leases (id),
    deadline TEXT NOT NULL,
    output_tail text CHECK (length(CAST(output_tail AS BLOB)) <= 8000),
    error text CHECK (length(CAST(error AS BLOB)) <= 2000),
    started_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    finished_at TEXT,
    secrets TEXT NOT NULL DEFAULT '[]',
    CONSTRAINT guest_jobs_status_check CHECK (status IN (
    'starting', 'running', 'cancelling', 'completed', 'failed', 'cancelled', 'lost')),
    CHECK (secrets IS NULL OR json_valid(secrets))
) STRICT;

CREATE TABLE artifacts (
    id BLOB NOT NULL PRIMARY KEY,
    bot_id BLOB NOT NULL REFERENCES bots (id),
    turn_id BLOB REFERENCES turns (id),
    guest_path text NOT NULL CHECK (length(CAST(guest_path AS BLOB)) <= 1024),
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    sha256 BLOB NOT NULL CHECK (length(CAST(sha256 AS BLOB)) = 32),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    meta TEXT,
    CHECK (meta IS NULL OR json_valid(meta))
) STRICT;

CREATE TABLE handoffs (
    id BLOB NOT NULL PRIMARY KEY,
    bot_id BLOB NOT NULL REFERENCES bots (id),
    conversation_id BLOB NOT NULL REFERENCES conversations (id),
    turn_id BLOB NOT NULL REFERENCES turns (id),
    attempt INTEGER NOT NULL,
    source_provider_id text NOT NULL,
    target_provider_id text NOT NULL,
    source_session_id BLOB REFERENCES provider_sessions (id),
    boundary_turn_no INTEGER NOT NULL,
    kind text NOT NULL CHECK (kind IN ('llm', 'fallback')),
    reason text CHECK (length(CAST(reason AS BLOB)) <= 2000),
    checkpoint text CHECK (length(CAST(checkpoint AS BLOB)) <= 8192),
    prompt_version text NOT NULL,
    selected_turns TEXT NOT NULL DEFAULT '[]',
    omitted_turns TEXT NOT NULL DEFAULT '[]',
    payload_sha256 BLOB CHECK (length(CAST(payload_sha256 AS BLOB)) = 32),
    target_session_id BLOB REFERENCES provider_sessions (id),
    state text NOT NULL DEFAULT 'prepared'
        CHECK (state IN ('prepared', 'sending', 'accepted', 'uncertain')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    UNIQUE (turn_id, attempt),
    CHECK ((kind = 'llm') = (checkpoint IS NOT NULL)),
    CHECK (selected_turns IS NULL OR json_valid(selected_turns)),
    CHECK (omitted_turns IS NULL OR json_valid(omitted_turns))
) STRICT;

CREATE TABLE backup_runs (
    id BLOB NOT NULL PRIMARY KEY,
    set_id text NOT NULL CHECK (set_id <> '' AND length(CAST(set_id AS BLOB)) <= 64),
    started_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    finished_at TEXT,
    local_status text NOT NULL DEFAULT 'running'
        CHECK (local_status IN ('running', 'completed', 'failed')),
    remote_status text CHECK (remote_status IN ('pending', 'running', 'completed', 'failed')),
    error text CHECK (length(CAST(error AS BLOB)) <= 2000),
    ciphertext_bytes INTEGER,
    coverage TEXT,
    CHECK (coverage IS NULL OR json_valid(coverage))
) STRICT;

CREATE TABLE backup_remote (
    set_id text CHECK (set_id <> '' AND length(CAST(set_id AS BLOB)) <= 64),
    state text NOT NULL,
    error text CHECK (length(CAST(error AS BLOB)) <= 2000),
    verified_at TEXT,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    destination text NOT NULL DEFAULT '',
    whole_assistant INTEGER NOT NULL DEFAULT false,
    PRIMARY KEY (destination, set_id),
    CONSTRAINT backup_remote_state_check
    CHECK (state IN ('completed', 'failed', 'retired'))
) STRICT;

CREATE TABLE computer_upgrades (
    id BLOB NOT NULL PRIMARY KEY,
    bot_id BLOB NOT NULL REFERENCES bot_computers (bot_id),
    from_runtime_name text NOT NULL,
    from_image text NOT NULL,
    to_runtime_name text NOT NULL UNIQUE,
    to_image text NOT NULL,
    state text NOT NULL CHECK (state IN ('started', 'completed', 'failed', 'rolled_back')),
    data_bytes INTEGER,
    data_sha256 text,
    error text CHECK (length(CAST(error AS BLOB)) <= 2000),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    finished_at TEXT,
    from_network text,
    to_network text
) STRICT;

CREATE TABLE task_secrets (
    bot_id BLOB NOT NULL REFERENCES bots (id),
    name text NOT NULL CHECK (length(name) BETWEEN 1 AND 40 AND substr(name, 1, 1) GLOB '[a-z]' AND name NOT GLOB '*[^a-z0-9_-]*'),
    env_names TEXT NOT NULL,
    salt BLOB NOT NULL CHECK (length(CAST(salt AS BLOB)) = 32),
    ciphertext BLOB NOT NULL CHECK (length(CAST(ciphertext AS BLOB)) <= 262144),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    PRIMARY KEY (bot_id, name),
    CHECK (env_names IS NULL OR json_valid(env_names))
) STRICT;

CREATE TABLE guest_runs (
    id BLOB NOT NULL PRIMARY KEY,
    bot_id BLOB NOT NULL REFERENCES bots (id),
    turn_id BLOB REFERENCES turns (id),
    runtime_name text NOT NULL,
    started_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    ended_at TEXT,
    end_reason text CHECK (length(CAST(end_reason AS BLOB)) <= 200)
) STRICT;

CREATE TABLE bot_categories (
    id BLOB NOT NULL PRIMARY KEY,
    name text NOT NULL CHECK (name = trim(name) AND length(name) BETWEEN 1 AND 80),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    name_key text NOT NULL,
    deleting INTEGER NOT NULL DEFAULT false
) STRICT;

CREATE TABLE category_requests (
    request_id BLOB NOT NULL PRIMARY KEY,
    category_id BLOB NOT NULL,
    payload_hash BLOB NOT NULL CHECK (length(CAST(payload_hash AS BLOB)) = 32),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))
) STRICT;

CREATE TABLE bot_profiles (
    bot_id BLOB NOT NULL PRIMARY KEY REFERENCES bots (id),
    provider_id text NOT NULL CHECK (provider_id <> '' AND length(CAST(provider_id AS BLOB)) <= 64),
    model text NOT NULL CHECK (model <> '' AND length(CAST(model AS BLOB)) <= 200),
    reasoning_effort text NOT NULL CHECK (length(CAST(reasoning_effort AS BLOB)) <= 64),
    service_tier text NOT NULL CHECK (length(CAST(service_tier AS BLOB)) <= 64),
    revision INTEGER NOT NULL DEFAULT 1,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))
) STRICT;

CREATE UNIQUE INDEX turns_one_active_per_bot ON turns (bot_id)
    WHERE status IN ('preparing', 'sending', 'running', 'cancelling');
CREATE INDEX turns_queued ON turns (bot_id, turn_no) WHERE status = 'queued';
CREATE INDEX items_turn ON items (turn_id);
CREATE INDEX request_receipts_pending_cancel ON request_receipts (bot_id)
    WHERE operation = 'cancel' AND state = 'pending';
CREATE UNIQUE INDEX provider_sessions_one_active
    ON provider_sessions (conversation_id, provider_id) WHERE status = 'active';
CREATE INDEX memory_entry_supersedes ON memory_entry (bot_id, supersedes_seq)
    WHERE supersedes_seq IS NOT NULL;
CREATE INDEX memory_job_ready ON memory_job (available_at, bot_id, start_seq, span);
CREATE UNIQUE INDEX conversations_one_owner ON conversations (bot_id) WHERE kind = 'owner';
CREATE INDEX delegations_root ON delegations (root_turn_id);
CREATE UNIQUE INDEX computer_leases_one_active_holder
    ON computer_leases (bot_id, holder) WHERE released_at IS NULL;
CREATE INDEX computer_leases_active ON computer_leases (bot_id) WHERE released_at IS NULL;
CREATE UNIQUE INDEX bot_computers_desktop_port ON bot_computers (desktop_port)
    WHERE desktop_port IS NOT NULL;
CREATE INDEX guest_jobs_running ON guest_jobs (bot_id) WHERE status IN ('starting', 'running');
CREATE INDEX handoffs_bot ON handoffs (bot_id, created_at);
CREATE INDEX backup_runs_started ON backup_runs (started_at);
CREATE INDEX computer_upgrades_bot ON computer_upgrades (bot_id, created_at DESC);
CREATE INDEX guest_runs_active ON guest_runs (bot_id) WHERE ended_at IS NULL;
CREATE INDEX bots_category ON bots (category_id) WHERE category_id IS NOT NULL;
CREATE UNIQUE INDEX bot_categories_name_key ON bot_categories (name_key);

CREATE VIRTUAL TABLE memory_search USING fts5(body, content='memory_entry', content_rowid='rowid', tokenize='unicode61 remove_diacritics 0');
CREATE TRIGGER memory_search_insert AFTER INSERT ON memory_entry BEGIN
    INSERT INTO memory_search(rowid, body) VALUES (new.rowid, new.body);
END;
CREATE TRIGGER memory_entry_no_update BEFORE UPDATE ON memory_entry BEGIN
    SELECT RAISE(ABORT, 'memory entries are immutable');
END;
CREATE TRIGGER memory_entry_no_delete BEFORE DELETE ON memory_entry BEGIN
    SELECT RAISE(ABORT, 'memory entries are immutable');
END;
CREATE TRIGGER memory_node_no_update BEFORE UPDATE ON memory_node BEGIN
    SELECT RAISE(ABORT, 'memory nodes are immutable');
END;
CREATE TRIGGER memory_node_no_delete BEFORE DELETE ON memory_node BEGIN
    SELECT RAISE(ABORT, 'memory nodes are immutable');
END;
CREATE TRIGGER memory_entry_correction BEFORE INSERT ON memory_entry
WHEN NEW.supersedes_seq IS NOT NULL AND NOT EXISTS (
    SELECT 1 FROM memory_entry WHERE bot_id = NEW.bot_id AND seq = NEW.supersedes_seq AND authority = 'owner'
) BEGIN
    SELECT RAISE(ABORT, 'a correction must supersede an owner entry of the same bot');
END;
CREATE TRIGGER turns_delegation_running AFTER UPDATE OF status ON turns
WHEN OLD.status <> NEW.status AND NEW.status IN ('preparing', 'sending', 'running') BEGIN
    UPDATE delegations SET status = 'running' WHERE child_turn_id = NEW.id AND status = 'queued';
END;
CREATE TRIGGER turns_delegation_child AFTER UPDATE OF status ON turns
WHEN OLD.status <> NEW.status AND NEW.status IN ('completed', 'interrupted', 'failed', 'uncertain') BEGIN
    UPDATE delegations SET
        status = CASE NEW.status WHEN 'completed' THEN 'completed' WHEN 'interrupted' THEN 'cancelled' ELSE 'failed' END,
        result = (SELECT group_concat(text, char(10) || char(10)) FROM
                  (SELECT text FROM items WHERE turn_id = NEW.id AND kind = 'assistant' ORDER BY seq)),
        error = CASE WHEN NEW.status = 'completed' THEN NULL ELSE coalesce(NEW.error_code || ': ' || NEW.error_message, NEW.status) END,
        finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE child_turn_id = NEW.id AND status NOT IN ('completed', 'failed', 'cancelled');
    UPDATE turns SET status = 'queued', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE id IN (SELECT root_turn_id FROM delegations WHERE child_turn_id = NEW.id) AND status = 'waiting';
END;
