CREATE TABLE connector_accounts (
    id BLOB NOT NULL PRIMARY KEY,
    address text NOT NULL CHECK (address <> '' AND length(CAST(address AS BLOB)) <= 320),
    config TEXT NOT NULL CHECK (json_valid(config) AND length(CAST(config AS BLOB)) <= 8192),
    generation INTEGER NOT NULL DEFAULT 1 CHECK (generation >= 1),
    state text NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'paused', 'removed')),
    health text NOT NULL DEFAULT 'unknown'
        CHECK (health <> '' AND length(CAST(health AS BLOB)) <= 64),
    credential_version INTEGER NOT NULL DEFAULT 1 CHECK (credential_version >= 1),
    credential_salt BLOB CHECK (credential_salt IS NULL OR length(credential_salt) = 32),
    credential BLOB CHECK (credential IS NULL OR length(credential) <= 16384),
    cursor_uid_validity INTEGER CHECK (cursor_uid_validity BETWEEN 0 AND 4294967295),
    cursor_last_uid INTEGER CHECK (cursor_last_uid BETWEEN 0 AND 4294967295),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    removed_at TEXT,
    CHECK ((state = 'removed') = (removed_at IS NOT NULL)),
    CHECK ((state = 'removed') = (credential IS NULL)),
    CHECK ((credential IS NULL) = (credential_salt IS NULL)),
    CHECK ((cursor_uid_validity IS NULL) = (cursor_last_uid IS NULL))
) STRICT;

CREATE UNIQUE INDEX connector_accounts_address ON connector_accounts (lower(address))
    WHERE state <> 'removed';

CREATE TABLE connector_grants (
    account_id BLOB NOT NULL REFERENCES connector_accounts (id),
    bot_id BLOB NOT NULL REFERENCES bots (id),
    binding_id BLOB NOT NULL UNIQUE,
    can_read INTEGER NOT NULL CHECK (can_read IN (0, 1)),
    can_write INTEGER NOT NULL CHECK (can_write IN (0, 1)),
    can_send INTEGER NOT NULL CHECK (can_send IN (0, 1)),
    can_wake INTEGER NOT NULL CHECK (can_wake IN (0, 1)),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    PRIMARY KEY (account_id, bot_id),
    CHECK (can_read OR NOT (can_write OR can_send OR can_wake))
) STRICT;

CREATE INDEX connector_grants_bot ON connector_grants (bot_id);



CREATE TABLE connector_events (
    id BLOB NOT NULL PRIMARY KEY,
    account_id BLOB NOT NULL REFERENCES connector_accounts (id),
    folder text NOT NULL CHECK (folder <> '' AND length(CAST(folder AS BLOB)) <= 255),
    uid_validity INTEGER NOT NULL CHECK (uid_validity BETWEEN 0 AND 4294967295),
    uid INTEGER NOT NULL CHECK (uid BETWEEN 1 AND 4294967295),
    generation INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    UNIQUE (account_id, folder, uid_validity, uid)
) STRICT;

CREATE TABLE connector_deliveries (
    event_id BLOB NOT NULL REFERENCES connector_events (id),
    bot_id BLOB NOT NULL REFERENCES bots (id),
    account_id BLOB NOT NULL REFERENCES connector_accounts (id),
    binding_id BLOB NOT NULL,
    status text NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'delivered', 'revoked', 'cancelled')),
    turn_id BLOB REFERENCES turns (id),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    settled_at TEXT,
    PRIMARY KEY (event_id, bot_id),
    CHECK ((status = 'delivered') = (turn_id IS NOT NULL)),
    CHECK ((status = 'pending') = (settled_at IS NULL))
) STRICT;

CREATE INDEX connector_deliveries_pending ON connector_deliveries (bot_id, account_id)
    WHERE status = 'pending';
CREATE INDEX connector_deliveries_turn ON connector_deliveries (turn_id) WHERE turn_id IS NOT NULL;

