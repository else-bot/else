-- no-transaction
-- WhatsApp joins Telegram and Discord. It links the owner's own account as a
-- device, so its credential is a data key that seals a device store the
-- WhatsApp client keeps updating, not a static bot token.
--
-- Widening the provider CHECKs needs SQLite's table rebuild, and seven tables
-- reference social_connections. Foreign keys can only be switched off outside
-- a transaction, so this migration runs its own. Every step converges: a run
-- interrupted after COMMIT, before sqlx records it, rebuilds the same tables
-- again.
PRAGMA foreign_keys = OFF;
BEGIN IMMEDIATE;
-- Triggers and the turn_holds view name these tables in their bodies; the
-- legacy rename keeps them pointing at the rebuilt tables.
PRAGMA legacy_alter_table = ON;

DROP TABLE IF EXISTS social_connections_new;
CREATE TABLE social_connections_new (
 id BLOB PRIMARY KEY NOT NULL, provider TEXT NOT NULL CHECK(provider IN ('telegram','discord','whatsapp')),
 remote_id TEXT NOT NULL, display_name TEXT NOT NULL, username TEXT,
 state TEXT NOT NULL DEFAULT 'active' CHECK(state IN ('active','paused','removed')),
 health TEXT NOT NULL DEFAULT 'connecting', generation INTEGER NOT NULL DEFAULT 1,
 instructions TEXT NOT NULL DEFAULT '' CHECK(length(CAST(instructions AS BLOB))<=8000),
 credential_version INTEGER NOT NULL DEFAULT 1, credential_salt BLOB, credential BLOB,
 poll_offset INTEGER NOT NULL DEFAULT 0,
 CHECK((state='removed')=(credential IS NULL)),
 CHECK((credential IS NULL)=(credential_salt IS NULL))
) STRICT;
INSERT INTO social_connections_new SELECT id,provider,remote_id,display_name,username,state,health,generation,instructions,credential_version,credential_salt,credential,poll_offset FROM social_connections;
DROP TABLE social_connections;
ALTER TABLE social_connections_new RENAME TO social_connections;
CREATE UNIQUE INDEX social_identity ON social_connections(provider,remote_id) WHERE state<>'removed';

-- The owner's words from the setup card come back to the bot with the result.
DROP TABLE IF EXISTS social_setups_new;
CREATE TABLE social_setups_new (
 id BLOB PRIMARY KEY NOT NULL, bot_id BLOB NOT NULL REFERENCES bots(id), turn_id BLOB NOT NULL REFERENCES turns(id),
 attempt INTEGER NOT NULL, provider TEXT NOT NULL CHECK(provider IN ('telegram','discord','whatsapp')), reason TEXT NOT NULL,
 status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN ('pending','connected','cancelled')),
 connection_id BLOB REFERENCES social_connections(id), delivered_attempt INTEGER,
 note TEXT CHECK(note IS NULL OR (status='connected' AND length(CAST(note AS BLOB)) BETWEEN 1 AND 4000)),
 UNIQUE(turn_id,provider)
) STRICT;
INSERT INTO social_setups_new(id,bot_id,turn_id,attempt,provider,reason,status,connection_id,delivered_attempt)
SELECT id,bot_id,turn_id,attempt,provider,reason,status,connection_id,delivered_attempt FROM social_setups;
DROP TABLE social_setups;
ALTER TABLE social_setups_new RENAME TO social_setups;

-- Creating a group has no existing chat, so an action may name none.
DROP TABLE IF EXISTS social_actions_new;
CREATE TABLE social_actions_new (
 connection_id BLOB NOT NULL REFERENCES social_connections(id), action_id BLOB NOT NULL,
 bot_id BLOB NOT NULL REFERENCES bots(id), turn_id BLOB NOT NULL REFERENCES turns(id),
 fingerprint BLOB NOT NULL, destination TEXT, status TEXT NOT NULL CHECK(status IN ('dispatching','succeeded','failed','uncertain')),
 result TEXT CHECK(result IS NULL OR json_valid(result)), PRIMARY KEY(connection_id,action_id)
) STRICT;
INSERT INTO social_actions_new SELECT connection_id,action_id,bot_id,turn_id,fingerprint,destination,status,result FROM social_actions;
DROP TABLE social_actions;
ALTER TABLE social_actions_new RENAME TO social_actions;
DROP TRIGGER IF EXISTS social_actions_settle_drafts;
CREATE TRIGGER social_actions_settle_drafts AFTER UPDATE OF status ON social_actions
 WHEN OLD.status<>NEW.status AND NEW.status<>'dispatching' BEGIN
 UPDATE outgoing_drafts SET status=CASE NEW.status WHEN 'succeeded' THEN 'sent' ELSE NEW.status END,
 result_detail=CASE NEW.status WHEN 'succeeded' THEN 'The connected social platform accepted the message.' WHEN 'failed' THEN 'The social connector refused delivery.' ELSE 'Delivery could not be confirmed. Check the destination before sending again.' END,
 updated_at=strftime('%Y-%m-%dT%H:%M:%f+00:00','now'), settled_at=strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
 WHERE status IN ('sending','uncertain') AND json_extract(delivery,'$.via')='social'
 AND replace(json_extract(delivery,'$.account_id'),'-','')=lower(hex(NEW.connection_id))
 AND replace(json_extract(delivery,'$.action_id'),'-','')=lower(hex(NEW.action_id));
END;

PRAGMA legacy_alter_table = OFF;

-- A linked device's key store. A pairing writes into a store with no
-- connection yet; success attaches it in the same transaction that seals its
-- data key into the connection. Stores never attached are deleted.
CREATE TABLE IF NOT EXISTS social_device_stores (
 id BLOB PRIMARY KEY NOT NULL,
 connection_id BLOB UNIQUE REFERENCES social_connections(id),
 created_at TEXT NOT NULL DEFAULT(strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
) STRICT;
-- One sealed value per (space, key). `stamp` is a plaintext time in seconds
-- for expiry sweeps; `flag` carries the few plaintext bits the client
-- filters on, such as an uploaded pre-key.
CREATE TABLE IF NOT EXISTS social_device_state (
 store_id BLOB NOT NULL REFERENCES social_device_stores(id) ON DELETE CASCADE,
 space TEXT NOT NULL, key TEXT NOT NULL,
 stamp INTEGER NOT NULL DEFAULT 0, flag INTEGER NOT NULL DEFAULT 0,
 value BLOB NOT NULL,
 PRIMARY KEY(store_id,space,key)
) STRICT, WITHOUT ROWID;

COMMIT;
PRAGMA foreign_keys = ON;
