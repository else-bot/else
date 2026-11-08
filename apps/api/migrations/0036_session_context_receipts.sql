-- Content hashes of the context blocks (bot registry, saved permissions) a
-- native session holds, so a turn resends a block only after it changes.
-- Existing sessions start empty and receive every block once.
ALTER TABLE provider_sessions ADD COLUMN context_receipts TEXT NOT NULL DEFAULT '{}'
    CHECK (json_valid(context_receipts));
