ALTER TABLE provider_sessions ADD COLUMN default_skills TEXT NOT NULL DEFAULT '{}'
    CHECK (json_valid(default_skills) AND json_type(default_skills) = 'object');
