-- What a background job is for, in the bot's words, shown as the title of a
-- wait on it. NULL for jobs started before this column and for routine
-- checks.
ALTER TABLE guest_jobs ADD COLUMN purpose TEXT;
