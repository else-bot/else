CREATE TABLE social_connections (
 id BLOB PRIMARY KEY NOT NULL, provider TEXT NOT NULL CHECK(provider IN ('telegram','discord')),
 remote_id TEXT NOT NULL, display_name TEXT NOT NULL, username TEXT,
 state TEXT NOT NULL DEFAULT 'active' CHECK(state IN ('active','paused','removed')),
 health TEXT NOT NULL DEFAULT 'connecting', generation INTEGER NOT NULL DEFAULT 1,
 instructions TEXT NOT NULL DEFAULT '' CHECK(length(CAST(instructions AS BLOB))<=8000),
 credential_version INTEGER NOT NULL DEFAULT 1, credential_salt BLOB, credential BLOB,
 poll_offset INTEGER NOT NULL DEFAULT 0,
 CHECK((state='removed')=(credential IS NULL)),
 CHECK((credential IS NULL)=(credential_salt IS NULL))
) STRICT;
CREATE UNIQUE INDEX social_identity ON social_connections(provider,remote_id) WHERE state<>'removed';
CREATE TABLE social_bindings (
 id BLOB NOT NULL UNIQUE, connection_id BLOB NOT NULL REFERENCES social_connections(id),
 bot_id BLOB NOT NULL REFERENCES bots(id), access TEXT NOT NULL CHECK(json_valid(access)),
 PRIMARY KEY(connection_id,bot_id)
) STRICT;
CREATE TABLE social_destinations (
 connection_id BLOB NOT NULL REFERENCES social_connections(id), id TEXT NOT NULL,
 info TEXT NOT NULL CHECK(json_valid(info)), PRIMARY KEY(connection_id,id)
) STRICT;
CREATE TABLE social_messages (
 connection_id BLOB NOT NULL REFERENCES social_connections(id), destination TEXT NOT NULL,
 message_id TEXT NOT NULL, author_id TEXT NOT NULL, body TEXT NOT NULL,
 event_id TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT(strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
 PRIMARY KEY(connection_id,destination,message_id), UNIQUE(connection_id,event_id)
) STRICT;
CREATE TABLE social_actions (
 connection_id BLOB NOT NULL REFERENCES social_connections(id), action_id BLOB NOT NULL,
 bot_id BLOB NOT NULL REFERENCES bots(id), turn_id BLOB NOT NULL REFERENCES turns(id),
 fingerprint BLOB NOT NULL, destination TEXT NOT NULL, status TEXT NOT NULL CHECK(status IN ('dispatching','succeeded','failed','uncertain')),
 result TEXT CHECK(result IS NULL OR json_valid(result)), PRIMARY KEY(connection_id,action_id)
) STRICT;
CREATE TABLE social_setups (
 id BLOB PRIMARY KEY NOT NULL, bot_id BLOB NOT NULL REFERENCES bots(id), turn_id BLOB NOT NULL REFERENCES turns(id),
 attempt INTEGER NOT NULL, provider TEXT NOT NULL CHECK(provider IN ('telegram','discord')), reason TEXT NOT NULL,
 status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN ('pending','connected','cancelled')),
 connection_id BLOB REFERENCES social_connections(id), delivered_attempt INTEGER,
 UNIQUE(turn_id,provider)
) STRICT;
CREATE TABLE social_deliveries (
 connection_id BLOB NOT NULL REFERENCES social_connections(id), event_id TEXT NOT NULL,
 bot_id BLOB NOT NULL REFERENCES bots(id), binding_id BLOB NOT NULL,
 destination TEXT NOT NULL, message_id TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'pending',
 turn_id BLOB REFERENCES turns(id), created_at TEXT NOT NULL DEFAULT(strftime('%Y-%m-%dT%H:%M:%f+00:00','now')), settled_at TEXT, PRIMARY KEY(connection_id,event_id,bot_id)
) STRICT;
DROP VIEW turn_holds;
CREATE VIEW turn_holds AS
 SELECT root_turn_id AS turn_id,parent_attempt AS attempt,status IN ('queued','running') AS pending,
 delivered_attempt IS NULL AND status IN ('completed','failed','cancelled') AS unseen FROM delegations
 UNION ALL SELECT turn_id,attempt,status='pending',delivered_attempt IS NULL AND status<>'pending' FROM secret_requests
 UNION ALL SELECT turn_id,attempt,status='pending',delivered_attempt IS NULL AND status<>'pending' FROM owner_questions
 UNION ALL SELECT turn_id,attempt,status='pending',delivered_attempt IS NULL AND status IN ('approved','discarded') FROM outgoing_drafts WHERE approval_source IS NOT 'message'
 UNION ALL SELECT turn_id,attempt,status='pending',delivered_attempt IS NULL AND status<>'pending' FROM social_setups;
CREATE TRIGGER turns_end_social_setups AFTER UPDATE OF status ON turns
 WHEN NEW.status IN ('cancelling','completed','interrupted','failed','uncertain') BEGIN
 UPDATE social_setups SET status='cancelled' WHERE turn_id=NEW.id AND status='pending';
END;
CREATE TRIGGER social_bot_revocation AFTER UPDATE OF status ON bots WHEN NEW.status<>'active' BEGIN
 DELETE FROM social_bindings WHERE bot_id=NEW.id;
 UPDATE social_deliveries SET status='revoked' WHERE bot_id=NEW.id AND status='pending';
END;
CREATE TRIGGER social_actions_settle_drafts AFTER UPDATE OF status ON social_actions
 WHEN OLD.status<>NEW.status AND NEW.status<>'dispatching' BEGIN
 UPDATE outgoing_drafts SET status=CASE NEW.status WHEN 'succeeded' THEN 'sent' ELSE NEW.status END,
 result_detail=CASE NEW.status WHEN 'succeeded' THEN 'The connected social platform accepted the message.' WHEN 'failed' THEN 'The social connector refused delivery.' ELSE 'Delivery could not be confirmed. Check the destination before sending again.' END,
 updated_at=strftime('%Y-%m-%dT%H:%M:%f+00:00','now'), settled_at=strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
 WHERE status IN ('sending','uncertain') AND json_extract(delivery,'$.via')='social'
 AND replace(json_extract(delivery,'$.account_id'),'-','')=lower(hex(NEW.connection_id))
 AND replace(json_extract(delivery,'$.action_id'),'-','')=lower(hex(NEW.action_id));
END;

CREATE TABLE social_sent_messages (connection_id BLOB NOT NULL REFERENCES social_connections(id),destination TEXT NOT NULL,message_id TEXT NOT NULL,PRIMARY KEY(connection_id,destination,message_id)) STRICT;
