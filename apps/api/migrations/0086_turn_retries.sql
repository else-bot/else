-- The latest attempt of a turn the owner retried. A repeat of that retry
-- starts nothing, and a choice card the failed attempt left cancelled does
-- not answer the same question asked again after it.
ALTER TABLE turns ADD COLUMN retried_attempt INTEGER
    CHECK (retried_attempt IS NULL OR retried_attempt >= 1);
