-- POC-028 review: a saved screenshot keeps its capture metadata (screen
-- size, scale, pointer, capture ID and time), so a later look at it maps
-- image coordinates to the screen as the original did.
ALTER TABLE artifacts ADD COLUMN meta jsonb;
