-- Any bot may use a social connection for owner work; per-bot access is
-- gone. A bot wakes on incoming messages only through triggers it set up
-- for an owner request.
CREATE TABLE social_triggers (
 id BLOB PRIMARY KEY NOT NULL,
 connection_id BLOB NOT NULL REFERENCES social_connections(id),
 bot_id BLOB NOT NULL REFERENCES bots(id),
 destination TEXT CHECK(destination IS NULL OR length(destination) BETWEEN 1 AND 100),
 wake TEXT NOT NULL CHECK(wake IN ('mentions','all')),
 instructions TEXT NOT NULL CHECK(length(CAST(instructions AS BLOB)) BETWEEN 1 AND 8000),
 enabled INTEGER NOT NULL DEFAULT 1 CHECK(enabled IN (0,1)),
 created_at TEXT NOT NULL DEFAULT(strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
 updated_at TEXT NOT NULL DEFAULT(strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
) STRICT;
CREATE UNIQUE INDEX social_trigger_scope ON social_triggers(connection_id,bot_id,coalesce(destination,''));
CREATE INDEX social_triggers_bot ON social_triggers(bot_id);

INSERT INTO social_triggers(id,connection_id,bot_id,destination,wake,instructions)
SELECT randomblob(16),connection_id,bot_id,destination,wake,instructions FROM (
 SELECT DISTINCT b.connection_id,b.bot_id,d.value AS destination,
        json_extract(b.access,'$.wake_policy') AS wake,
        coalesce(nullif(trim(json_extract(b.access,'$.instructions')),''),'Handle new messages here as the owner asked.') AS instructions
 FROM social_bindings b JOIN json_each(b.access,'$.destinations') d
 JOIN social_connections c ON c.id=b.connection_id AND c.state<>'removed'
 WHERE json_extract(b.access,'$.permissions.read') AND json_extract(b.access,'$.permissions.wake')
   AND json_extract(b.access,'$.wake_policy') IN ('mentions','all'));

UPDATE social_deliveries SET status='revoked',settled_at=strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
WHERE status='pending';
ALTER TABLE social_deliveries RENAME COLUMN binding_id TO trigger_id;

DROP TRIGGER social_bot_revocation;
CREATE TRIGGER social_bot_revocation AFTER UPDATE OF status ON bots WHEN NEW.status<>'active' BEGIN
 DELETE FROM social_triggers WHERE bot_id=NEW.id;
 UPDATE social_deliveries SET status='revoked' WHERE bot_id=NEW.id AND status='pending';
END;
DROP TABLE social_bindings;
