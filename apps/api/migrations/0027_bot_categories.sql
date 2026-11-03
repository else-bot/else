-- POC-030: flat, owner-managed sidebar categories. A category only groups
-- bots in the sidebar: it changes no identity, role, permission, memory,
-- provider profile, or computer. Names are trimmed, 1 to 80 characters,
-- and unique without regard to case. Existing bots start unassigned.
CREATE TABLE bot_categories (
    id uuid PRIMARY KEY,
    name text NOT NULL CHECK (name = btrim(name) AND char_length(name) BETWEEN 1 AND 80),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX bot_categories_name ON bot_categories (lower(name));

-- A retried create returns the category it made (or says it was deleted
-- since); the receipt outlives the category.
CREATE TABLE category_requests (
    request_id uuid PRIMARY KEY,
    category_id uuid NOT NULL,
    payload_hash bytea NOT NULL CHECK (octet_length(payload_hash) = 32),
    created_at timestamptz NOT NULL DEFAULT now()
);

ALTER TABLE bots ADD COLUMN category_id uuid REFERENCES bot_categories (id);
CREATE INDEX bots_category ON bots (category_id) WHERE category_id IS NOT NULL;
