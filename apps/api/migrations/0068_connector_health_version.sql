-- Counts health writes, so a watch that scanned before a newer health write
-- cannot overwrite it with 'connected' (connectors::store::mark_connected).
ALTER TABLE connector_accounts ADD COLUMN health_version INTEGER NOT NULL DEFAULT 0
    CHECK (health_version >= 0);
