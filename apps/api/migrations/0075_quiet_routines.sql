-- A quiet routine's bot may end a run with no news: the run stays in the
-- routine's history, and its replies are silent: never the bot's latest
-- reply, unread, or its newest message.
ALTER TABLE routines ADD COLUMN quiet INTEGER NOT NULL DEFAULT false CHECK (quiet IN (false, true));

-- 'acted': a gated action ran in the run, so it reports whatever the bot says.
ALTER TABLE routine_runs ADD COLUMN report TEXT NOT NULL DEFAULT 'news'
    CHECK (report IN ('news', 'no_news', 'acted'));

ALTER TABLE items ADD COLUMN silent INTEGER NOT NULL DEFAULT false
    CHECK (silent IN (false, true) AND (NOT silent OR kind = 'assistant'));

CREATE INDEX items_alerting_replies ON items (conversation_id, seq)
    WHERE kind = 'assistant' AND NOT silent;
CREATE INDEX items_alerting_messages ON items (conversation_id, seq)
    WHERE kind IN ('user', 'assistant') AND NOT hidden AND NOT silent;
CREATE INDEX social_actions_turn ON social_actions (turn_id);
CREATE INDEX connector_actions_turn ON connector_actions (turn_id);

-- Only the attempt that ends the run may declare no news: one that parks
-- may find news when it continues.
CREATE TRIGGER routine_runs_news_when_parked AFTER UPDATE OF status ON turns
WHEN OLD.status <> NEW.status AND NEW.status IN ('queued', 'waiting') BEGIN
    UPDATE routine_runs SET report = 'news' WHERE turn_id = NEW.id AND report = 'no_news';
END;

-- A run that involved the owner or acted outside the bot reports as news
-- however it ended. A run that completes with no news makes its replies
-- silent.
CREATE TRIGGER routine_runs_settle_report AFTER UPDATE OF status ON turns
WHEN OLD.status <> NEW.status AND NEW.status = 'completed' BEGIN
    UPDATE routine_runs SET report = 'news'
    WHERE turn_id = NEW.id AND report = 'no_news' AND (
        EXISTS (SELECT 1 FROM owner_questions p WHERE p.turn_id = NEW.id)
        OR EXISTS (SELECT 1 FROM computer_handoffs p WHERE p.turn_id = NEW.id)
        OR EXISTS (SELECT 1 FROM secret_requests p WHERE p.turn_id = NEW.id)
        OR EXISTS (SELECT 1 FROM skill_installs p WHERE p.turn_id = NEW.id)
        OR EXISTS (SELECT 1 FROM outgoing_drafts p WHERE p.turn_id = NEW.id)
        OR EXISTS (SELECT 1 FROM owner_notices p WHERE p.turn_id = NEW.id)
        OR EXISTS (SELECT 1 FROM social_setups p WHERE p.turn_id = NEW.id)
        OR EXISTS (SELECT 1 FROM social_actions p WHERE p.turn_id = NEW.id)
        OR EXISTS (SELECT 1 FROM connector_actions p WHERE p.turn_id = NEW.id)
        OR EXISTS (SELECT 1 FROM delegations p WHERE p.root_turn_id = NEW.id));
    UPDATE items SET silent = true
    WHERE turn_id = NEW.id AND kind = 'assistant'
      AND EXISTS (SELECT 1 FROM routine_runs r WHERE r.turn_id = NEW.id AND r.report = 'no_news');
END;
