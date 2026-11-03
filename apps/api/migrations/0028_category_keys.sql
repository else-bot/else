-- POC-030 review: uniqueness by a Unicode case-folded key (Straße and
-- STRASSE are the same name), computed by the API; a portable order by
-- that key's bytes; and a deleting mark so deletion never holds more than
-- one bot row lock (it unassigns bots one at a time, then removes the row).
ALTER TABLE bot_categories ADD COLUMN name_key text;
UPDATE bot_categories SET name_key = lower(name);
ALTER TABLE bot_categories ALTER COLUMN name_key SET NOT NULL;
DROP INDEX bot_categories_name;
CREATE UNIQUE INDEX bot_categories_name_key ON bot_categories (name_key);
ALTER TABLE bot_categories ADD COLUMN deleting boolean NOT NULL DEFAULT false;
