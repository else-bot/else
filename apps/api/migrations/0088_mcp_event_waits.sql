-- An MCP wait can also watch an MCP event (SEP-3415) that means its job
-- stopped. The status call stays the fallback. `event` is the subscription
-- and payload match, fixed at creation. `event_status` is `watching` while
-- the watcher subscribes, and `dropped` once the server refused the
-- subscription for good, after which only status calls check the job.
-- `event_cursor` is the last position the server reported, so a restart
-- resumes without missing an event.
ALTER TABLE mcp_waits ADD COLUMN event TEXT
    CHECK (event IS NULL OR json_valid(event));
ALTER TABLE mcp_waits ADD COLUMN event_status TEXT
    CHECK ((event IS NULL AND event_status IS NULL)
           OR (event IS NOT NULL AND coalesce(event_status IN ('watching', 'dropped'), 0)));
ALTER TABLE mcp_waits ADD COLUMN event_cursor TEXT;
