CREATE TABLE legacy_bot_descriptions (
    bot_id BLOB NOT NULL PRIMARY KEY REFERENCES bots (id),
    description text NOT NULL
) STRICT;

INSERT INTO legacy_bot_descriptions (bot_id, description) SELECT id, role FROM bots;

ALTER TABLE bots DROP COLUMN role;
