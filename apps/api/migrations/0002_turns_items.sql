-- Durable owner messages, ordered history, and turn attempts.

-- Set while Stop work is confirming; new messages are rejected meanwhile.
ALTER TABLE bots ADD COLUMN cancel_pending boolean NOT NULL DEFAULT false;

ALTER TABLE conversations
    ADD COLUMN last_item_seq bigint NOT NULL DEFAULT 0,
    ADD COLUMN last_turn_no bigint NOT NULL DEFAULT 0;

-- One row per accepted owner message. The turn records the provider chosen
-- at acceptance and a snapshot of the effective execution profile; neither
-- changes afterwards. Turns never retry automatically, so one row is one
-- provider attempt.
CREATE TABLE turns (
    id uuid PRIMARY KEY,
    bot_id uuid NOT NULL REFERENCES bots (id),
    conversation_id uuid NOT NULL REFERENCES conversations (id),
    request_id uuid NOT NULL,
    request_hash bytea NOT NULL CHECK (octet_length(request_hash) = 32),
    turn_no bigint NOT NULL CHECK (turn_no >= 1),
    owner_item_id uuid NOT NULL UNIQUE,
    provider_id text NOT NULL,
    profile_revision bigint,
    profile jsonb,
    status text NOT NULL DEFAULT 'queued' CHECK (status IN (
        'queued', 'preparing', 'sending', 'running', 'cancelling',
        'completed', 'interrupted', 'failed', 'uncertain')),
    error_code text,
    error_message text,
    -- True when an effect may or may not have happened.
    effect_uncertain boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    started_at timestamptz,
    finished_at timestamptz,
    UNIQUE (conversation_id, request_id),
    UNIQUE (conversation_id, turn_no),
    CHECK ((profile IS NULL) = (profile_revision IS NULL))
);

-- At most one provider attempt per bot is past the queue at a time.
CREATE UNIQUE INDEX turns_one_active_per_bot ON turns (bot_id)
    WHERE status IN ('preparing', 'sending', 'running', 'cancelling');
CREATE INDEX turns_queued ON turns (bot_id, turn_no) WHERE status = 'queued';

-- Visible conversation history. `seq` orders bubbles within a conversation.
CREATE TABLE items (
    id uuid PRIMARY KEY,
    conversation_id uuid NOT NULL REFERENCES conversations (id),
    seq bigint NOT NULL CHECK (seq >= 1),
    turn_id uuid NOT NULL REFERENCES turns (id),
    kind text NOT NULL CHECK (kind IN ('user', 'assistant', 'status')),
    text text NOT NULL,
    provider_id text,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (conversation_id, seq)
);
CREATE INDEX items_turn ON items (turn_id);
