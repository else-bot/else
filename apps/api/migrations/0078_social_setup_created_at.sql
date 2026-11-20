-- The sidebar shows how long a pending setup has waited. Older rows take
-- their turn's last update as an approximation.
ALTER TABLE social_setups ADD COLUMN created_at TEXT;
UPDATE social_setups
SET created_at = (SELECT t.updated_at FROM turns t WHERE t.id = social_setups.turn_id);
CREATE INDEX social_setups_pending ON social_setups(bot_id, created_at) WHERE status = 'pending';
