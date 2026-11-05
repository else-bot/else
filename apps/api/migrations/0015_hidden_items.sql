ALTER TABLE items ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0 CHECK (hidden IN (0, 1) AND (NOT hidden OR kind = 'user'));
