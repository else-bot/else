-- The memory view revision a native session last received (POC-015). A
-- session gets bounded memory context once, when new or not yet given any;
-- it is not re-injected while unchanged and uses memory tools afterwards.
ALTER TABLE provider_sessions ADD COLUMN memory_revision bigint;
