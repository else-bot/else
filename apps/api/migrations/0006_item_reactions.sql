ALTER TABLE items ADD COLUMN reaction TEXT
    CHECK (reaction IS NULL OR (kind = 'user' AND json_valid(reaction)));
