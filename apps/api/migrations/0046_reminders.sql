-- A reminder is a routine that runs once, at run_at, instead of on a cron
-- schedule. Its cron column holds 'once'.
ALTER TABLE routines ADD COLUMN run_at TEXT CHECK ((run_at IS NOT NULL) = (cron = 'once'));
