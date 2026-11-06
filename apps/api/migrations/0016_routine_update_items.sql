ALTER TABLE items ADD COLUMN routine_update TEXT
    CHECK (routine_update IS NULL OR (kind = 'status' AND json_valid(routine_update)
        AND json_type(routine_update) = 'object'));
