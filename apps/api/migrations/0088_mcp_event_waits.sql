-- An MCP wait can also watch an MCP event (SEP-3415) that means its job
-- stopped; status calls remain the fallback. `event_status` is `watching`
-- while the watcher subscribes, `gap` once the server reported skipped
-- events and before a status call has seen the job running, and `dropped`
-- when the subscription could not start or the server refused it for good. `event_gap` names
-- the gap, so a status call that started before a newer gap cannot clear
-- it. `event_cursor` is where a new subscription resumes.
ALTER TABLE mcp_waits ADD COLUMN event TEXT
    CHECK (event IS NULL OR json_valid(event));
ALTER TABLE mcp_waits ADD COLUMN event_status TEXT
    CHECK ((event IS NULL AND event_status IS NULL)
           OR (event IS NOT NULL
               AND coalesce(event_status IN ('watching', 'gap', 'dropped'), 0)));
ALTER TABLE mcp_waits ADD COLUMN event_gap TEXT
    CHECK (coalesce(event_status = 'gap', 0) = (event_gap IS NOT NULL));
ALTER TABLE mcp_waits ADD COLUMN event_cursor TEXT;
