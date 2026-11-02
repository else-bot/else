-- Remote state belongs to one destination (endpoint, bucket, and prefix):
-- a set verified in one bucket is not taken as replicated to another.
-- Rows from before carry an empty destination, so they are verified again.
-- `whole_assistant` lets remote retention keep the newest whole set.
ALTER TABLE backup_remote ADD COLUMN destination text NOT NULL DEFAULT '';
ALTER TABLE backup_remote ADD COLUMN whole_assistant boolean NOT NULL DEFAULT false;
ALTER TABLE backup_remote DROP CONSTRAINT backup_remote_pkey;
ALTER TABLE backup_remote ADD PRIMARY KEY (destination, set_id);
-- A set removed from the bucket by retention is `retired` there: it is not
-- uploaded again.
ALTER TABLE backup_remote DROP CONSTRAINT backup_remote_state_check;
ALTER TABLE backup_remote ADD CONSTRAINT backup_remote_state_check
    CHECK (state IN ('completed', 'failed', 'retired'));
