-- Preset avatars by name. Rows without one get a stable avatar derived from
-- the bot ID when read; the main bot starts as the default blue star.
ALTER TABLE bots ADD COLUMN avatar_shape text;
ALTER TABLE bots ADD COLUMN avatar_color text
    CHECK ((avatar_shape IS NULL) = (avatar_color IS NULL));

UPDATE bots SET avatar_shape = 'star', avatar_color = 'blue'
WHERE id = (SELECT default_bot_id FROM installation);
