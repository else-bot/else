-- Where each mail trigger's message is now. A bot's archive, trash, move
-- or delete records it, and so does the owner's reader when it finds the
-- message elsewhere by its Message-ID or finds it nowhere.
ALTER TABLE connector_events ADD COLUMN message_id TEXT
    CHECK (message_id IS NULL OR (message_id <> '' AND length(CAST(message_id AS BLOB)) <= 255));
ALTER TABLE connector_events ADD COLUMN subject TEXT
    CHECK (subject IS NULL OR length(CAST(subject AS BLOB)) <= 1024);
ALTER TABLE connector_events ADD COLUMN sender TEXT
    CHECK (sender IS NULL OR length(CAST(sender AS BLOB)) <= 1024);
ALTER TABLE connector_events ADD COLUMN location TEXT NOT NULL DEFAULT 'arrived'
    CHECK (location IN ('arrived', 'archived', 'trashed', 'moved', 'deleted'));
ALTER TABLE connector_events ADD COLUMN moved_folder TEXT
    CHECK (moved_folder IS NULL OR (moved_folder <> '' AND length(CAST(moved_folder AS BLOB)) <= 512));
ALTER TABLE connector_events ADD COLUMN moved_uid_validity INTEGER
    CHECK (moved_uid_validity BETWEEN 0 AND 4294967295);
ALTER TABLE connector_events ADD COLUMN moved_uid INTEGER
    CHECK (moved_uid BETWEEN 1 AND 4294967295);
ALTER TABLE connector_events ADD COLUMN located_turn_id BLOB REFERENCES turns (id);
ALTER TABLE connector_events ADD COLUMN located_at TEXT
    CHECK ((location = 'arrived') = (located_at IS NULL))
    CHECK ((location IN ('archived', 'trashed', 'moved')) = (moved_folder IS NOT NULL))
    CHECK ((moved_uid_validity IS NULL) = (moved_uid IS NULL))
    CHECK (moved_uid IS NULL OR moved_folder IS NOT NULL)
    CHECK (located_turn_id IS NULL OR location <> 'arrived');

CREATE INDEX connector_events_moved ON connector_events
    (account_id, moved_folder, moved_uid_validity, moved_uid) WHERE moved_uid IS NOT NULL;
