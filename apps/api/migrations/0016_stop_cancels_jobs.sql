-- POC-019: bot-wide Stop work (and deletion) also cancels the bot's guest
-- jobs. They are marked `cancelling` in the stop's transaction; maintenance
-- kills them, and the stop completes only once none remains.
ALTER TABLE guest_jobs DROP CONSTRAINT guest_jobs_status_check;
ALTER TABLE guest_jobs ADD CONSTRAINT guest_jobs_status_check CHECK (status IN (
    'starting', 'running', 'cancelling', 'completed', 'failed', 'cancelled', 'lost'));
