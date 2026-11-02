-- Provider handoffs (POC-020). When an owner turn runs on a different
-- provider than the turn before it, the API freezes the settled history,
-- asks the outgoing provider for a bounded no-tool checkpoint (or records
-- why the deterministic fallback is used), and gives the receiving session
-- the checkpoint with the history it missed. Delivery is tracked like the
-- turn's own: persisted before sending, accepted only on correlated
-- acknowledgment, uncertain when that is unknown.
CREATE TABLE handoffs (
    id uuid PRIMARY KEY,
    bot_id uuid NOT NULL REFERENCES bots (id),
    conversation_id uuid NOT NULL REFERENCES conversations (id),
    turn_id uuid NOT NULL REFERENCES turns (id),
    attempt integer NOT NULL,
    source_provider_id text NOT NULL,
    target_provider_id text NOT NULL,
    source_session_id uuid REFERENCES provider_sessions (id),
    -- History through this owner turn was frozen for the handoff.
    boundary_turn_no bigint NOT NULL,
    kind text NOT NULL CHECK (kind IN ('llm', 'fallback')),
    reason text CHECK (octet_length(reason) <= 2000),
    checkpoint text CHECK (octet_length(checkpoint) <= 8192),
    prompt_version text NOT NULL,
    selected_turns bigint[] NOT NULL DEFAULT '{}',
    omitted_turns bigint[] NOT NULL DEFAULT '{}',
    payload_sha256 bytea CHECK (octet_length(payload_sha256) = 32),
    target_session_id uuid REFERENCES provider_sessions (id),
    state text NOT NULL DEFAULT 'prepared'
        CHECK (state IN ('prepared', 'sending', 'accepted', 'uncertain')),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (turn_id, attempt),
    CHECK ((kind = 'llm') = (checkpoint IS NOT NULL))
);
CREATE INDEX handoffs_bot ON handoffs (bot_id, created_at);
