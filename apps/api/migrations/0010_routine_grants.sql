-- A routine's prompt revision counts prompt changes: it only grows, so a
-- prompt edited back to an earlier text is still a new revision. Each run
-- records the revision it was admitted with; runs from before this
-- migration have none and never use a routine grant.
ALTER TABLE routines ADD COLUMN prompt_revision INTEGER NOT NULL DEFAULT 1;
ALTER TABLE routine_runs ADD COLUMN prompt_revision INTEGER;

UPDATE owner_questions
SET choices = '[{"id":"approve","label":"Approve once"},{"id":"deny","label":"Deny"}]'
WHERE kind = 'destructive' AND status = 'pending'
  AND turn_id IN (SELECT turn_id FROM routine_runs);

-- The owner's standing approval of destructive actions for one routine,
-- given by choosing "Always allow for this routine" on a card its run
-- showed. It covers runs of that routine admitted with this prompt
-- revision only. External actions are never allowed in advance.
CREATE TABLE routine_grants (
    routine_id BLOB NOT NULL PRIMARY KEY REFERENCES routines (id),
    prompt_revision INTEGER NOT NULL,
    question_id BLOB NOT NULL REFERENCES owner_questions (id),
    granted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))
) STRICT;
