-- The owner's sidebar order for categories, shared by every desktop. NULL
-- means never placed: those categories follow the placed ones by name.
ALTER TABLE bot_categories ADD COLUMN position INTEGER CHECK (position >= 0);
