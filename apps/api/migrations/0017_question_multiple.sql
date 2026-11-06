ALTER TABLE owner_questions ADD COLUMN multiple INTEGER NOT NULL DEFAULT 0
    CHECK (multiple IN (0, 1) AND (multiple = 0 OR kind = 'choice'));