CREATE TABLE connector_actions (
    account_id BLOB NOT NULL REFERENCES connector_accounts (id),
    action_id BLOB NOT NULL,
    bot_id BLOB NOT NULL REFERENCES bots (id),
    turn_id BLOB NOT NULL REFERENCES turns (id),
    generation INTEGER NOT NULL,
    operation text NOT NULL CHECK (operation <> '' AND length(operation) <= 32),
    fingerprint BLOB NOT NULL CHECK (length(fingerprint) = 32),
    status text NOT NULL DEFAULT 'dispatching'
        CHECK (status IN ('dispatching', 'succeeded', 'failed', 'uncertain')),
    result TEXT CHECK (result IS NULL OR (json_valid(result) AND length(CAST(result AS BLOB)) <= 262144)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    finished_at TEXT,
    PRIMARY KEY (account_id, action_id),
    CHECK ((status = 'dispatching') = (finished_at IS NULL))
) STRICT;

CREATE INDEX connector_actions_dispatching ON connector_actions (account_id) WHERE status = 'dispatching';

CREATE TRIGGER connector_bot_revoked AFTER UPDATE OF status ON bots
WHEN NEW.status <> 'active' BEGIN
    DELETE FROM connector_grants WHERE bot_id = NEW.id;
    UPDATE connector_deliveries SET status='revoked', settled_at=strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
    WHERE bot_id=NEW.id AND status='pending';
END;

-- SQLite requires a table rebuild to extend the item-kind CHECK constraint.
DROP TRIGGER turns_delegation_child;

CREATE TABLE items_new (
    id BLOB NOT NULL PRIMARY KEY,
    conversation_id BLOB NOT NULL REFERENCES conversations (id),
    seq INTEGER NOT NULL CHECK (seq >= 1),
    turn_id BLOB NOT NULL REFERENCES turns (id),
    kind text NOT NULL CHECK (kind IN ('user', 'assistant', 'status', 'routine', 'connector')),
    text text NOT NULL,
    provider_id text,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    reaction TEXT CHECK (reaction IS NULL OR (kind = 'user' AND json_valid(reaction))),
    parts TEXT CHECK (parts IS NULL OR (kind = 'user' AND json_valid(parts) AND json_type(parts) = 'array')),
    UNIQUE (conversation_id, seq)
) STRICT;

INSERT INTO items_new (id, conversation_id, seq, turn_id, kind, text, provider_id, created_at, reaction, parts)
SELECT id, conversation_id, seq, turn_id, kind, text, provider_id, created_at, reaction, parts FROM items;

DROP TABLE items;
ALTER TABLE items_new RENAME TO items;
CREATE INDEX items_turn ON items (turn_id);

CREATE TRIGGER turns_delegation_child AFTER UPDATE OF status ON turns
WHEN OLD.status <> NEW.status AND NEW.status IN ('completed', 'interrupted', 'failed', 'uncertain') BEGIN
    UPDATE delegations SET
        status = CASE NEW.status WHEN 'completed' THEN 'completed' WHEN 'interrupted' THEN 'cancelled' ELSE 'failed' END,
        result = (SELECT group_concat(text, char(10) || char(10)) FROM
                  (SELECT text FROM items WHERE turn_id = NEW.id AND kind = 'assistant' ORDER BY seq)),
        error = CASE WHEN NEW.status = 'completed' THEN NULL ELSE coalesce(NEW.error_code || ': ' || NEW.error_message, NEW.status) END,
        finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE child_turn_id = NEW.id AND status NOT IN ('completed', 'failed', 'cancelled');
    UPDATE turns SET status = 'queued', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE id IN (SELECT root_turn_id FROM delegations WHERE child_turn_id = NEW.id) AND status = 'waiting'
      AND NOT EXISTS (SELECT 1 FROM turn_holds h WHERE h.turn_id = turns.id AND h.pending);
END;
