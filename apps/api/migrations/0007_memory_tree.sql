-- Memory summary tree and saved views (POC-014). See docs/research/memory.md.

-- Byte budgets for new memory generations. Changing them affects only a
-- generation created afterwards (an explicit rebuild).
ALTER TABLE installation ADD COLUMN memory_recipe jsonb NOT NULL
    DEFAULT '{"version":1,"node_bytes":512,"view_high":128000,"view_low":64000}';

-- Each generation of a bot's tree records the recipe it was built with.
-- `watermark` is the log length when a rebuild started (null for the first).
CREATE TABLE memory_generation (
    bot_id uuid NOT NULL REFERENCES memory_store (bot_id),
    generation integer NOT NULL CHECK (generation >= 1),
    recipe jsonb NOT NULL,
    watermark bigint,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (bot_id, generation)
);

-- The saved view: an ordered list of [start, span] nodes of the active
-- generation covering entries 0..=published_seq. It is saved state: restart
-- loads it unchanged, and only publication or an explicit rebuild changes it
-- (each change bumps `view_revision`).
ALTER TABLE memory_store
    ADD COLUMN generation integer NOT NULL DEFAULT 1,
    ADD COLUMN published_seq bigint NOT NULL DEFAULT -1,
    ADD COLUMN view_revision bigint NOT NULL DEFAULT 0,
    ADD COLUMN view_nodes jsonb NOT NULL DEFAULT '[]',
    ADD COLUMN compacting boolean NOT NULL DEFAULT false,
    ADD COLUMN building_generation integer,
    ADD CONSTRAINT memory_store_published CHECK (published_seq >= -1 AND published_seq < next_seq),
    ADD CONSTRAINT memory_store_building CHECK (building_generation IS NULL OR building_generation > generation);

-- Immutable binary nodes: `start_seq + span` covers entries
-- start_seq .. start_seq + span - 1; spans are powers of two, starts aligned.
CREATE TABLE memory_node (
    bot_id uuid NOT NULL,
    generation integer NOT NULL,
    start_seq bigint NOT NULL CHECK (start_seq >= 0),
    span bigint NOT NULL CHECK (span > 0 AND span <= (1::bigint << 60)),
    body text NOT NULL CHECK (octet_length(body) BETWEEN 1 AND 65536),
    -- verbatim: one original copied; join: two children concatenated;
    -- model: a summary (POC-015).
    method text NOT NULL CHECK (method IN ('verbatim', 'join', 'model')),
    input_hash bytea NOT NULL CHECK (octet_length(input_hash) = 32),
    audit jsonb NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (bot_id, generation, start_seq, span),
    FOREIGN KEY (bot_id, generation) REFERENCES memory_generation (bot_id, generation),
    CHECK ((span & (span - 1)) = 0 AND start_seq % span = 0)
);

CREATE TRIGGER memory_node_no_update BEFORE UPDATE OR DELETE ON memory_node
    FOR EACH ROW EXECUTE FUNCTION memory_entry_immutable();

-- Work to build one node. A claim sets an opaque lease token; completion
-- must present it, so a worker whose lease was replaced cannot publish.
CREATE TABLE memory_job (
    bot_id uuid NOT NULL,
    generation integer NOT NULL,
    start_seq bigint NOT NULL,
    span bigint NOT NULL,
    available_at timestamptz NOT NULL DEFAULT now(),
    lease_token uuid,
    lease_until timestamptz,
    attempts integer NOT NULL DEFAULT 0,
    -- Why the job waits (for example `summary_unavailable`).
    blocked_reason text,
    last_error text CHECK (octet_length(last_error) <= 1000),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (bot_id, generation, start_seq, span),
    FOREIGN KEY (bot_id, generation) REFERENCES memory_generation (bot_id, generation),
    CHECK (span > 0 AND (span & (span - 1)) = 0 AND start_seq >= 0 AND start_seq % span = 0),
    CHECK ((lease_token IS NULL) = (lease_until IS NULL))
);
CREATE INDEX memory_job_ready ON memory_job (available_at, bot_id, start_seq, span);

-- Existing stores start generation 1 with the installation recipe, and
-- every existing entry gets its leaf job.
INSERT INTO memory_generation (bot_id, generation, recipe)
SELECT s.bot_id, 1, coalesce((SELECT memory_recipe FROM installation),
       '{"version":1,"node_bytes":512,"view_high":128000,"view_low":64000}')
FROM memory_store s;

INSERT INTO memory_job (bot_id, generation, start_seq, span)
SELECT bot_id, 1, seq, 1 FROM memory_entry;
