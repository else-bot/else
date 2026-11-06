-- A routine's own model: NULL inherits the bot's live choice. A model the
-- bot was asked for but could not resolve is kept for the owner to see.
ALTER TABLE routines ADD COLUMN model TEXT CHECK (model IS NULL OR json_valid(model));
ALTER TABLE routines ADD COLUMN requested_model TEXT CHECK (model IS NULL OR requested_model IS NULL);

-- Turns that run on a routine's own model keep their native sessions apart
-- from the bot's chat sessions ('' is the chat lane).
ALTER TABLE turns ADD COLUMN session_lane TEXT NOT NULL DEFAULT '';
ALTER TABLE provider_sessions ADD COLUMN lane TEXT NOT NULL DEFAULT '';
DROP INDEX provider_sessions_one_active;
CREATE UNIQUE INDEX provider_sessions_one_active
    ON provider_sessions (conversation_id, provider_id, lane) WHERE status = 'active';
