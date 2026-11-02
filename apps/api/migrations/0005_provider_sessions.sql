-- Native provider sessions per conversation. The conversation in PostgreSQL
-- stays authoritative; a session is a replaceable cache of provider context.

CREATE TABLE provider_sessions (
    id uuid PRIMARY KEY,
    bot_id uuid NOT NULL REFERENCES bots (id),
    conversation_id uuid NOT NULL REFERENCES conversations (id),
    provider_id text NOT NULL,
    native_session_id text NOT NULL,
    -- Covers runtime, prompt version, tool policy, profile, and agent policy.
    fingerprint text NOT NULL,
    status text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'retired')),
    -- Settled history up to this owner turn was given to the session.
    considered_through_turn bigint NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX provider_sessions_one_active
    ON provider_sessions (conversation_id, provider_id) WHERE status = 'active';

ALTER TABLE turns
    ADD COLUMN provider_session_id uuid REFERENCES provider_sessions (id),
    ADD COLUMN native_turn_id text,
    -- Settings the provider reported for this attempt, when it reports them.
    ADD COLUMN reported_profile jsonb;
