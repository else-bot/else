-- The provider attempt that wrote an assistant item. A turn resumed after a
-- wait writes its reply in a later attempt, so the owner can still see the
-- reply that ended the earlier one.
ALTER TABLE items ADD COLUMN attempt INTEGER CHECK (attempt IS NULL OR (kind = 'assistant' AND attempt >= 0));
