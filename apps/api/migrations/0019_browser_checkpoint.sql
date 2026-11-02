-- Evidence that a desktop computer's browser was checkpointed (closed
-- cleanly) when it was last stopped (POC-022): `ok`, `failed`, or NULL
-- (never checkpointed since it last ran). A backup captures a stopped
-- desktop without starting it, so it relies on this record.
ALTER TABLE bot_computers
    ADD COLUMN browser_checkpoint text CHECK (browser_checkpoint IN ('ok', 'failed'));
