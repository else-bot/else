ALTER TABLE items ADD COLUMN assistant_parts TEXT
    CHECK (assistant_parts IS NULL OR
        (kind = 'assistant' AND json_valid(assistant_parts) AND json_type(assistant_parts) = 'array'));
