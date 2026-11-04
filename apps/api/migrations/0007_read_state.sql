-- How far the owner has read the bot's replies in its owner conversation:
-- the assistant item seq and the UTF-8 bytes of its text seen, compared as
-- a pair. `marked_unread` is the owner's explicit flag; reading never
-- rewinds the position. History from before this migration counts as read.
ALTER TABLE conversations ADD COLUMN read_item_seq INTEGER NOT NULL DEFAULT 0
    CHECK (read_item_seq >= 0);
ALTER TABLE conversations ADD COLUMN read_text_bytes INTEGER NOT NULL DEFAULT 0
    CHECK (read_text_bytes >= 0);
ALTER TABLE conversations ADD COLUMN marked_unread INTEGER NOT NULL DEFAULT false
    CHECK (marked_unread IN (0, 1));

UPDATE conversations SET (read_item_seq, read_text_bytes) = (
    SELECT i.seq, length(CAST(i.text AS BLOB)) FROM items i
    WHERE i.conversation_id = conversations.id AND i.kind = 'assistant'
    ORDER BY i.seq DESC LIMIT 1
)
WHERE kind = 'owner'
  AND EXISTS (SELECT 1 FROM items i WHERE i.conversation_id = conversations.id AND i.kind = 'assistant');
