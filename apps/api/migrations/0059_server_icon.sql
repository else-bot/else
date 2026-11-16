-- The owner's icon for this server, shown by every desktop. NULL: never
-- chosen, so each desktop keeps its own. A picture icon keeps its PNG in
-- server_picture; every other icon has none.
ALTER TABLE installation ADD COLUMN server_icon TEXT CHECK (json_valid(server_icon));
ALTER TABLE installation ADD COLUMN server_picture BLOB
    CHECK (length(server_picture) <= 131072);
