-- A draft may carry attachments (app::outgoing::DraftFile). The owner may
-- remove some while editing, never add, repeat, reorder, or change one;
-- everything else outside the subject and body stays fixed.
DROP TRIGGER outgoing_drafts_fixed_content;

CREATE TRIGGER outgoing_drafts_fixed_content BEFORE UPDATE OF content ON outgoing_drafts
WHEN json_remove(OLD.content, '$.subject', '$.body', '$.attachments')
        IS NOT json_remove(NEW.content, '$.subject', '$.body', '$.attachments')
    OR EXISTS (SELECT 1 FROM json_each(NEW.content, '$.attachments') AS n
               WHERE NOT EXISTS (SELECT 1 FROM json_each(OLD.content, '$.attachments') AS o
                                 WHERE o.value = n.value))
    OR EXISTS (SELECT 1 FROM json_each(NEW.content, '$.attachments') AS a
               JOIN json_each(NEW.content, '$.attachments') AS b ON a.key < b.key
               WHERE NOT (SELECT min(o.key) FROM json_each(OLD.content, '$.attachments') AS o
                          WHERE o.value = a.value)
                       < (SELECT min(o.key) FROM json_each(OLD.content, '$.attachments') AS o
                          WHERE o.value = b.value))
BEGIN
    SELECT RAISE(ABORT, 'only the body, an email subject, and removing attachments can change a draft');
END;
