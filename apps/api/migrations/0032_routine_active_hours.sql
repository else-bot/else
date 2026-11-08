-- When a routine may start, in its time zone: a Monday-first day mask and
-- a span in minutes after midnight, end excluded. All NULL: any time.
-- Routines saved before this release keep running at any time.
ALTER TABLE routines ADD COLUMN active_days INTEGER CHECK (active_days BETWEEN 1 AND 127);
ALTER TABLE routines ADD COLUMN active_start INTEGER CHECK (active_start BETWEEN 0 AND 1439);
ALTER TABLE routines ADD COLUMN active_end INTEGER CHECK (
    (active_days IS NULL AND active_start IS NULL AND active_end IS NULL)
    OR (active_days IS NOT NULL AND active_start IS NOT NULL AND active_end IS NOT NULL
        AND active_start < active_end AND active_end <= 1439));

-- The IANA time zone the owner's desktop app last reported.
ALTER TABLE installation ADD COLUMN owner_timezone TEXT
    CHECK (owner_timezone <> '' AND length(CAST(owner_timezone AS BLOB)) <= 64);
