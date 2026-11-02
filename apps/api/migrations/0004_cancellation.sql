-- Durable Stop work receipts. A cancellation covers every turn accepted up
-- to its boundary; later turns are never affected by a retry.

ALTER TABLE request_receipts
    ADD COLUMN state text CHECK (state IN ('pending', 'done')),
    ADD COLUMN boundary_turn_no bigint,
    ADD COLUMN completed_at timestamptz,
    ADD CONSTRAINT request_receipts_cancel_fields CHECK (
        (operation = 'cancel') = (state IS NOT NULL AND boundary_turn_no IS NOT NULL)
    );

CREATE INDEX request_receipts_pending_cancel ON request_receipts (bot_id)
    WHERE operation = 'cancel' AND state = 'pending';
