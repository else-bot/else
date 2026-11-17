-- A delegation's short title, written by the delegating bot, and what the
-- delegated bot declared its task needs from the owner (owner_notices::Need).
-- NULL owner_need means it declared nothing: older tasks, or a bot that
-- forgot. owner_line is the bot's one line for 'fyi'. A 'decision' or
-- 'blocked' ask keeps its text and lifecycle in owner_notices.
ALTER TABLE delegations ADD COLUMN title TEXT;
ALTER TABLE delegations ADD COLUMN owner_need TEXT
    CHECK (owner_need IN ('nothing', 'fyi', 'decision', 'blocked'));
ALTER TABLE delegations ADD COLUMN owner_line TEXT;
