-- Attempts cut at the attempt limit whose turn continued in a new attempt.
-- Their count caps a request's continuations, and recent rows cap a bot's
-- continuations per day. The attempt that told the provider about the cut
-- is recorded once the provider took it.
CREATE TABLE turn_limit_cuts (
    turn_id BLOB NOT NULL REFERENCES turns (id),
    attempt INTEGER NOT NULL,
    delivered_attempt INTEGER,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    PRIMARY KEY (turn_id, attempt)
) STRICT;

CREATE INDEX turn_limit_cuts_created ON turn_limit_cuts (created_at);
