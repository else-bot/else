-- An owner message sent while the conversation's reply is running joins
-- that reply instead of stopping it. Its turn stays `queued` but is not
-- claimed while it has a row here. `pending` means nothing was written to
-- the provider yet, `sending` that it was or may have been written but the
-- provider has not echoed it, and `delivered` that it echoed it into the
-- running turn. See `db::steers::settle` for how each one ends.
CREATE TABLE turn_steers (
    turn_id BLOB PRIMARY KEY NOT NULL REFERENCES turns (id),
    into_turn_id BLOB NOT NULL REFERENCES turns (id),
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'sending', 'delivered')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))
);

CREATE INDEX turn_steers_into ON turn_steers (into_turn_id);
