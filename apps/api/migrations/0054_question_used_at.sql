-- An approved tool call the gate let run: when it started, and what its
-- tool returned. A use without an outcome is still running or unknown.
ALTER TABLE owner_questions ADD COLUMN used_at TEXT;
ALTER TABLE owner_questions ADD COLUMN use_outcome TEXT
    CHECK (use_outcome IS NULL OR (used_at IS NOT NULL AND use_outcome IN ('completed', 'error')));
