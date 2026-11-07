-- Earlier notices are recovered only when their exact creator and created bot
-- ids match, since bot names may contain the notice's own separators.
ALTER TABLE items ADD COLUMN bot_created TEXT
    CHECK (bot_created IS NULL OR (kind = 'status' AND json_valid(bot_created)
        AND json_type(bot_created) = 'object'));

WITH named AS (
    SELECT id, name,
        substr(h, 1, 8) || '-' || substr(h, 9, 4) || '-' || substr(h, 13, 4) || '-'
            || substr(h, 17, 4) || '-' || substr(h, 21) AS uuid
    FROM (SELECT id, name, lower(hex(id)) AS h FROM bots)
),
notices AS (
    SELECT i.id, i.text, c.uuid AS creator_uuid, c.name AS creator_name, b.uuid AS bot_uuid,
        length(c.name || ' (bot_id ' || c.uuid || ') created ') AS head,
        instr(i.text, ' (bot_id ' || b.uuid || ').' || char(10) || 'Purpose: ') AS tail
    FROM items i
    JOIN turns t ON t.id = i.turn_id AND t.provider_id = 'system'
    JOIN named b ON b.id = t.request_id
    JOIN named c ON substr(i.text, 1, length(c.name || ' (bot_id ' || c.uuid || ') created '))
        = c.name || ' (bot_id ' || c.uuid || ') created '
    WHERE i.kind = 'status'
)
UPDATE items SET bot_created = (
    SELECT json_object(
        'bot_id', n.bot_uuid,
        'name', substr(n.text, n.head + 1, n.tail - n.head - 1),
        'creator_bot_id', n.creator_uuid,
        'creator_name', n.creator_name,
        'purpose', substr(n.text, n.tail + 57))
    FROM notices n WHERE n.id = items.id)
WHERE id IN (SELECT id FROM notices WHERE tail > head + 1);
