-- Provider failures of a job, separate from claims (POC-015 review): waiting
-- for capacity does not lengthen the backoff after a real failure.
ALTER TABLE memory_job ADD COLUMN failures integer NOT NULL DEFAULT 0 CHECK (failures >= 0);
