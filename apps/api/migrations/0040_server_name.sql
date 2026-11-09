-- The owner's name for this server, shown by every desktop. NULL: never
-- named, so each desktop keeps its own name.
ALTER TABLE installation ADD COLUMN server_name TEXT
    CHECK (server_name <> '' AND length(server_name) <= 80);
