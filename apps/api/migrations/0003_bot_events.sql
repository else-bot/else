-- Per-bot event journal for stream replay. History tables remain the source
-- of snapshots; this journal only orders committed changes for streaming.

ALTER TABLE bots ADD COLUMN last_event_seq bigint NOT NULL DEFAULT 0;

CREATE TABLE bot_events (
    bot_id uuid NOT NULL REFERENCES bots (id),
    seq bigint NOT NULL CHECK (seq >= 1),
    kind text NOT NULL
        CHECK (kind IN ('item.upsert', 'item.delta', 'turn.updated', 'bot.updated')),
    data jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (bot_id, seq)
);
