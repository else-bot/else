-- Per-bot original memory log (POC-013). Summary nodes, jobs, and saved
-- views arrive with POC-014. See docs/research/memory.md.

-- One row per bot with memory. Its row lock serializes appends for that bot
-- only (taken FOR NO KEY UPDATE); a rolled-back append consumes no sequence.
CREATE TABLE memory_store (
    bot_id uuid PRIMARY KEY REFERENCES bots (id),
    next_seq bigint NOT NULL DEFAULT 0 CHECK (next_seq >= 0)
);

-- Immutable originals: owner and assistant messages, explicit owner
-- corrections, and observable tool activity. Never private reasoning.
CREATE TABLE memory_entry (
    bot_id uuid NOT NULL REFERENCES memory_store (bot_id),
    -- Zero-based, contiguous per bot, in commit order.
    seq bigint NOT NULL CHECK (seq >= 0),
    -- Bot-scoped idempotency key, e.g. `item:<id>`; a retry with the same
    -- key and fingerprint returns the existing entry.
    request_key text NOT NULL CHECK (request_key <> '' AND octet_length(request_key) <= 200),
    request_fingerprint bytea NOT NULL CHECK (octet_length(request_fingerprint) = 32),
    kind text NOT NULL CHECK (kind IN (
        'owner_message', 'assistant_message', 'owner_correction', 'tool_activity')),
    -- Who produced the text. Only the owner's own words are `owner`.
    authority text NOT NULL CHECK (authority IN ('owner', 'assistant', 'tool', 'external')),
    body text NOT NULL CHECK (octet_length(body) <= 64000),
    body_sha256 bytea NOT NULL CHECK (octet_length(body_sha256) = 32),
    -- Where the original lives: conversation, item, turn, tool, provider.
    source jsonb NOT NULL,
    -- Long originals are split at UTF-8 boundaries into ordered parts.
    part_index integer NOT NULL DEFAULT 0 CHECK (part_index >= 0),
    part_count integer NOT NULL DEFAULT 1 CHECK (part_count >= 1 AND part_index < part_count),
    original_sha256 bytea NOT NULL CHECK (octet_length(original_sha256) = 32),
    -- An explicit owner correction of an earlier owner assertion.
    supersedes_seq bigint,
    created_at timestamptz NOT NULL DEFAULT now(),
    search tsvector GENERATED ALWAYS AS (to_tsvector('simple', body)) STORED,
    PRIMARY KEY (bot_id, seq),
    UNIQUE (bot_id, request_key, part_index),
    FOREIGN KEY (bot_id, supersedes_seq) REFERENCES memory_entry (bot_id, seq),
    CHECK (supersedes_seq IS NULL OR (kind = 'owner_correction' AND authority = 'owner' AND supersedes_seq < seq)),
    CHECK ((kind = 'owner_correction') = (supersedes_seq IS NOT NULL)),
    CHECK (kind NOT IN ('owner_message', 'owner_correction') OR authority = 'owner'),
    CHECK (authority <> 'owner' OR kind IN ('owner_message', 'owner_correction'))
);
CREATE INDEX memory_entry_search ON memory_entry USING gin (search);
CREATE INDEX memory_entry_supersedes ON memory_entry (bot_id, supersedes_seq)
    WHERE supersedes_seq IS NOT NULL;

-- Originals are immutable. A future explicit purge must lift this on purpose.
CREATE FUNCTION memory_entry_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'memory entries are immutable' USING ERRCODE = 'integrity_constraint_violation';
END $$;
CREATE TRIGGER memory_entry_no_update BEFORE UPDATE OR DELETE ON memory_entry
    FOR EACH ROW EXECUTE FUNCTION memory_entry_immutable();

-- A correction may only replace an owner assertion of the same bot.
CREATE FUNCTION memory_correction_target() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.supersedes_seq IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM memory_entry
        WHERE bot_id = NEW.bot_id AND seq = NEW.supersedes_seq AND authority = 'owner'
    ) THEN
        RAISE EXCEPTION 'a correction must supersede an owner entry of the same bot'
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER memory_entry_correction BEFORE INSERT ON memory_entry
    FOR EACH ROW EXECUTE FUNCTION memory_correction_target();

-- Owner-configured cross-bot read access: `reader` may read `target`'s
-- memory. Same-bot reads need no row.
CREATE TABLE memory_read_grants (
    reader_bot_id uuid NOT NULL REFERENCES bots (id),
    target_bot_id uuid NOT NULL REFERENCES bots (id),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (reader_bot_id, target_bot_id),
    CHECK (reader_bot_id <> target_bot_id)
);

-- Existing conversation history becomes each bot's first memory, in
-- conversation order. Text over the per-entry limit is split into parts.
INSERT INTO memory_store (bot_id) SELECT id FROM bots;

WITH items_in AS (
    SELECT c.bot_id, i.id, i.seq AS item_seq, i.kind, i.text, i.turn_id, i.conversation_id,
           i.provider_id, i.created_at,
           greatest(1, ceil(char_length(i.text) / 16000.0)::int) AS part_count
    FROM items i
    JOIN conversations c ON c.id = i.conversation_id
    JOIN turns t ON t.id = i.turn_id
    WHERE i.kind IN ('user', 'assistant') AND i.text <> ''
      AND (i.kind = 'user' OR t.status IN ('completed', 'interrupted', 'failed', 'uncertain'))
),
parts AS (
    -- 16,000 characters are at most 64,000 bytes and end on a character.
    SELECT items_in.*, p AS part_index,
           substr(text, p * 16000 + 1, 16000) AS body,
           row_number() OVER (PARTITION BY bot_id ORDER BY item_seq, p) - 1 AS seq
    FROM items_in, generate_series(0, part_count - 1) AS p
)
INSERT INTO memory_entry (bot_id, seq, request_key, request_fingerprint, kind, authority, body,
                          body_sha256, source, part_index, part_count, original_sha256, created_at)
SELECT bot_id, seq, 'item:' || id,
       sha256(convert_to('backfill:' || id, 'UTF8')),
       CASE kind WHEN 'user' THEN 'owner_message' ELSE 'assistant_message' END,
       CASE kind WHEN 'user' THEN 'owner' ELSE 'assistant' END,
       body, sha256(convert_to(body, 'UTF8')),
       jsonb_build_object('conversation_id', conversation_id, 'item_id', id, 'turn_id', turn_id,
                          'provider_id', provider_id),
       part_index, part_count, sha256(convert_to(text, 'UTF8')), created_at
FROM parts;

UPDATE memory_store s SET next_seq = coalesce(
    (SELECT max(seq) + 1 FROM memory_entry e WHERE e.bot_id = s.bot_id), 0);
