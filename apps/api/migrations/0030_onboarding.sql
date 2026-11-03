-- When the owner completed the welcome screen (named the main bot and chose
-- its provider and model). NULL shows it, also on existing installations.
ALTER TABLE installation ADD COLUMN onboarded_at timestamptz;
