-- Social actions keep the receipt shape and settlement time mail actions
-- have: `result` holds the whole receipt, not just the platform's reply.
ALTER TABLE social_actions ADD COLUMN finished_at TEXT;

UPDATE social_actions SET
    finished_at = CASE WHEN status <> 'dispatching'
                       THEN strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') END,
    result = json_object(
        'action_id', lower(substr(hex(action_id), 1, 8) || '-' || substr(hex(action_id), 9, 4) || '-'
            || substr(hex(action_id), 13, 4) || '-' || substr(hex(action_id), 17, 4) || '-'
            || substr(hex(action_id), 21)),
        'status', status,
        'result', json(result))
WHERE result IS NOT NULL;
