-- A bot's mail trigger is its readable inbox grant with instructions: what
-- to do when new mail arrives. Turning wake on gives a grant instructions,
-- so a paused trigger keeps its card. Grants that already wake get the
-- default instructions.
ALTER TABLE connector_grants ADD COLUMN wake_instructions TEXT
    CHECK (wake_instructions IS NULL
           OR length(CAST(wake_instructions AS BLOB)) BETWEEN 1 AND 8000);

UPDATE connector_grants SET wake_instructions = 'Handle new mail as the owner asked.'
WHERE can_wake;
