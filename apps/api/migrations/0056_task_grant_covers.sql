-- A task grant now names what it covers in its scope key, after `task:`.
-- A grant from an approval card covers the kind of call the card approved.
-- A grant saved by confirm_permission, or without its card, keeps covering both.
UPDATE bot_permissions SET scope = 'task:' || coalesce(
    (SELECT CASE
        WHEN json_extract(q.action, '$.host') IS NOT NULL THEN 'host'
        WHEN json_extract(q.action, '$.tool') IS NOT NULL THEN 'computer'
     END
     FROM owner_questions q WHERE q.id = bot_permissions.question_id),
    'both') || substr(scope, 5)
WHERE scope GLOB 'task:destructive:*' OR scope GLOB 'task:external:*';

UPDATE owner_questions SET action = json_set(action, '$.permission.covers', 'both')
WHERE json_extract(action, '$.permission.scope') = 'task';
