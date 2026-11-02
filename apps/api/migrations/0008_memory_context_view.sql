-- The smaller saved compaction view (POC-014 review): summarization context
-- covering the same published prefix as the main view, with its own
-- hysteresis. It changes together with the main view (same revision).
ALTER TABLE memory_store
    ADD COLUMN context_nodes jsonb NOT NULL DEFAULT '[]',
    ADD COLUMN context_compacting boolean NOT NULL DEFAULT false;

-- Existing views start their compaction view as a copy (same coverage);
-- it compacts toward its own budget on the next publication.
UPDATE memory_store SET context_nodes = view_nodes WHERE published_seq >= 0;
