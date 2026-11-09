-- Agent Skills installed from GitHub, shared by every bot or kept to the
-- bot that installed them. Files are stored verbatim at the commit the
-- owner approved. A skill's keys live in the task secret skill-<id>.
CREATE TABLE skills (
    id TEXT NOT NULL PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 34 AND substr(id, 1, 1) GLOB '[a-z]' AND id NOT GLOB '*[^a-z0-9-]*'),
    name TEXT NOT NULL CHECK (trim(name) <> '' AND length(name) <= 120),
    description TEXT NOT NULL CHECK (length(description) <= 1024),
    source TEXT NOT NULL CHECK (json_valid(source)),
    scope TEXT NOT NULL CHECK (scope IN ('all', 'bot')),
    bot_id BLOB REFERENCES bots (id),
    env_names TEXT NOT NULL CHECK (json_valid(env_names)),
    installed_by BLOB NOT NULL REFERENCES bots (id),
    installed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    CHECK ((scope = 'bot') = (bot_id IS NOT NULL))
) STRICT;

CREATE TABLE skill_files (
    skill_id TEXT NOT NULL REFERENCES skills (id) ON DELETE CASCADE,
    path TEXT NOT NULL CHECK (length(path) BETWEEN 1 AND 255),
    content TEXT NOT NULL,
    PRIMARY KEY (skill_id, path)
) STRICT;

-- A bot's request to install a skill, answered by the owner in a card. The
-- fetched contents ride in the row so the owner approves exactly what
-- gets installed; files lists them without content for the card.
CREATE TABLE skill_installs (
    id BLOB NOT NULL PRIMARY KEY,
    bot_id BLOB NOT NULL REFERENCES bots (id),
    turn_id BLOB NOT NULL REFERENCES turns (id),
    attempt INTEGER NOT NULL,
    skill_id TEXT NOT NULL CHECK (length(skill_id) BETWEEN 1 AND 34 AND substr(skill_id, 1, 1) GLOB '[a-z]' AND skill_id NOT GLOB '*[^a-z0-9-]*'),
    name TEXT NOT NULL CHECK (trim(name) <> '' AND length(name) <= 120),
    description TEXT NOT NULL CHECK (length(description) <= 1024),
    reason TEXT NOT NULL CHECK (trim(reason) <> '' AND length(CAST(reason AS BLOB)) <= 500),
    source TEXT NOT NULL CHECK (json_valid(source)),
    files TEXT NOT NULL CHECK (json_valid(files)),
    contents TEXT NOT NULL CHECK (json_valid(contents)),
    env_names TEXT NOT NULL CHECK (json_valid(env_names)),
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'installed', 'declined', 'cancelled')),
    scope TEXT CHECK (scope IN ('all', 'bot')),
    key_stored INTEGER NOT NULL DEFAULT 0 CHECK (key_stored IN (0, 1)),
    delivered_attempt INTEGER,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')),
    settled_at TEXT,
    UNIQUE (turn_id, skill_id),
    CHECK ((status = 'installed') = (scope IS NOT NULL))
) STRICT;

CREATE INDEX skill_installs_turn ON skill_installs (turn_id);
CREATE UNIQUE INDEX skill_installs_pending ON skill_installs (turn_id) WHERE status = 'pending';

-- Whatever ends the turn withdraws a card still pending.
CREATE TRIGGER turns_end_skill_installs AFTER UPDATE OF status ON turns
WHEN OLD.status <> NEW.status AND NEW.status IN ('cancelling', 'completed', 'interrupted', 'failed', 'uncertain') BEGIN
    UPDATE skill_installs SET status = 'cancelled',
        settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
    WHERE turn_id = NEW.id AND status = 'pending';
END;

DROP VIEW turn_holds;

CREATE VIEW turn_holds AS
    SELECT root_turn_id AS turn_id, parent_attempt AS attempt,
           status IN ('queued', 'running') AS pending,
           delivered_attempt IS NULL AND status IN ('completed', 'failed', 'cancelled') AS unseen
    FROM delegations
    UNION ALL
    SELECT turn_id, attempt, status = 'pending', delivered_attempt IS NULL AND status <> 'pending'
    FROM secret_requests
    UNION ALL
    SELECT turn_id, attempt, status = 'pending', delivered_attempt IS NULL AND status <> 'pending'
    FROM owner_questions WHERE json_type(action, '$.permission') IS NULL
    UNION ALL
    SELECT turn_id, attempt, status = 'pending',
           delivered_attempt IS NULL AND status IN ('approved', 'discarded')
    FROM outgoing_drafts WHERE approval_source IS NOT 'message'
    UNION ALL
    SELECT turn_id, attempt, status = 'pending', delivered_attempt IS NULL AND status <> 'pending'
    FROM social_setups
    UNION ALL
    SELECT turn_id, attempt, status = 'pending',
           delivered_attempt IS NULL AND status IN ('finished', 'failed', 'expired')
    FROM mcp_waits
    UNION ALL
    SELECT turn_id, attempt, status = 'pending',
           delivered_attempt IS NULL AND status IN ('installed', 'declined')
    FROM skill_installs;
