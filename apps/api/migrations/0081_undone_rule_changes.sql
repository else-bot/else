-- Stamp the conversation row of each rule change the owner undid before the
-- row recorded it: the next version is the owner's restore of the one before.
UPDATE items SET rules_update = json_set(rules_update, '$.undone_at', (
    SELECT u.created_at FROM conversations c
    JOIN bot_rules u ON u.bot_id = c.bot_id
    WHERE c.id = items.conversation_id
      AND u.version = json_extract(items.rules_update, '$.version') + 1
      AND u.change = 'restored'
      AND u.restored_from = json_extract(items.rules_update, '$.version') - 1
))
WHERE rules_update IS NOT NULL
  AND json_extract(rules_update, '$.undone_at') IS NULL
  AND EXISTS (
    SELECT 1 FROM conversations c
    JOIN bot_rules u ON u.bot_id = c.bot_id
    WHERE c.id = items.conversation_id
      AND u.version = json_extract(items.rules_update, '$.version') + 1
      AND u.change = 'restored'
      AND u.restored_from = json_extract(items.rules_update, '$.version') - 1
  );
