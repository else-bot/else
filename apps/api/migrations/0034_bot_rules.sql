-- Each row is one saved version of a bot's rules file; the highest version
-- is current. Versions are never edited, so history and restores stay exact.
CREATE TABLE bot_rules (
    bot_id BLOB NOT NULL REFERENCES bots(id),
    version INTEGER NOT NULL CHECK(version >= 1),
    markdown TEXT NOT NULL CHECK(length(CAST(markdown AS BLOB)) <= 32000),
    section TEXT NOT NULL CHECK(length(section) <= 80),
    change TEXT NOT NULL CHECK(change IN ('added','changed','removed','restored')),
    owner_words TEXT NOT NULL CHECK(length(owner_words) <= 1000),
    author TEXT NOT NULL CHECK(author IN ('bot','owner')),
    restored_from INTEGER CHECK((change = 'restored') = (restored_from IS NOT NULL) AND (restored_from IS NULL OR restored_from >= 0)),
    diff TEXT NOT NULL CHECK(json_valid(diff) AND json_type(diff) = 'array'),
    turn_id BLOB REFERENCES turns(id),
    created_at TEXT NOT NULL DEFAULT(strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    PRIMARY KEY (bot_id, version)
) STRICT;

CREATE TRIGGER bot_rules_append_only_update BEFORE UPDATE ON bot_rules
BEGIN SELECT RAISE(ABORT, 'bot_rules versions are immutable'); END;
CREATE TRIGGER bot_rules_append_only_delete BEFORE DELETE ON bot_rules
BEGIN SELECT RAISE(ABORT, 'bot_rules versions are immutable'); END;

ALTER TABLE items ADD COLUMN rules_update TEXT
    CHECK (rules_update IS NULL OR (kind = 'status' AND json_valid(rules_update)
        AND json_type(rules_update) = 'object'));
